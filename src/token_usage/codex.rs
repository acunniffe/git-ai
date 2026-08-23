//! Codex rollout transcript usage extraction.
//!
//! Ported from ccusage's Codex adapter (rust/adapters/codex/src/parser.rs in
//! <https://github.com/ccusage/ccusage>, MIT License, Copyright (c) 2025
//! @ryoppippi), adapted to git-ai's incremental line-oriented streaming with
//! persisted parser state.
//!
//! Deviations from ccusage:
//! - Session rollout format only (`event_msg`/`token_count`, `turn_context`,
//!   `session_meta`); the headless `codex exec` log format is not tracked by
//!   git-ai's streams and is not parsed.
//! - No service-tier / fast pricing multipliers and no `codex-auto-review`
//!   release-date fallback table; model ids price through git-ai's catalog.
//! - Fork replay: ccusage matches a forked session's leading usage against
//!   the parent log's usage prefix, which requires reading other files. That
//!   is not possible incrementally, so forks always take ccusage's fallback
//!   for an unavailable parent log: the "rewritten burst" heuristic (leading
//!   usage events spaced <= 1s apart are replayed history and are skipped).

use serde::{Deserialize, Serialize};

use super::extractor::UsageExtractor;
use super::types::{TokenCounts, UsageEntry};

/// ccusage `CODEX_REWRITTEN_BURST_PAUSE_MS`: the longest pause tolerated
/// inside a burst of replayed usage. Codex rewrites replayed history to the
/// fork instant and writes it in one go, so the burst is dense (10-40ms in
/// measured logs) while the child's own first turn follows a real pause.
const REWRITTEN_BURST_PAUSE_MS: i64 = 1_000;

/// ccusage's model fallback when a rollout names no model at all.
const FALLBACK_MODEL: &str = "gpt-5";

#[derive(Default)]
pub struct CodexUsageExtractor {
    state: CodexState,
}

/// Parser state persisted between incremental runs.
///
/// Replaying lines against post-batch state would corrupt `prev_totals` and
/// duplicate entries, so callers must persist this state atomically with the
/// read cursor and the extracted entries (the token-usage database commits
/// all three in one transaction).
#[derive(Debug, Default, Serialize, Deserialize)]
struct CodexState {
    /// Most recent model named by a `turn_context` (or usage) payload.
    model: Option<String>,
    /// Last cumulative `total_token_usage`, for repeat-skipping and delta
    /// subtraction.
    prev_totals: Option<CodexTotals>,
    /// Monotonic counter assigning stable entry keys.
    entry_seq: u64,
    #[serde(default)]
    replay: ReplayState,
}

/// Cumulative or per-turn raw usage as recorded by Codex.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CodexTotals {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

/// Fork-replay filter state (ccusage `CodexReplayState`, minus the
/// parent-prefix arm — see the module docs).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplayState {
    /// Not a fork, or past the replayed history.
    #[default]
    Done,
    /// Fork detected; no usage event seen yet.
    AwaitingFirst,
    /// One usage event buffered: whether it was replayed history depends on
    /// how soon the next one follows.
    AwaitingSecond { first: PendingEvent },
    /// Inside the rewritten burst; events within the pause window are
    /// replayed history.
    SkippingBurst { last_ts_ms: i64 },
}

/// A usage event held back until the burst decision can be made.
#[derive(Debug, Serialize, Deserialize)]
struct PendingEvent {
    ts_ms: i64,
    model: String,
    delta: CodexTotals,
}

impl UsageExtractor for CodexUsageExtractor {
    fn wants_line(&self, line: &str) -> bool {
        line.contains("token_count")
            || line.contains("turn_context")
            || line.contains("session_meta")
    }

    fn extract_line(&mut self, line: &str) -> Vec<UsageEntry> {
        let Ok(raw) = serde_json::from_str::<RawLine>(line) else {
            return Vec::new();
        };
        match raw.entry_type.as_deref() {
            Some("session_meta") => {
                if raw.payload.as_ref().is_some_and(is_forked_session) {
                    self.state.replay = ReplayState::AwaitingFirst;
                }
                Vec::new()
            }
            Some("turn_context") => {
                if let Some(model) = raw.payload.as_ref().and_then(payload_model) {
                    self.state.model = Some(model);
                }
                Vec::new()
            }
            Some("event_msg") => self.handle_event_msg(&raw),
            _ => Vec::new(),
        }
    }

    fn state_json(&self) -> Option<String> {
        serde_json::to_string(&self.state).ok()
    }

    fn restore_state(&mut self, json: &str) {
        self.state = serde_json::from_str(json).unwrap_or_default();
    }
}

