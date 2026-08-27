//! Claude Code transcript usage extraction.
//!
//! Ported from ccusage's Claude adapter (rust/adapters/claude/src/lib.rs in
//! <https://github.com/ccusage/ccusage>, MIT License, Copyright (c) 2025
//! @ryoppippi), adapted to git-ai's incremental line-oriented streaming.
//!
//! Deviations from ccusage:
//! - Deduplication and the replacement policy run in the token-usage database
//!   (entries stream in across runs), via [`should_replace_entry`] and the
//!   `message_id` fallback; ccusage dedups in memory over whole files.
//! - Fast-speed entries keep their base model name (no "-fast" suffix);
//!   `usage.speed` carries the fast pricing multiplier (see `cost.rs`) and
//!   remains the replacement tie-breaker.
//! - Entries whose model is missing or `<synthetic>` are attributed to
//!   [`UNKNOWN_MODEL`] instead of carrying no model, so tokens aren't lost.
//! - Sidechain (subagent) usage is attributed to the sidechain's own session
//!   with the parent as a relationship (leaf attribution, standardized
//!   across tools); ccusage rolls it into the parent session. Cross-file
//!   dedup of replayed parent messages is unaffected — it is global.

use serde::Deserialize;

use super::extractor::UsageExtractor;
use super::types::{PricingShape, Speed, TokenCounts, UsageEntry, parse_rfc3339_secs};

/// Model recorded for entries with no usable model name.
pub const UNKNOWN_MODEL: &str = "unknown";

/// Substring present on every JSONL line that carries token usage.
const USAGE_MARKER: &str = r#""usage":{"#;

pub struct ClaudeUsageExtractor;

impl UsageExtractor for ClaudeUsageExtractor {
    fn wants_line(&self, line: &str) -> bool {
        line.contains(USAGE_MARKER)
    }

    fn extract_line(&mut self, line: &str) -> Vec<UsageEntry> {
        extract_claude_line(line)
    }
}

/// The fields the replacement policy compares. Built from a fresh
/// [`UsageEntry`] or from a stored database row.
#[derive(Debug, Clone, Copy)]
pub struct ReplacementCandidate {
    pub is_sidechain: bool,
    pub token_total: u64,
    pub has_speed: bool,
}

impl From<&UsageEntry> for ReplacementCandidate {
    fn from(entry: &UsageEntry) -> Self {
        Self {
            is_sidechain: entry.is_sidechain,
            token_total: entry.dedupe_token_total(),
            has_speed: entry.speed.is_some(),
        }
    }
}

/// Replacement policy for two entries sharing a dedup identity (ccusage
/// `should_replace_deduped_entry`): non-sidechain beats sidechain, then the
/// larger token total wins, then a `usage.speed` marker breaks the tie.
pub fn should_replace(candidate: ReplacementCandidate, existing: ReplacementCandidate) -> bool {
    if candidate.is_sidechain != existing.is_sidechain {
        return existing.is_sidechain;
    }
    if candidate.token_total != existing.token_total {
        return candidate.token_total > existing.token_total;
    }
    candidate.has_speed && !existing.has_speed
}

