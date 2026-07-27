//! Tracing layer that forwards daemon log events to the telemetry worker.

use crate::api::types::{DaemonLogEvent, DaemonLogFieldValue, DaemonLogKind, DaemonLogLevel};
use crate::authorship::secrets::redact_secrets_in_text;
use crate::config::Config;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const MAX_MESSAGE_LENGTH: usize = 16_000;
const MAX_TARGET_LENGTH: usize = 512;
const MAX_FIELD_KEY_LENGTH: usize = 256;
const MAX_FIELD_VALUE_LENGTH: usize = 4096;
const MAX_FIELDS_PER_EVENT: usize = 64;
const MAX_SECRET_SCAN_TOKEN_LENGTH: usize = 90;
const DAEMON_LOG_CAPTURE_ELIGIBILITY_TTL: Duration = Duration::from_secs(30);

/// Captures tracing events for best-effort daemon diagnostics upload.
pub struct DaemonLogUploadLayer;

struct CaptureEligibilityCache {
    checked_at: Instant,
    enabled: bool,
}

struct DaemonLogVisitor {
    message: String,
    fields: BTreeMap<String, DaemonLogFieldValue>,
}

impl DaemonLogVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: BTreeMap::new(),
        }
    }

    fn record_string(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = sanitize_log_string(&value, MAX_MESSAGE_LENGTH);
            return;
        }

        self.record_field_value(field, DaemonLogFieldValue::String(value));
    }

    fn record_field_value(&mut self, field: &Field, value: DaemonLogFieldValue) {
        self.record_named_field_value(field.name(), value);
    }

    fn record_named_field_value(&mut self, name: &str, value: DaemonLogFieldValue) {
        let key = truncate_string(name, MAX_FIELD_KEY_LENGTH);
        if !self.fields.contains_key(&key) && self.fields.len() >= MAX_FIELDS_PER_EVENT {
            return;
        }
        let value = sanitize_field_value(value);
        self.fields.insert(key, value);
    }
}

impl Visit for DaemonLogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let max_len = max_string_len_for_field(field);
        self.record_string(field, bounded_debug_string(value, max_len));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let max_len = max_string_len_for_field(field);
        self.record_string(field, bounded_copy(value, max_len));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_field_value(field, DaemonLogFieldValue::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_field_value(field, DaemonLogFieldValue::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_field_value(field, DaemonLogFieldValue::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let max_len = max_string_len_for_field(field);
        self.record_string(field, bounded_display_string(value, max_len));
    }
}

impl<S: Subscriber> Layer<S> for DaemonLogUploadLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !daemon_log_capture_enabled() {
            return;
        }

        let metadata = event.metadata();
        let mut visitor = DaemonLogVisitor::new();
        event.record(&mut visitor);
        if !daemon_log_target_is_captureable(metadata.target(), &visitor.fields) {
            return;
        }

        if let Some(file) = metadata.file() {
            visitor.record_named_field_value("file", DaemonLogFieldValue::from(file));
        }
        if let Some(line) = metadata.line() {
            visitor.record_named_field_value("line", DaemonLogFieldValue::from(u64::from(line)));
        }
        if let Some(module_path) = metadata.module_path() {
            visitor.record_named_field_value("module_path", DaemonLogFieldValue::from(module_path));
        }

        let log_event = DaemonLogEvent {
            id: Some(crate::uuid::generate_v4()),
            kind: DaemonLogKind::Log,
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: daemon_log_level_from_tracing(metadata.level()),
            target: Some(sanitize_log_string(metadata.target(), MAX_TARGET_LENGTH)),
            message: visitor.message,
            fields: visitor.fields,
            repo_url: None,
            git_ai_version: None,
        };

        crate::daemon::telemetry_worker::submit_daemon_internal_daemon_logs(vec![log_event]);
    }
}

fn daemon_log_target_is_captureable(
    metadata_target: &str,
    fields: &BTreeMap<String, DaemonLogFieldValue>,
) -> bool {
    // tracing-log bridges log records under the `log` metadata target and keeps
    // the originating crate target in this field.
    let target = match fields.get("log.target") {
        Some(DaemonLogFieldValue::String(target)) => target.as_str(),
        _ => metadata_target,
    };
    target != "ureq" && !target.starts_with("ureq::")
}