impl CodexUsageExtractor {
    fn handle_event_msg(&mut self, raw: &RawLine) -> Vec<UsageEntry> {
        let Some(payload) = raw.payload.as_ref() else {
            return Vec::new();
        };
        if payload.payload_type.as_deref() != Some("token_count") {
            return Vec::new();
        }
        let Some(ts_ms) = timestamp_millis(raw.timestamp.as_ref()) else {
            return Vec::new();
        };

        // Delta computation (ccusage `visit_codex_session_entry`): skip
        // repeats of an unchanged cumulative total, prefer the recorded
        // per-turn usage, else subtract the previous cumulative total.
        let info = payload.info.as_ref();
        let total_usage = info.and_then(|info| info.total_token_usage);
        let cumulative_advanced =
            total_usage.is_none_or(|totals| self.state.prev_totals != Some(totals));
        let delta = info
            .and_then(|info| info.last_token_usage)
            .filter(|_| cumulative_advanced)
            .or_else(|| total_usage.map(|totals| subtract_totals(totals, self.state.prev_totals)));
        if let Some(totals) = total_usage {
            self.state.prev_totals = Some(totals);
        }
        let Some(delta) = delta else {
            return Vec::new();
        };
        if delta.input_tokens == 0
            && delta.cached_input_tokens == 0
            && delta.output_tokens == 0
            && delta.reasoning_output_tokens == 0
        {
            return Vec::new();
        }

        let parsed_model = payload_model(payload).or_else(|| info.and_then(info_model));
        if let Some(model) = &parsed_model {
            self.state.model = Some(model.clone());
        }
        let model = self
            .state
            .model
            .clone()
            .unwrap_or_else(|| FALLBACK_MODEL.to_string());

        self.filter_replay(ts_ms, model, delta)
    }

    /// Run one usage event through the fork-replay filter, returning the
    /// entries that count as the session's own usage.
    fn filter_replay(&mut self, ts_ms: i64, model: String, delta: CodexTotals) -> Vec<UsageEntry> {
        match std::mem::take(&mut self.state.replay) {
            ReplayState::Done => {
                vec![self.make_entry(ts_ms, model, delta)]
            }
            ReplayState::AwaitingFirst => {
                self.state.replay = ReplayState::AwaitingSecond {
                    first: PendingEvent {
                        ts_ms,
                        model,
                        delta,
                    },
                };
                Vec::new()
            }
            ReplayState::AwaitingSecond { first } => {
                if (0..=REWRITTEN_BURST_PAUSE_MS).contains(&(ts_ms - first.ts_ms)) {
                    // Two usage events back to back: a replayed burst. Both
                    // belong to the parent's history.
                    self.state.replay = ReplayState::SkippingBurst { last_ts_ms: ts_ms };
                    Vec::new()
                } else {
                    // A real pause: the session recorded its own turns from
                    // the start.
                    vec![
                        self.make_entry(first.ts_ms, first.model, first.delta),
                        self.make_entry(ts_ms, model, delta),
                    ]
                }
            }
            ReplayState::SkippingBurst { last_ts_ms } => {
                if (0..=REWRITTEN_BURST_PAUSE_MS).contains(&(ts_ms - last_ts_ms)) {
                    self.state.replay = ReplayState::SkippingBurst { last_ts_ms: ts_ms };
                    Vec::new()
                } else {
                    vec![self.make_entry(ts_ms, model, delta)]
                }
            }
        }
    }

    fn make_entry(&mut self, ts_ms: i64, model: String, delta: CodexTotals) -> UsageEntry {
        self.state.entry_seq += 1;
        // ccusage clamps cached to input; normalized input excludes cache.
        let cached = delta.cached_input_tokens.min(delta.input_tokens);
        UsageEntry {
            entry_key: format!("codex:{}", self.state.entry_seq),
            message_id: None,
            ts: (ts_ms / 1000).clamp(0, u32::MAX as i64) as u32,
            model,
            tokens: TokenCounts {
                input: delta.input_tokens - cached,
                output: delta.output_tokens,
                cache_read: cached,
                cache_write: 0,
                reasoning_output: Some(delta.reasoning_output_tokens),
                total: delta.total_tokens,
            },
            cache_write_1h: 0,
            transcript_cost_micro_usd: None,
            is_sidechain: false,
            has_speed: false,
        }
    }
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    timestamp: Option<serde_json::Value>,
    payload: Option<RawPayload>,
}