/// [`should_replace`] over full entries.
pub fn should_replace_entry(candidate: &UsageEntry, existing: &UsageEntry) -> bool {
    should_replace(candidate.into(), existing.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    timestamp: String,
    version: Option<String>,
    session_id: Option<String>,
    message: RawMessage,
    #[serde(rename = "costUSD")]
    cost_usd: Option<f64>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
}

#[derive(Deserialize)]
struct RawMessage {
    usage: RawUsage,
    model: Option<String>,
    id: Option<String>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct RawUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    /// ccusage `Speed`: unknown values fail the line's parse, matching
    /// ccusage's strict schema.
    #[serde(default)]
    speed: Option<Speed>,
    #[serde(default)]
    cache_creation: Option<RawCacheCreation>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct RawCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

impl RawUsage {
    /// ccusage `cache_creation_token_count`: the duration breakdown wins over
    /// the flat count when present.
    fn cache_creation_tokens(&self) -> u64 {
        match &self.cache_creation {
            Some(b) => b
                .ephemeral_5m_input_tokens
                .saturating_add(b.ephemeral_1h_input_tokens),
            None => self.cache_creation_input_tokens,
        }
    }
}

fn extract_claude_line(line: &str) -> Vec<UsageEntry> {
    if has_unsupported_null_field(line.as_bytes()) {
        return Vec::new();
    }
    let Ok(raw) = serde_json::from_str::<RawEntry>(line) else {
        return Vec::new();
    };
    let Some(ts) = parse_rfc3339_secs(&raw.timestamp) else {
        return Vec::new();
    };
    if !is_valid_usage_entry(&raw) {
        return Vec::new();
    }

    let is_sidechain = raw.is_sidechain == Some(true);
    let mut entries = vec![usage_entry(
        raw.message.id.clone(),
        raw.request_id.as_deref(),
        line,
        "",
        ts,
        resolve_model(raw.message.model.as_deref()),
        &raw.message.usage,
        raw.cost_usd,
        is_sidechain,
    )];
    for (index, advisor) in advisor_usages_from_line(line).into_iter().enumerate() {
        // Advisor sub-entries get synthetic ids so they never dedupe against
        // their parent, and their cost is always computed from the advisor
        // model (the parent costUSD does not cover them).
        let message_id = raw
            .message
            .id
            .as_ref()
            .map(|id| format!("{id}:advisor:{index}"));
        entries.push(usage_entry(
            message_id,
            raw.request_id.as_deref(),
            line,
            &format!(":advisor:{index}"),
            ts,
            advisor.model,
            &advisor.usage,
            None,
            is_sidechain,
        ));
    }
    entries
}

#[allow(clippy::too_many_arguments)]
fn usage_entry(
    message_id: Option<String>,
    request_id: Option<&str>,
    line: &str,
    key_salt: &str,
    ts: u32,
    model: String,
    usage: &RawUsage,
    cost_usd: Option<f64>,
    is_sidechain: bool,
) -> UsageEntry {
    let cache_write = usage.cache_creation_tokens();
    let tokens = TokenCounts {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write,
        reasoning_output: None,
        total: usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .saturating_add(usage.cache_read_input_tokens)
            .saturating_add(cache_write),
    };
    UsageEntry {
        entry_key: entry_key(message_id.as_deref(), request_id, line, key_salt),
        message_id,
        ts,
        model,
        tokens,
        cache_write_1h: usage
            .cache_creation
            .map_or(0, |b| b.ephemeral_1h_input_tokens),
        // A negative or non-finite costUSD is corruption, not a price:
        // treat it as absent so catalog pricing applies instead of a
        // suppressing Some(0).
        transcript_cost_micro_usd: cost_usd
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(super::cost::micro_usd),
        is_sidechain,
        speed: usage.speed,
        // The wire flag means "tier not recorded in the transcript" — an
        // unmarked Claude entry lands in the standard bucket by default,
        // exactly like an unmarked Codex entry, and must report the same.
        speed_inferred: usage.speed.is_none(),
        pricing_shape: PricingShape::Claude,
    }
}

/// Dedup key: `message_id|request_id` when the message has an id (the exact
/// key ccusage hashes), otherwise a content hash so re-reads stay idempotent.
/// `key_salt` disambiguates advisor sub-entries whose parent has no message
/// id (they share the parent's line and would otherwise collide).
fn entry_key(
    message_id: Option<&str>,
    request_id: Option<&str>,
    line: &str,
    key_salt: &str,
) -> String {
    match message_id {
        Some(id) => format!("{id}|{}", request_id.unwrap_or_default()),
        None => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(line.as_bytes());
            hasher.update(key_salt.as_bytes());
            format!("line:{:x}", hasher.finalize())[..21].to_string()
        }
    }
}

fn resolve_model(model: Option<&str>) -> String {
    match model {
        Some("<synthetic>") | None => UNKNOWN_MODEL.to_string(),
        Some(model) => model.to_string(),
    }
}

/// ccusage `is_valid_usage_entry`: fields that are present must be non-empty,
/// and a present version must look like semver.
fn is_valid_usage_entry(raw: &RawEntry) -> bool {
    if raw
        .version
        .as_deref()
        .is_some_and(|version| !is_semver_prefix(version))
    {
        return false;
    }
    let empty = |value: &Option<String>| value.as_deref().is_some_and(str::is_empty);
    !(empty(&raw.session_id)
        || empty(&raw.request_id)
        || empty(&raw.message.id)
        || empty(&raw.message.model))
}

fn is_semver_prefix(value: &str) -> bool {
    let mut parts = value.splitn(3, '.');
    let leading_digits = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().next().is_some_and(|b| b.is_ascii_digit()))
    };
    let all_digits = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    all_digits(parts.next()) && all_digits(parts.next()) && leading_digits(parts.next())
}