fn daemon_log_capture_enabled() -> bool {
    static CACHE: OnceLock<Mutex<Option<CaptureEligibilityCache>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let now = Instant::now();
    if let Ok(mut guard) = cache.lock() {
        if let Some(cached) = guard.as_ref()
            && now.duration_since(cached.checked_at) < DAEMON_LOG_CAPTURE_ELIGIBILITY_TTL
        {
            return cached.enabled;
        }

        let enabled = compute_daemon_log_capture_enabled();
        *guard = Some(CaptureEligibilityCache {
            checked_at: now,
            enabled,
        });
        return enabled;
    }

    compute_daemon_log_capture_enabled()
}

fn compute_daemon_log_capture_enabled() -> bool {
    let config = Config::fresh();
    let flags = config.get_feature_flags();
    if !flags.daemon_log_upload {
        return false;
    }

    daemon_log_capture_allowed_for_config(
        config.api_base_url(),
        config.api_key().is_some(),
        has_unexpired_auth_credentials(),
    )
}

fn daemon_log_capture_allowed_for_config(
    api_base_url: &str,
    has_api_key: bool,
    has_unexpired_auth: bool,
) -> bool {
    api_base_url != crate::config::DEFAULT_API_BASE_URL || has_api_key || has_unexpired_auth
}

fn has_unexpired_auth_credentials() -> bool {
    crate::auth::CredentialStore::new()
        .load()
        .ok()
        .flatten()
        .is_some_and(|credentials| !credentials.is_refresh_token_expired())
}

fn daemon_log_level_from_tracing(level: &Level) -> DaemonLogLevel {
    match *level {
        Level::TRACE => DaemonLogLevel::Trace,
        Level::DEBUG => DaemonLogLevel::Debug,
        Level::INFO => DaemonLogLevel::Info,
        Level::WARN => DaemonLogLevel::Warn,
        Level::ERROR => DaemonLogLevel::Error,
    }
}

fn sanitize_field_value(value: DaemonLogFieldValue) -> DaemonLogFieldValue {
    match value {
        DaemonLogFieldValue::String(raw) => {
            DaemonLogFieldValue::String(sanitize_log_string(&raw, MAX_FIELD_VALUE_LENGTH))
        }
        other => other,
    }
}

fn sanitize_log_string(value: &str, max_len: usize) -> String {
    let (redacted, _) = redact_secrets_in_text(value);
    truncate_string(&redacted, max_len)
}

fn max_string_len_for_field(field: &Field) -> usize {
    if field.name() == "message" {
        MAX_MESSAGE_LENGTH
    } else {
        MAX_FIELD_VALUE_LENGTH
    }
}

fn bounded_raw_len(max_len: usize) -> usize {
    max_len.saturating_add(MAX_SECRET_SCAN_TOKEN_LENGTH)
}

fn bounded_copy(value: &str, max_len: usize) -> String {
    truncate_string(value, bounded_raw_len(max_len))
}

fn bounded_debug_string(value: &dyn std::fmt::Debug, max_len: usize) -> String {
    let mut writer = BoundedStringWriter::new(bounded_raw_len(max_len));
    let _ = write!(&mut writer, "{value:?}");
    writer.into_string()
}

fn bounded_display_string(value: &dyn std::fmt::Display, max_len: usize) -> String {
    let mut writer = BoundedStringWriter::new(bounded_raw_len(max_len));
    let _ = write!(&mut writer, "{value}");
    writer.into_string()
}

fn truncate_string(value: &str, max_len: usize) -> String {
    if max_len == 0 {
        return String::new();
    }

    let mut count = 0;
    for (index, _) in value.char_indices() {
        count += 1;
        if count > max_len {
            return value[..index].to_string();
        }
    }

    value.to_string()
}

struct BoundedStringWriter {
    value: String,
    max_chars: usize,
    chars: usize,
}

impl BoundedStringWriter {
    fn new(max_chars: usize) -> Self {
        Self {
            value: String::new(),
            max_chars,
            chars: 0,
        }
    }

    fn into_string(self) -> String {
        self.value
    }
}

