//! Timestamp extraction policy shared by stream adapters and daemon consumers.

/// Extract a Unix-epoch timestamp from a raw JSON event's `timestamp` field.
///
/// Supports RFC 3339 strings and numeric milliseconds. Missing or unparseable
/// values return `None` so callers can apply their context-specific fallback.
pub fn parse_event_timestamp(event: &serde_json::Value) -> Option<u32> {
    let timestamp = event.get("timestamp")?;
    if let Some(value) = timestamp.as_str() {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|parsed| parsed.timestamp() as u32)
    } else {
        timestamp.as_u64().map(|millis| (millis / 1000) as u32)
    }
}

/// Fallback timestamp from file metadata when an event has no timestamp.
///
/// The first event uses birthtime when available, falling back to mtime. Later
/// events use mtime. If neither is available, the current clock is used.
pub(crate) fn file_time_fallback(meta: &std::fs::Metadata, is_first_event: bool) -> u32 {
    let time = if is_first_event {
        meta.created().or_else(|_| meta.modified()).ok()
    } else {
        meta.modified().ok()
    };
    time.and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or_else(|| crate::model::clock::now_secs() as u32)
}

/// Extract an event timestamp, falling back to the transcript file's metadata.
pub(crate) fn event_timestamp_or_file_time(
    event: &serde_json::Value,
    file_meta: &std::fs::Metadata,
    is_first_event: bool,
) -> u32 {
    parse_event_timestamp(event).unwrap_or_else(|| file_time_fallback(file_meta, is_first_event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        let event = serde_json::json!({"timestamp": "2026-05-11T23:13:12.819Z"});
        assert_eq!(parse_event_timestamp(&event), Some(1778541192));
    }

    #[test]
    fn parses_rfc3339_without_millis() {
        let event = serde_json::json!({"timestamp": "2026-05-12T00:21:05Z"});
        assert_eq!(parse_event_timestamp(&event), Some(1778545265));
    }

    #[test]
    fn discards_rfc3339_subseconds() {
        let event = serde_json::json!({"timestamp": "2026-05-11T23:13:12.999Z"});
        assert_eq!(parse_event_timestamp(&event), Some(1778541192));
    }

    #[test]
    fn parses_numeric_millis() {
        let event = serde_json::json!({"timestamp": 1759845073835u64});
        assert_eq!(parse_event_timestamp(&event), Some(1759845073));
    }

    #[test]
    fn rejects_missing_field() {
        let event = serde_json::json!({"type": "user.message"});
        assert_eq!(parse_event_timestamp(&event), None);
    }

    #[test]
    fn rejects_null_value() {
        let event = serde_json::json!({"timestamp": null});
        assert_eq!(parse_event_timestamp(&event), None);
    }

    #[test]
    fn rejects_invalid_string() {
        let event = serde_json::json!({"timestamp": "not-a-date"});
        assert_eq!(parse_event_timestamp(&event), None);
    }

    #[test]
    fn rejects_empty_string() {
        let event = serde_json::json!({"timestamp": ""});
        assert_eq!(parse_event_timestamp(&event), None);
    }

    #[test]
    fn event_timestamp_wins_over_file_fallback() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let event = serde_json::json!({"timestamp": 1759845073835u64});

        assert_eq!(
            event_timestamp_or_file_time(&event, &metadata, false),
            1759845073
        );
    }

    #[test]
    fn missing_event_timestamp_uses_file_fallback() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let metadata = file.as_file().metadata().unwrap();
        let event = serde_json::json!({"type": "user.message"});

        assert_eq!(
            event_timestamp_or_file_time(&event, &metadata, false),
            file_time_fallback(&metadata, false)
        );
    }
}
