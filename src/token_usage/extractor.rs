//! Per-agent usage extraction trait.

use serde::{Deserialize, Serialize};

use super::types::{Speed, UsageEntry};

/// A forked session's request for its parent's pre-fork usage history: the
/// parent's external session id, and the fork instant (the child's
/// `session_meta` timestamp; parent usage past it was never replayed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentPrefixRequest {
    pub parent_id: String,
    pub fork_ts_ms: i64,
}

/// Raw per-event usage identity used to match a forked session's replayed
/// leading events against its parent's history. Timestamps are excluded —
/// Codex rewrites replayed history to the fork instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSignature {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning_output: u64,
    pub total: u64,
}

/// Incremental, line-oriented extractor of token-usage entries from an agent
/// transcript. Fed complete JSONL lines in file order; may keep per-session
/// state between lines, persisted across runs via `state_json`.
pub trait UsageExtractor: Send {
    /// Cheap substring prefilter run before any JSON parsing. Must return
    /// true for every line that could yield usage or affect extractor state.
    fn wants_line(&self, line: &str) -> bool;

    /// Consume one raw JSONL line, returning any usage entries it completes.
    fn extract_line(&mut self, line: &str) -> Vec<UsageEntry>;

    /// Inject the configuration-derived speed for entries whose transcript
    /// records no service tier (Codex `~/.codex/config.toml`). Resolved by
    /// the worker once per pass, before any line is fed. No-op for agents
    /// without the concept.
    fn set_fallback_speed(&mut self, _speed: Option<Speed>) {}

    /// The extractor found a fork and needs its parent's pre-fork usage
    /// prefix before it can judge usage events (Codex). The worker answers
    /// via [`UsageExtractor::provide_parent_prefix`] after every consumed
    /// line and after restoring state, so the request is served before the
    /// next usage event. `None` for extractors without the concept.
    fn parent_request(&self) -> Option<ParentPrefixRequest> {
        None
    }

    /// Answer a [`UsageExtractor::parent_request`]: `Some` carries the
    /// parent's pre-fork usage signatures in file order; `None` means the
    /// parent could not be resolved, and the extractor takes its
    /// unavailable-parent fallback (never retried).
    fn provide_parent_prefix(&mut self, _prefix: Option<Vec<UsageSignature>>) {}

    /// Serialized parser state to persist between incremental runs. `None`
    /// for stateless extractors.
    fn state_json(&self) -> Option<String> {
        None
    }

    /// Restore state persisted by an earlier `state_json` call. Returns
    /// false when the state was unreadable (corrupt, or written by a
    /// different version): the extractor resets to defaults, and the caller
    /// must also reset its read cursor — replaying a file against default
    /// state is safe (entry-level dedup), but continuing mid-file with
    /// default state would book the session's whole cumulative history as
    /// one fresh delta.
    fn restore_state(&mut self, _json: &str) -> bool {
        true
    }

    /// True when the extractor holds buffered entries that a later
    /// [`UsageExtractor::flush`] could release. Files with pending state must
    /// be re-processed even when their bytes have not changed.
    fn has_pending(&self) -> bool {
        false
    }

    /// Release buffered entries whose deferral window has passed as of
    /// `now_ms` (wall clock, unix millis). Called when a pass reaches the end
    /// of the file.
    fn flush(&mut self, _now_ms: i64) -> Vec<UsageEntry> {
        Vec::new()
    }
}

/// Extractor for the given git-ai tool id, if token usage is supported.
pub fn extractor_for_tool(tool: &str) -> Option<Box<dyn UsageExtractor>> {
    match tool {
        "claude" => Some(Box::new(super::claude::ClaudeUsageExtractor)),
        "codex" => Some(Box::new(super::codex::CodexUsageExtractor::default())),
        _ => None,
    }
}