#[derive(Deserialize)]
struct RawPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    info: Option<RawInfo>,
    model: Option<String>,
    model_name: Option<String>,
    metadata: Option<RawMetadata>,
    // session_meta fields:
    forked_from_id: Option<String>,
    source: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawInfo {
    total_token_usage: Option<CodexTotals>,
    last_token_usage: Option<CodexTotals>,
    model: Option<String>,
    model_name: Option<String>,
    metadata: Option<RawMetadata>,
}

#[derive(Deserialize)]
struct RawMetadata {
    model: Option<String>,
}

/// Fork markers from ccusage `read_codex_session_metadata`.
fn is_forked_session(payload: &RawPayload) -> bool {
    let non_empty = |value: Option<&str>| value.is_some_and(|v| !v.is_empty());
    non_empty(payload.forked_from_id.as_deref())
        || non_empty(
            payload
                .source
                .as_ref()
                .and_then(|source| source.pointer("/subagent/thread_spawn/parent_thread_id"))
                .and_then(|value| value.as_str()),
        )
}

fn payload_model(payload: &RawPayload) -> Option<String> {
    model_from_parts(
        payload.model.as_deref(),
        payload.model_name.as_deref(),
        payload.metadata.as_ref(),
    )
}

fn info_model(info: &RawInfo) -> Option<String> {
    model_from_parts(
        info.model.as_deref(),
        info.model_name.as_deref(),
        info.metadata.as_ref(),
    )
}

fn model_from_parts(
    model: Option<&str>,
    model_name: Option<&str>,
    metadata: Option<&RawMetadata>,
) -> Option<String> {
    let non_empty = |value: Option<&str>| {
        value.and_then(|v| {
            let v = v.trim();
            (!v.is_empty()).then(|| v.to_string())
        })
    };
    non_empty(model)
        .or_else(|| non_empty(model_name))
        .or_else(|| non_empty(metadata.and_then(|m| m.model.as_deref())))
}

/// Codex timestamps are RFC3339 strings or epoch numbers (seconds or millis).
fn timestamp_millis(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return chrono::DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|dt| dt.timestamp_millis())
            .filter(|ms| *ms >= 0);
    }
    let raw = value.as_u64()?;
    let millis = if raw > 10_000_000_000 {
        raw
    } else {
        raw.checked_mul(1_000)?
    };
    Some(millis.min(i64::MAX as u64) as i64)
}