/// ccusage `has_unsupported_null_field`: schema fields that must never be
/// `null`. Serde would silently accept `"requestId":null` as absent, changing
/// dedup semantics, so these lines are rejected before parsing.
fn has_unsupported_null_field(line: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(relative) = find_bytes(&line[offset..], b":null") {
        let null_index = offset + relative;
        let mut field_end = null_index.saturating_sub(1);
        if line.get(field_end) != Some(&b'"') {
            while field_end > 0 && line[field_end] != b'"' {
                field_end -= 1;
            }
        }
        if line.get(field_end) == Some(&b'"') {
            let mut field_start = field_end.saturating_sub(1);
            while field_start > 0 && line[field_start] != b'"' {
                field_start -= 1;
            }
            // field_start < field_end guards the slice: a line starting with
            // `":null` puts both cursors at 0, and a reversed range panics.
            if field_start < field_end
                && line.get(field_start) == Some(&b'"')
                && is_unsupported_nullable_field(&line[field_start + 1..field_end])
            {
                return true;
            }
        }
        offset = null_index + b":null".len();
    }
    false
}

fn is_unsupported_nullable_field(field: &[u8]) -> bool {
    matches!(
        field,
        b"id"
            | b"cwd"
            | b"model"
            | b"speed"
            | b"costUSD"
            | b"version"
            | b"sessionId"
            | b"requestId"
            | b"isApiErrorMessage"
            | b"cache_read_input_tokens"
            | b"cache_creation_input_tokens"
    )
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Deserialize)]
struct AdvisorEnvelope {
    message: AdvisorMessage,
}

#[derive(Deserialize)]
struct AdvisorMessage {
    usage: AdvisorUsageList,
}

#[derive(Deserialize)]
struct AdvisorUsageList {
    #[serde(default)]
    iterations: Vec<AdvisorIteration>,
}

#[derive(Deserialize)]
struct AdvisorIteration {
    #[serde(rename = "type")]
    kind: String,
    model: Option<String>,
    #[serde(flatten)]
    usage: RawUsage,
}

struct AdvisorUsage {
    model: String,
    usage: RawUsage,
}

