//! Metrics tracking module.
//!
//! This module provides functionality for recording metric events.
//! Events are routed through the daemon telemetry worker.
//!
//! All public types are re-exported for external use (e.g., ingestion server).

pub mod attrs;
pub mod db;
pub mod events;
pub mod local_stats;
pub mod model_pricing;
pub mod pos_encoded;
pub mod types;

// Re-export all public types for external crates
pub use attrs::EventAttributes;
pub use events::{
    AgentUsageValues, CheckpointValues, CommittedValues, InstallHooksValues, OtelTraceValues,
    RewriteCommittedValues, SessionEventValues, TokenUsageValues,
};
pub use pos_encoded::PosEncoded;
pub use types::{EventValues, METRICS_API_VERSION, MetricEvent, MetricsBatch};

/// Record an event with values and attributes.
///
/// Events are sent to the daemon telemetry worker which batches
/// and uploads them to the API.
///
/// # Example
///
/// ```ignore
/// use crate::metrics::{record, CommittedValues, EventAttributes};
///
/// let values = CommittedValues::new()
///     .commit_sha("abc123...")
///     .human_additions(50)
///     .git_diff_added_lines(150)
///     .git_diff_deleted_lines(20)
///     .tool_model_pairs(vec!["all".to_string()])
///     .ai_additions(vec![100]);
///
/// let attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
///     .repo_url("https://github.com/user/repo")
///     .author("user@example.com")
///     .tool("claude-code");
///
/// record(values, attrs);
/// ```
pub fn record<V: EventValues>(values: V, attrs: EventAttributes) {
    if attrs.tool == Some(Some("mock_ai".to_string())) {
        return;
    }
    if should_ignore_debug_self_check_event(&attrs) {
        return;
    }
    let event = MetricEvent::new(&values, attrs.to_sparse());
    // Write directly to observability log
    crate::observability::log_metrics(vec![event]);
}

fn should_ignore_debug_self_check_event(attrs: &EventAttributes) -> bool {
    if attrs.repo_url.as_ref().is_some_and(|repo_url| {
        repo_url
            .as_ref()
            .is_some_and(|url| crate::diagnostic_sentinels::is_debug_self_check_remote_url(url))
    }) {
        return true;
    }

    std::env::current_dir()
        .is_ok_and(|cwd| crate::diagnostic_sentinels::path_is_in_debug_self_check_root(&cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::MetricEventId;

    #[test]
    fn test_record_creates_event() {
        // This test verifies that record() creates a valid MetricEvent
        // The actual write to the log file happens via observability::log_metrics()
        let values = CommittedValues::new()
            .human_additions(5)
            .git_diff_added_lines(10)
            .git_diff_deleted_lines(5)
            .tool_model_pairs(vec!["all".to_string()])
            .ai_additions(vec![10]);

        let attrs = EventAttributes::with_version("1.0.0")
            .tool("test")
            .commit_sha("test-commit");

        // Create the event manually to verify structure
        let event = MetricEvent::new(&values, attrs.to_sparse());
        assert_eq!(event.event_id, MetricEventId::Committed as u16);
        assert!(event.timestamp > 0);
    }

    /// Verify that the mock_ai guard in record() detects tool="mock_ai" in attrs.
    #[test]
    fn test_mock_ai_is_blocked_by_record_guard() {
        let attrs = EventAttributes::with_version("1.0.0").tool("mock_ai");
        assert_eq!(attrs.tool, Some(Some("mock_ai".to_string())));
        // record() early-returns for mock_ai; nothing to assert on the write
        // side since log_metrics is a no-op in tests, but the guard is exercised.
        let values = events::AgentUsageValues::new();
        record(values, attrs);
    }

    #[test]
    fn test_debug_self_check_repo_url_is_ignored() {
        let attrs = EventAttributes::with_version("1.0.0")
            .repo_url(crate::diagnostic_sentinels::DEBUG_SELF_CHECK_NORMALIZED_REMOTE_URL);
        assert!(should_ignore_debug_self_check_event(&attrs));
    }

    /// Verify that a Committed event whose tool_model_pairs contain "mock_ai::unknown"
    /// but whose attrs.tool is unset would NOT be caught by the record() guard.
    /// This demonstrates why filtering must also happen at the call site
    /// (post_commit::record_commit_metrics).
    #[test]
    fn test_committed_with_mock_ai_tool_model_pair_bypasses_attrs_guard() {
        let attrs = EventAttributes::with_version("1.0.0");
        // attrs.tool is None — the guard won't trigger
        assert_eq!(attrs.tool, None);

        let values = CommittedValues::new()
            .tool_model_pairs(vec!["all".to_string(), "mock_ai::unknown".to_string()])
            .ai_additions(vec![10, 10]);

        // This would pass through record() — the call-site filter in
        // record_commit_metrics is responsible for stripping mock_ai entries.
        record(values, attrs);
    }
}