fn subtract_totals(current: CodexTotals, previous: Option<CodexTotals>) -> CodexTotals {
    let previous = previous.unwrap_or_default();
    CodexTotals {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        cached_input_tokens: current
            .cached_input_tokens
            .saturating_sub(previous.cached_input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        reasoning_output_tokens: current
            .reasoning_output_tokens
            .saturating_sub(previous.reasoning_output_tokens),
        total_tokens: current.total_tokens.saturating_sub(previous.total_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_count_line(ts: &str, total: (u64, u64, u64, u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{}}}}}}}}}"#,
            total.0, total.1, total.2, total.3, total.4
        )
    }

    fn turn_context_line(model: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{{"model":"{model}"}}}}"#
        )
    }

    #[test]
    fn prefilter_matches_relevant_lines() {
        let e = CodexUsageExtractor::default();
        assert!(e.wants_line(&token_count_line("2026-01-01T00:00:00Z", (1, 0, 1, 0, 2))));
        assert!(e.wants_line(&turn_context_line("gpt-5.1")));
        assert!(e.wants_line(r#"{"type":"session_meta","payload":{"id":"x"}}"#));
        assert!(!e.wants_line(r#"{"type":"response_item","payload":{"type":"message"}}"#));
    }

    #[test]
    fn computes_deltas_from_cumulative_totals() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&turn_context_line("gpt-5.1"));
        let first = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].model, "gpt-5.1");
        assert_eq!(first[0].tokens.input, 60); // 100 input - 40 cached
        assert_eq!(first[0].tokens.cache_read, 40);
        assert_eq!(first[0].tokens.output, 50);
        assert_eq!(first[0].tokens.reasoning_output, Some(10));
        assert_eq!(first[0].tokens.total, 150);
        assert_eq!(first[0].entry_key, "codex:1");

        let second = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].tokens.input, 100); // (300-100) - (140-40)
        assert_eq!(second[0].tokens.cache_read, 100);
        assert_eq!(second[0].tokens.output, 40);
        assert_eq!(second[0].tokens.reasoning_output, Some(20));
        assert_eq!(second[0].entry_key, "codex:2");
    }

    #[test]
    fn skips_repeats_of_unchanged_cumulative_totals() {
        let mut e = CodexUsageExtractor::default();
        let line = token_count_line("2026-01-01T00:00:10Z", (100, 0, 50, 0, 150));
        assert_eq!(e.extract_line(&line).len(), 1);
        assert!(e.extract_line(&line).is_empty());
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:05:00Z",
                (100, 0, 50, 0, 150)
            ))
            .is_empty()
        );
    }

    #[test]
    fn prefers_last_token_usage_when_cumulative_advanced() {
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":150},"last_token_usage":{"input_tokens":7,"cached_input_tokens":0,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":10}}}}"#;
        let entries = e.extract_line(line);
        assert_eq!(entries[0].tokens.input, 7);
        assert_eq!(entries[0].tokens.output, 3);
        assert_eq!(entries[0].tokens.total, 10);
    }

    #[test]
    fn skips_all_zero_deltas() {
        let mut e = CodexUsageExtractor::default();
        assert!(
            e.extract_line(&token_count_line("2026-01-01T00:00:10Z", (0, 0, 0, 0, 0)))
                .is_empty()
        );
    }

    #[test]
    fn clamps_cached_to_input() {
        let mut e = CodexUsageExtractor::default();
        let entries = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (10, 25, 5, 0, 15),
        ));
        assert_eq!(entries[0].tokens.cache_read, 10);
        assert_eq!(entries[0].tokens.input, 0);
    }

    #[test]
    fn falls_back_to_gpt5_without_model_context() {
        let mut e = CodexUsageExtractor::default();
        let entries = e.extract_line(&token_count_line("2026-01-01T00:00:10Z", (1, 0, 1, 0, 2)));
        assert_eq!(entries[0].model, FALLBACK_MODEL);
    }

    #[test]
    fn forked_session_skips_rewritten_burst() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        // Three replayed events written within the burst window.
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.000Z",
                (10, 0, 5, 0, 15)
            ))
            .is_empty()
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.400Z",
                (20, 0, 10, 0, 30)
            ))
            .is_empty()
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.900Z",
                (30, 0, 15, 0, 45)
            ))
            .is_empty()
        );
        // The child's own first turn follows a real pause and is counted.
        let own = e.extract_line(&token_count_line(
            "2026-01-01T00:00:20Z",
            (40, 0, 20, 0, 60),
        ));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].tokens.input, 10);
        assert_eq!(own[0].tokens.output, 5);
    }

    #[test]
    fn forked_session_with_real_pause_counts_from_the_start() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .is_empty()
        );
        // 8s pause: not a rewritten burst, so both events are real usage.
        let entries = e.extract_line(&token_count_line(
            "2026-01-01T00:00:09Z",
            (20, 0, 10, 0, 30),
        ));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tokens.input, 10);
        assert_eq!(entries[1].tokens.input, 10);
    }

    #[test]
    fn subagent_thread_spawn_counts_as_fork() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}}}"#,
        );
        assert!(matches!(e.state.replay, ReplayState::AwaitingFirst));
    }

    #[test]
    fn unforked_session_counts_immediately() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"solo"}}"#,
        );
        assert_eq!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .len(),
            1
        );
    }

    #[test]
    fn state_roundtrip_matches_single_pass() {
        let lines = [
            turn_context_line("gpt-5.2"),
            token_count_line("2026-01-01T00:00:10Z", (100, 40, 50, 10, 150)),
            token_count_line("2026-01-01T00:01:10Z", (300, 140, 90, 30, 390)),
            token_count_line("2026-01-01T00:02:10Z", (450, 200, 120, 40, 570)),
        ];

        let mut single = CodexUsageExtractor::default();
        let single_pass: Vec<_> = lines
            .iter()
            .flat_map(|line| single.extract_line(line))
            .collect();

        // Same lines, with a state save/restore after every line.
        let mut resumed_entries = Vec::new();
        let mut state: Option<String> = None;
        for line in &lines {
            let mut e = CodexUsageExtractor::default();
            if let Some(json) = &state {
                e.restore_state(json);
            }
            resumed_entries.extend(e.extract_line(line));
            state = e.state_json();
        }

        assert_eq!(single_pass, resumed_entries);
        assert_eq!(single_pass.len(), 3);
    }

    #[test]
    fn corrupt_state_resets_to_defaults() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 0, 50, 0, 150),
        ));
        e.restore_state("{not json");
        assert!(e.state.prev_totals.is_none());
        assert_eq!(e.state.entry_seq, 0);
    }

    #[test]
    fn numeric_timestamps_are_supported() {
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":1767225610,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}}}}"#;
        let entries = e.extract_line(line);
        assert_eq!(entries[0].ts, 1_767_225_610);
    }
}