/// Advisor iterations embedded in a usage entry (ccusage
/// `advisor_usages_from_line`): each becomes its own entry under the advisor
/// model.
fn advisor_usages_from_line(line: &str) -> Vec<AdvisorUsage> {
    if !line.contains(r#""advisor_message""#) {
        return Vec::new();
    }
    let Ok(envelope) = serde_json::from_str::<AdvisorEnvelope>(line) else {
        return Vec::new();
    };
    envelope
        .message
        .usage
        .iterations
        .into_iter()
        .filter_map(|iteration| {
            (iteration.kind == "advisor_message")
                .then_some(iteration.model)
                .flatten()
                .filter(|model| !model.is_empty())
                .map(|model| AdvisorUsage {
                    model,
                    usage: iteration.usage,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(line: &str) -> Vec<UsageEntry> {
        ClaudeUsageExtractor.extract_line(line)
    }

    fn usage_line() -> String {
        r#"{"timestamp":"2026-01-01T00:02:30.000Z","version":"1.2.3","sessionId":"sess-1","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4-20250514","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":30,"cache_read_input_tokens":200}}}"#
            .to_string()
    }

    #[test]
    fn prefilter_matches_usage_lines_only() {
        let extractor = ClaudeUsageExtractor;
        assert!(extractor.wants_line(&usage_line()));
        assert!(!extractor.wants_line(r#"{"type":"user","message":{"content":"hi"}}"#));
    }

    #[test]
    fn extracts_a_basic_usage_entry() {
        let entries = extract(&usage_line());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.entry_key, "msg-1|req-1");
        assert_eq!(e.message_id.as_deref(), Some("msg-1"));
        assert_eq!(e.model, "claude-sonnet-4-20250514");
        assert_eq!(e.ts, 1_767_225_750);
        assert_eq!(
            e.tokens,
            TokenCounts {
                input: 100,
                output: 50,
                cache_read: 200,
                cache_write: 30,
                reasoning_output: None,
                total: 380,
            }
        );
        assert_eq!(e.cache_write_1h, 0);
        assert_eq!(e.transcript_cost_micro_usd, None);
        assert!(!e.is_sidechain);
        assert_eq!(e.speed, None);
        assert!(e.speed_inferred, "no usage.speed marker was recorded");
        assert_eq!(e.pricing_shape, PricingShape::Claude);
    }

    #[test]
    fn cache_creation_breakdown_wins_over_flat_count() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":999,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}},"requestId":"r"}"#;
        let entries = extract(line);
        assert_eq!(entries[0].tokens.cache_write, 30);
        assert_eq!(entries[0].cache_write_1h, 20);
    }

    #[test]
    fn honors_transcript_cost_usd() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","costUSD":1.25,"message":{"id":"m","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#;
        assert_eq!(extract(line)[0].transcript_cost_micro_usd, Some(1_250_000));
    }

    #[test]
    fn skips_lines_with_null_schema_fields() {
        assert!(
            extract(
                r#"{"timestamp":"2026-01-01T00:00:00Z","requestId":null,"message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#
            )
            .is_empty()
        );
        // Null content is allowed (ccusage parity).
        assert_eq!(
            extract(
                r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"content":null,"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#
            )
            .len(),
            1
        );
    }

    #[test]
    fn skips_invalid_entries() {
        // Non-semver version.
        assert!(
            extract(
                r#"{"timestamp":"2026-01-01T00:00:00Z","version":"dev","message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#
            )
            .is_empty()
        );
        // Empty message id.
        assert!(
            extract(
                r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"","model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#
            )
            .is_empty()
        );
        // Missing timestamp.
        assert!(
            extract(
                r#"{"message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#
            )
            .is_empty()
        );
        // Missing input_tokens (parse failure, matching ccusage's strict fields).
        assert!(
            extract(
                r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"x","usage":{"output_tokens":1}}}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn synthetic_or_missing_model_becomes_unknown() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"<synthetic>","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#;
        assert_eq!(extract(line)[0].model, UNKNOWN_MODEL);
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#;
        assert_eq!(extract(line)[0].model, UNKNOWN_MODEL);
    }

    #[test]
    fn entries_without_message_id_get_stable_content_keys() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        let a = extract(line);
        let b = extract(line);
        assert!(a[0].entry_key.starts_with("line:"));
        assert_eq!(a[0].entry_key, b[0].entry_key);
        assert_eq!(a[0].message_id, None);
    }

    #[test]
    fn extracts_advisor_iterations_as_separate_entries() {
        let line = r#"{"timestamp":"2026-05-22T02:34:40.000Z","version":"1.2.3","sessionId":"s","requestId":"req-p","costUSD":1.23,"message":{"id":"msg-p","model":"main-model","usage":{"input_tokens":1,"output_tokens":2,"iterations":[{"type":"advisor_message","model":"advisor-model","input_tokens":10,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}]}}}"#;
        let entries = extract(line);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].model, "main-model");
        assert_eq!(entries[0].transcript_cost_micro_usd, Some(1_230_000));
        assert_eq!(entries[1].model, "advisor-model");
        assert_eq!(entries[1].entry_key, "msg-p:advisor:0|req-p");
        assert_eq!(entries[1].message_id.as_deref(), Some("msg-p:advisor:0"));
        // Advisor cost is never covered by the parent costUSD.
        assert_eq!(entries[1].transcript_cost_micro_usd, None);
        assert_eq!(entries[1].tokens.input, 10);
    }

    #[test]
    fn replacement_policy_matches_ccusage() {
        let base = &extract(&usage_line())[0];
        let mut sidechain = base.clone();
        sidechain.is_sidechain = true;
        sidechain.tokens.cache_read = 50_000;

        // Non-sidechain replaces sidechain even with fewer tokens.
        assert!(should_replace_entry(base, &sidechain));
        assert!(!should_replace_entry(&sidechain, base));

        // Same sidechain-ness: larger total wins.
        let mut bigger = base.clone();
        bigger.tokens.output += 10;
        assert!(should_replace_entry(&bigger, base));
        assert!(!should_replace_entry(base, &bigger));

        // Tie: speed marker wins.
        let mut with_speed = base.clone();
        with_speed.speed = Some(Speed::Standard);
        assert!(should_replace_entry(&with_speed, base));
        assert!(!should_replace_entry(base, &with_speed));
        assert!(!should_replace_entry(&base.clone(), base));
    }

    #[test]
    fn sidechain_flag_is_extracted() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","isSidechain":true,"message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#;
        assert!(extract(line)[0].is_sidechain);
    }

    #[test]
    fn speed_marker_is_extracted() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1,"speed":"fast"}},"requestId":"r"}"#;
        let e = &extract(line)[0];
        assert_eq!(e.speed, Some(Speed::Fast));
        assert!(!e.speed_inferred);
        // Model name keeps its base form (deviation from ccusage's "-fast").
        assert_eq!(e.model, "x");

        let standard = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1,"speed":"standard"}},"requestId":"r"}"#;
        assert_eq!(extract(standard)[0].speed, Some(Speed::Standard));
    }

    #[test]
    fn garbled_lines_with_leading_null_marker_do_not_panic() {
        // A line starting with `":null` used to compute a reversed slice
        // range and panic, poisoning the whole file forever.
        assert!(!has_unsupported_null_field(br#"":null,"usage":{}}"#));
        assert!(!has_unsupported_null_field(b":null"));
        assert!(!has_unsupported_null_field(br#""":null"#));
        assert!(extract(r#"":null,"usage":{}}"#).is_empty());
    }

    #[test]
    fn advisor_entries_without_parent_message_id_get_distinct_keys() {
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"model":"main-model","usage":{"input_tokens":1,"output_tokens":2,"iterations":[{"type":"advisor_message","model":"advisor-model","input_tokens":10,"output_tokens":2}]}}}"#;
        let entries = extract(line);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].message_id.is_none());
        assert_ne!(entries[0].entry_key, entries[1].entry_key);
    }

    #[test]
    fn null_rejection_list_is_pinned() {
        // ccusage's unsupported-null set: dropping any entry silently
        // changes what counts. Pin the schema-relevant ones beyond requestId.
        for field in ["speed", "model", "sessionId", "id", "costUSD"] {
            let line = format!(
                r#"{{"timestamp":"2026-01-01T00:00:00Z","{field}":null,"message":{{"id":"m","model":"x","usage":{{"input_tokens":1,"output_tokens":1,"{field}":null}}}},"requestId":"r"}}"#
            );
            assert!(
                extract(&line).is_empty(),
                "null {field} must reject the line"
            );
        }
    }

    #[test]
    fn empty_string_identity_fields_reject_the_line() {
        // An empty requestId would otherwise produce dedup key "m1|",
        // colliding with the absent-request-id key for the same message.
        for (field, json) in [
            (
                "requestId",
                r#"{"timestamp":"2026-01-01T00:00:00Z","requestId":"","message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ),
            (
                "sessionId",
                r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"","message":{"id":"m","model":"x","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#,
            ),
            (
                "model",
                r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"","usage":{"input_tokens":1,"output_tokens":1}},"requestId":"r"}"#,
            ),
        ] {
            assert!(extract(json).is_empty(), "empty {field} must reject");
        }
    }

    #[test]
    fn garbage_cost_usd_falls_back_to_catalog_pricing() {
        // Negative costUSD is corruption, not a price: it must not become a
        // Some(0) that suppresses catalog pricing.
        let line = r#"{"timestamp":"2026-01-01T00:00:00Z","costUSD":-5.0,"message":{"id":"m","model":"claude-sonnet-4-20250514","usage":{"input_tokens":1000000,"output_tokens":0}},"requestId":"r"}"#;
        let entry = &extract(line)[0];
        assert_eq!(entry.transcript_cost_micro_usd, None);
        assert!(super::super::cost::entry_cost_micro_usd(entry).unwrap() > 0);
    }

    #[test]
    fn fast_speed_entries_price_at_the_fast_multiplier() {
        // claude-opus-4-8 carries a 2x fast multiplier in the catalog; a
        // fast-speed entry bills its whole request at it, while the base
        // model name is unchanged.
        let base = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"claude-opus-4-8-20260601","usage":{"input_tokens":1000000,"output_tokens":0}},"requestId":"r"}"#;
        let fast = r#"{"timestamp":"2026-01-01T00:00:00Z","message":{"id":"m","model":"claude-opus-4-8-20260601","usage":{"input_tokens":1000000,"output_tokens":0,"speed":"fast"}},"requestId":"r"}"#;
        assert_eq!(
            super::super::cost::entry_cost_micro_usd(&extract(fast)[0]).unwrap(),
            2 * super::super::cost::entry_cost_micro_usd(&extract(base)[0]).unwrap(),
        );
    }

    #[test]
    fn golden_fixture_extraction_is_pinned() {
        // Real captured transcript (23 usage lines, streaming re-emits of
        // duplicate message ids, cache_creation breakdowns): pins the
        // parser's behavior against serde-shape regressions and upstream
        // format drift that would otherwise silently zero production
        // counting.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/example-claude-code.jsonl");
        let content = std::fs::read_to_string(fixture).unwrap();
        let mut extractor = ClaudeUsageExtractor;
        let mut entries = Vec::new();
        for line in content.lines() {
            if extractor.wants_line(line) {
                entries.extend(extractor.extract_line(line));
            }
        }
        let mut per_model: std::collections::BTreeMap<String, (usize, u64, u64, u64, u64)> =
            std::collections::BTreeMap::new();
        let mut distinct_keys = std::collections::BTreeSet::new();
        for entry in &entries {
            let slot = per_model.entry(entry.model.clone()).or_default();
            slot.0 += 1;
            slot.1 += entry.tokens.input;
            slot.2 += entry.tokens.output;
            slot.3 += entry.tokens.cache_read;
            slot.4 += entry.tokens.cache_write;
            distinct_keys.insert(entry.entry_key.clone());
        }
        insta::assert_debug_snapshot!((entries.len(), distinct_keys.len(), per_model));
    }

    #[test]
    fn secondary_fixtures_still_extract_usage() {
        for fixture in [
            "tests/fixtures/claude-code-with-thinking.jsonl",
            "tests/fixtures/claude-code-with-plan.jsonl",
            "tests/fixtures/claude-model-not-last.jsonl",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
            let content = std::fs::read_to_string(path).unwrap();
            let mut extractor = ClaudeUsageExtractor;
            let mut count = 0;
            for line in content.lines() {
                if extractor.wants_line(line) {
                    count += extractor.extract_line(line).len();
                }
            }
            assert!(count > 0, "{fixture} must yield usage entries");
        }
    }

    #[test]
    fn semver_prefix_validation() {
        assert!(is_semver_prefix("1.2.3"));
        assert!(is_semver_prefix("10.0.1-beta"));
        assert!(!is_semver_prefix("dev"));
        assert!(!is_semver_prefix("1.2"));
        assert!(!is_semver_prefix("1.x.3"));
    }
}