impl std::fmt::Write for BoundedStringWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let remaining = self.max_chars.saturating_sub(self.chars);
        if remaining == 0 {
            return Err(std::fmt::Error);
        }

        let mut written = 0;
        let mut chars = s.chars();
        for ch in chars.by_ref().take(remaining) {
            self.value.push(ch);
            written += 1;
        }
        self.chars += written;

        if chars.next().is_some() || self.chars >= self.max_chars {
            return Err(std::fmt::Error);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn sanitize_log_string_redacts_and_truncates() {
        let secret = "sk_test_4eC39HqLyjWDarjtT1zdp7dc";
        let value = format!("token={secret}");

        let sanitized = sanitize_log_string(&value, 12);

        assert_eq!(sanitized.chars().count(), 12);
        assert!(!sanitized.contains(secret));
    }

    #[test]
    fn visitor_collects_primitive_fields() {
        let mut visitor = DaemonLogVisitor::new();
        visitor.record_named_field_value("repo", DaemonLogFieldValue::from("example"));
        visitor.record_named_field_value("count", DaemonLogFieldValue::from(3_u64));
        visitor.record_named_field_value("ok", DaemonLogFieldValue::from(true));

        assert!(visitor.fields.contains_key("repo"));
        assert_eq!(
            visitor.fields.get("count"),
            Some(&DaemonLogFieldValue::from(3_u64))
        );
        assert_eq!(
            visitor.fields.get("ok"),
            Some(&DaemonLogFieldValue::from(true))
        );
    }

    #[test]
    fn visitor_caps_field_count() {
        let mut visitor = DaemonLogVisitor::new();

        for index in 0..=MAX_FIELDS_PER_EVENT {
            visitor.record_named_field_value(
                &format!("field-{index}"),
                DaemonLogFieldValue::from(index as u64),
            );
        }

        assert_eq!(visitor.fields.len(), MAX_FIELDS_PER_EVENT);
        assert!(visitor.fields.contains_key("field-0"));
        assert!(visitor.fields.contains_key("field-63"));
        assert!(!visitor.fields.contains_key("field-64"));
    }

    #[test]
    fn bounded_debug_string_stops_formatting_after_cap() {
        struct NoisyDebug<'a> {
            writes: &'a AtomicUsize,
        }

        impl std::fmt::Debug for NoisyDebug<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for index in 0..10_000 {
                    self.writes.fetch_add(1, Ordering::SeqCst);
                    write!(f, "field-{index}-")?;
                }
                Ok(())
            }
        }

        let writes = AtomicUsize::new(0);
        let formatted = bounded_debug_string(&NoisyDebug { writes: &writes }, 16);

        assert_eq!(
            formatted.chars().count(),
            bounded_raw_len(16),
            "bounded formatting should only keep cap plus redaction slack"
        );
        assert!(
            writes.load(Ordering::SeqCst) < 100,
            "formatter should stop invoking Debug once the bounded writer is full"
        );
    }

    #[test]
    fn bounded_string_fields_are_redacted_and_truncated() {
        let mut visitor = DaemonLogVisitor::new();
        let secret = "sk_test_4eC39HqLyjWDarjtT1zdp7dc";
        visitor.record_named_field_value(
            "token",
            DaemonLogFieldValue::String(format!("token={secret}")),
        );

        let value = visitor.fields.get("token").unwrap();
        let DaemonLogFieldValue::String(value) = value else {
            panic!("expected string field");
        };
        assert!(!value.contains(secret));
        assert!(value.chars().count() <= MAX_FIELD_VALUE_LENGTH);
    }

    #[test]
    fn daemon_log_capture_skips_default_api_without_auth_or_key() {
        assert!(!daemon_log_capture_allowed_for_config(
            crate::config::DEFAULT_API_BASE_URL,
            false,
            false,
        ));
        assert!(daemon_log_capture_allowed_for_config(
            crate::config::DEFAULT_API_BASE_URL,
            true,
            false,
        ));
        assert!(daemon_log_capture_allowed_for_config(
            "https://api.example.com",
            false,
            false,
        ));
    }

    #[test]
    fn daemon_log_capture_skips_ureq_targets() {
        let fields = BTreeMap::new();
        assert!(!daemon_log_target_is_captureable("ureq", &fields));
        assert!(!daemon_log_target_is_captureable("ureq::unit", &fields));
        assert!(daemon_log_target_is_captureable("git_ai::daemon", &fields));
        assert!(daemon_log_target_is_captureable("urequest", &fields));

        let fields = BTreeMap::from([(
            "log.target".to_string(),
            DaemonLogFieldValue::from("ureq::unit"),
        )]);
        assert!(!daemon_log_target_is_captureable("log", &fields));
    }
}
