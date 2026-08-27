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
//! - The service tier is resolved per entry at extraction time (recorded
//!   `thread_settings_applied` tier, else the injected config fallback, else
//!   standard) and stored on the entry; ccusage's auto mode instead applies
//!   the config in force at *report* time to unmarked usage, retroactively
//!   repricing history when `~/.codex/config.toml` changes.
//! - No `codex-auto-review` release-date fallback table; model ids price
//!   through git-ai's catalog.
//! - Fork replay: ccusage matches a forked session's leading usage against
//!   the parent log's usage prefix, which requires reading other files. That
//!   is not possible incrementally, so forks always take ccusage's fallback
//!   for an unavailable parent log: the "rewritten burst" heuristic (leading
//!   usage events spaced <= 1s apart are replayed history and are skipped).
//! - Numeric token fields accept ccusage's aliases and string-encoded
//!   numbers, but a line whose payload/info has an unexpected *shape* (e.g. a
//!   scalar where an object belongs) is skipped whole, where ccusage's lossy
//!   deserializers would still process it. Timestamp parsing is slightly
//!   more lenient than ccusage's fixed-width RFC3339 forms.
//! - Cost: no long-context tiered pricing and no `codex-auto-review` model
//!   mapping (that model prices at $0 unless the catalog learns it); see
//!   `cost.rs` for the shared pricing deviations.

use serde::{Deserialize, Serialize};

use super::extractor::UsageExtractor;
use super::types::{PricingShape, Speed, TokenCounts, UsageEntry};

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
    /// Speed for entries whose transcript records no service tier, resolved
    /// from `~/.codex/config.toml` and injected per pass by the worker (not
    /// persisted — the config in force when an entry is extracted decides).
    fallback_speed: Option<Speed>,
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
    #[serde(default)]
    model: Option<String>,
    /// Last cumulative `total_token_usage`, for repeat-skipping and delta
    /// subtraction.
    #[serde(default)]
    prev_totals: Option<CodexTotals>,
    /// Sticky service tier recorded by the last `thread_settings_applied`
    /// event that carried one (ccusage `current_service_tier`).
    #[serde(default)]
    service_tier: Option<Speed>,
    #[serde(default)]
    replay: ReplayState,
}

/// Cumulative or per-turn raw usage as recorded by Codex. Field aliases,
/// lossy numeric parsing (string-encoded counts), and total derivation match
/// ccusage's custom `CodexRawUsage` deserializer (rust/adapters/codex/src/
/// types.rs): a recorded zero total means the field is unusable rather than
/// that the turn spent nothing, so it derives to input + output (reasoning is
/// a subset of output and must not be added on top).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CodexTotals {
    #[serde(
        default,
        alias = "prompt_tokens",
        alias = "input",
        deserialize_with = "lossy_u64"
    )]
    input_tokens: u64,
    #[serde(
        default,
        alias = "cache_read_input_tokens",
        alias = "cached_tokens",
        deserialize_with = "lossy_u64"
    )]
    cached_input_tokens: u64,
    #[serde(
        default,
        alias = "completion_tokens",
        alias = "output",
        deserialize_with = "lossy_u64"
    )]
    output_tokens: u64,
    #[serde(default, alias = "reasoning_tokens", deserialize_with = "lossy_u64")]
    reasoning_output_tokens: u64,
    #[serde(default, deserialize_with = "lossy_u64")]
    total_tokens: u64,
}

impl CodexTotals {
    fn normalized(mut self) -> Self {
        if self.total_tokens == 0 {
            self.total_tokens = self.input_tokens.saturating_add(self.output_tokens);
        }
        self
    }
}

/// Accept unsigned integers or numeric strings; anything else counts as
/// absent (ccusage `deserialize_optional_u64_lossy`).
fn lossy_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0))
}

/// Fork-replay filter state (ccusage `CodexReplayState`, minus the
/// parent-prefix arm — see the module docs). Tracks every usage-carrying
/// event's timestamp, matching ccusage's `detect_rewritten_burst`, which
/// anchors on raw usage events even when they are cumulative repeats that
/// produce no delta.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplayState {
    /// Not a fork, or past the replayed history.
    #[default]
    Done,
    /// Fork detected; no usage event seen yet.
    AwaitingFirst,
    /// One usage event seen (its delta, if any, is buffered): whether it was
    /// replayed history depends on how soon the next one follows.
    AwaitingSecond {
        first_ts_ms: i64,
        pending: Option<PendingEvent>,
    },
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
    /// Resolved speed at event time (defaults keep pre-speed persisted state
    /// readable).
    #[serde(default)]
    speed: Speed,
    #[serde(default)]
    speed_inferred: bool,
}

impl UsageExtractor for CodexUsageExtractor {
    fn wants_line(&self, line: &str) -> bool {
        line.contains("token_count")
            || line.contains("turn_context")
            || line.contains("session_meta")
            || line.contains("thread_settings_applied")
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
                // session_meta names the model too (matching
                // streams::model_extraction, so usage prices under the same
                // model SessionEvents resolve).
                if let Some(model) = raw.payload.as_ref().and_then(payload_model) {
                    self.state.model = Some(model);
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

    fn restore_state(&mut self, json: &str) -> bool {
        match serde_json::from_str(json) {
            Ok(state) => {
                self.state = state;
                true
            }
            Err(_) => {
                self.state = CodexState::default();
                false
            }
        }
    }

    fn set_fallback_speed(&mut self, speed: Option<Speed>) {
        self.fallback_speed = speed;
    }

    fn has_pending(&self) -> bool {
        matches!(
            self.state.replay,
            ReplayState::AwaitingSecond {
                pending: Some(_),
                ..
            }
        )
    }

    /// A forked session whose transcript ends while its first usage event is
    /// still parked in `AwaitingSecond` would never release it (a single-turn
    /// subagent rollout, for example). Once the burst window has passed in
    /// wall-clock time, no later event can be within it, so the buffered
    /// event is real usage and is released.
    ///
    /// The successor state is `SkippingBurst` anchored at the released
    /// event's timestamp, not `Done`: if the release misfired because the
    /// replayed burst was written with >1s of lag (recorded timestamps still
    /// sub-second apart), the late-arriving burst partners land within the
    /// window and are skipped, bounding the over-count to the one released
    /// event rather than the fork's whole replayed history. A genuine own
    /// turn is unaffected — its timestamp is necessarily past the window the
    /// flush itself just waited out, so it exits the skip and counts.
    fn flush(&mut self, now_ms: i64) -> Vec<UsageEntry> {
        let ReplayState::AwaitingSecond { first_ts_ms, .. } = &self.state.replay else {
            return Vec::new();
        };
        let anchor_ts_ms = *first_ts_ms;
        if now_ms - anchor_ts_ms <= REWRITTEN_BURST_PAUSE_MS {
            return Vec::new();
        }
        let ReplayState::AwaitingSecond { pending, .. } = std::mem::replace(
            &mut self.state.replay,
            ReplayState::SkippingBurst {
                last_ts_ms: anchor_ts_ms,
            },
        ) else {
            unreachable!("matched AwaitingSecond above");
        };
        pending.map(make_entry).into_iter().collect()
    }
}

impl CodexUsageExtractor {
    fn handle_event_msg(&mut self, raw: &RawLine) -> Vec<UsageEntry> {
        let Some(payload) = raw.payload.as_ref() else {
            return Vec::new();
        };
        if payload.payload_type.as_deref() == Some("thread_settings_applied") {
            // A settings event that carries no `service_tier` at all says
            // nothing about the tier, so the previous one stands (Codex emits
            // such events for auto-review threads). A tier that is present
            // but unrecognized is different: it means the tier changed to
            // something unknown, so the stale value must not be inherited
            // (ccusage `visit_codex_session_entry`).
            if let Some(recorded) = payload
                .thread_settings
                .as_ref()
                .and_then(|settings| settings.service_tier.as_deref())
            {
                self.state.service_tier = service_tier_speed(recorded);
            }
            return Vec::new();
        }
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
        let total_usage = info
            .and_then(|info| info.total_token_usage)
            .map(CodexTotals::normalized);
        let last_usage = info
            .and_then(|info| info.last_token_usage)
            .map(CodexTotals::normalized);
        if total_usage.is_none() && last_usage.is_none() {
            return Vec::new();
        }
        let cumulative_advanced =
            total_usage.is_none_or(|totals| self.state.prev_totals != Some(totals));
        let delta = last_usage
            .filter(|_| cumulative_advanced)
            .or_else(|| total_usage.map(|totals| subtract_totals(totals, self.state.prev_totals)));
        if let Some(totals) = total_usage {
            self.state.prev_totals = Some(totals);
        }
        let delta = delta.filter(|delta| {
            delta.input_tokens != 0
                || delta.cached_input_tokens != 0
                || delta.output_tokens != 0
                || delta.reasoning_output_tokens != 0
        });

        let event = delta.map(|delta| {
            let parsed_model = payload_model(payload).or_else(|| info.and_then(info_model));
            if let Some(model) = &parsed_model {
                self.state.model = Some(model.clone());
            }
            let model = self
                .state
                .model
                .clone()
                .unwrap_or_else(|| FALLBACK_MODEL.to_string());
            // Recorded tier wins, else the config fallback, else standard
            // (ccusage's auto speed policy, resolved at event time).
            let recorded = self.state.service_tier;
            PendingEvent {
                ts_ms,
                model,
                delta,
                speed: recorded.or(self.fallback_speed).unwrap_or_default(),
                speed_inferred: recorded.is_none(),
            }
        });
        // The replay filter sees every usage-carrying event, including
        // zero-delta repeats: they still anchor/extend the rewritten burst.
        self.filter_replay(ts_ms, event)
    }

    /// Run one usage-carrying event through the fork-replay filter, returning
    /// the entries that count as the session's own usage. `event` is `None`
    /// for events that produced no delta but still mark activity.
    fn filter_replay(&mut self, ts_ms: i64, event: Option<PendingEvent>) -> Vec<UsageEntry> {
        let within_burst =
            |anchor_ts_ms: i64| (0..=REWRITTEN_BURST_PAUSE_MS).contains(&(ts_ms - anchor_ts_ms));
        match std::mem::take(&mut self.state.replay) {
            ReplayState::Done => event.map(|e| vec![make_entry(e)]).unwrap_or_default(),
            ReplayState::AwaitingFirst => {
                self.state.replay = ReplayState::AwaitingSecond {
                    first_ts_ms: ts_ms,
                    pending: event,
                };
                Vec::new()
            }
            ReplayState::AwaitingSecond {
                first_ts_ms,
                pending,
            } => {
                if within_burst(first_ts_ms) {
                    // Two usage events back to back: a replayed burst. Both
                    // belong to the parent's history.
                    self.state.replay = ReplayState::SkippingBurst { last_ts_ms: ts_ms };
                    Vec::new()
                } else {
                    // A real pause: the session recorded its own turns from
                    // the start.
                    pending.into_iter().chain(event).map(make_entry).collect()
                }
            }
            ReplayState::SkippingBurst { last_ts_ms } => {
                if event.is_none() {
                    // Zero-delta repeats never reach ccusage's skip machine:
                    // they must neither extend the burst window (a chain of
                    // sub-second heartbeats could otherwise bridge it across
                    // the child's real first turn) nor resolve it.
                    self.state.replay = ReplayState::SkippingBurst { last_ts_ms };
                    Vec::new()
                } else if within_burst(last_ts_ms) {
                    self.state.replay = ReplayState::SkippingBurst { last_ts_ms: ts_ms };
                    Vec::new()
                } else {
                    event.map(|e| vec![make_entry(e)]).unwrap_or_default()
                }
            }
        }
    }
}

fn make_entry(event: PendingEvent) -> UsageEntry {
    let PendingEvent {
        ts_ms,
        model,
        delta,
        speed,
        speed_inferred,
    } = event;
    // ccusage clamps cached to input; normalized input excludes cache.
    let cached = delta.cached_input_tokens.min(delta.input_tokens);
    UsageEntry {
        entry_key: entry_key(ts_ms, &model, &delta),
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
        speed: Some(speed),
        speed_inferred,
        pricing_shape: PricingShape::Codex,
    }
}

/// The speed a recorded or configured service-tier value maps to (ccusage
/// `codex_service_tier`): exact strings, no case-folding. Unknown values map
/// to `None`.
fn service_tier_speed(value: &str) -> Option<Speed> {
    match value {
        // Both spellings mean non-priority pricing and occur in the same
        // Codex version on the same day; which one is written depends on the
        // client (Codex Desktop writes "standard"), not on the CLI version.
        "default" | "standard" => Some(Speed::Standard),
        "fast" | "priority" => Some(Speed::Fast),
        _ => None,
    }
}

/// Speed fallback implied by a Codex `config.toml`: `Some(Fast)` when any
/// `service_tier` key holds `fast`/`priority`, comment-stripped and
/// quote-trimmed (ccusage `codex_config_requests_fast_service_tier`). A
/// configured `standard` resolves like an absent key — the auto policy
/// already defaults to standard.
pub fn config_fallback_speed(config_toml: &str) -> Option<Speed> {
    config_toml
        .lines()
        .any(|line| {
            let setting = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = setting.split_once('=') else {
                return false;
            };
            key.trim() == "service_tier"
                && service_tier_speed(value.trim().trim_matches(['"', '\''])) == Some(Speed::Fast)
        })
        .then_some(Speed::Fast)
}

/// Content-derived dedup key over the event's full identity (timestamp,
/// model, all counts), matching ccusage's codex dedup key. A fork that
/// replays the parent's events verbatim maps them to the same keys
/// (deduplicated), while distinct turns from files sharing a rollup session
/// never collide. Rewritten bursts (Codex re-stamps replayed history to the
/// fork instant, so keys differ from the parent's) are handled by the replay
/// filter above, not by key dedup.
fn entry_key(ts_ms: i64, model: &str, delta: &CodexTotals) -> String {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{ts_ms}:{model}:{}:{}:{}:{}:{}",
        delta.input_tokens,
        delta.cached_input_tokens,
        delta.output_tokens,
        delta.reasoning_output_tokens,
        delta.total_tokens
    );
    format!("codex:{:x}", Sha256::digest(identity.as_bytes()))[..22].to_string()
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
    #[serde(alias = "modelId")]
    model_id: Option<String>,
    metadata: Option<RawMetadata>,
    // thread_settings_applied fields:
    thread_settings: Option<RawThreadSettings>,
    // session_meta fields:
    id: Option<String>,
    forked_from_id: Option<String>,
    source: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawThreadSettings {
    service_tier: Option<String>,
}

#[derive(Deserialize)]
struct RawInfo {
    total_token_usage: Option<CodexTotals>,
    last_token_usage: Option<CodexTotals>,
    model: Option<String>,
    model_name: Option<String>,
    #[serde(alias = "modelId")]
    model_id: Option<String>,
    metadata: Option<RawMetadata>,
}

#[derive(Deserialize)]
struct RawMetadata {
    model: Option<String>,
}

/// Fork markers from ccusage `read_codex_session_metadata`. A session that
/// lists itself as its own parent is not a fork (ccusage guards this too: it
/// would match the whole stream and drop every event).
fn is_forked_session(payload: &RawPayload) -> bool {
    let own_id = payload.id.as_deref();
    let is_parent = |value: Option<&str>| value.is_some_and(|v| !v.is_empty() && Some(v) != own_id);
    is_parent(payload.forked_from_id.as_deref())
        || is_parent(
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
        payload.model_id.as_deref(),
        payload.metadata.as_ref(),
    )
}

fn info_model(info: &RawInfo) -> Option<String> {
    model_from_parts(
        info.model.as_deref(),
        info.model_name.as_deref(),
        info.model_id.as_deref(),
        info.metadata.as_ref(),
    )
}

/// ccusage's model/model_name/metadata.model chain, extended with the
/// model_id/modelId aliases that streams::model_extraction already accepts.
fn model_from_parts(
    model: Option<&str>,
    model_name: Option<&str>,
    model_id: Option<&str>,
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
        .or_else(|| non_empty(model_id))
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

    fn thread_settings_line(service_tier: Option<&str>) -> String {
        let settings = match service_tier {
            Some(tier) => format!(r#"{{"service_tier":"{tier}"}}"#),
            None => "{}".to_string(),
        };
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_settings":{settings}}}}}"#
        )
    }

    #[test]
    fn service_tier_is_sticky_and_maps_all_spellings() {
        // "default"/"standard" and "fast"/"priority" are spelling pairs; the
        // recorded tier applies to every following usage event until changed.
        for (tier, speed) in [
            ("default", Speed::Standard),
            ("standard", Speed::Standard),
            ("fast", Speed::Fast),
            ("priority", Speed::Fast),
        ] {
            let mut e = CodexUsageExtractor::default();
            e.extract_line(&thread_settings_line(Some(tier)));
            let entries = e.extract_line(&token_count_line(
                "2026-01-01T00:00:10Z",
                (100, 40, 50, 10, 150),
            ));
            assert_eq!(entries[0].speed, Some(speed), "tier {tier}");
            assert!(!entries[0].speed_inferred, "tier {tier} is recorded");
        }

        // Sticky across turns, and a later settings event switches it.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&thread_settings_line(Some("fast")));
        let first = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(first[0].speed, Some(Speed::Fast));
        e.extract_line(&thread_settings_line(Some("standard")));
        let second = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(second[0].speed, Some(Speed::Standard));
        assert!(!second[0].speed_inferred);
    }

    #[test]
    fn unknown_tier_resets_but_an_absent_tier_says_nothing() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&thread_settings_line(Some("fast")));
        // No `service_tier` at all (Codex writes such events for auto-review
        // threads): the previous tier stands.
        e.extract_line(&thread_settings_line(None));
        let kept = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(kept[0].speed, Some(Speed::Fast));
        assert!(!kept[0].speed_inferred);

        // A present but unrecognized tier means it changed to something
        // unknown; the stale value must not be inherited.
        e.extract_line(&thread_settings_line(Some("turbo")));
        let reset = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(reset[0].speed, Some(Speed::Standard));
        assert!(reset[0].speed_inferred, "falls back to the default");
    }

    #[test]
    fn fallback_speed_covers_unmarked_entries_only() {
        let mut e = CodexUsageExtractor::default();
        e.set_fallback_speed(Some(Speed::Fast));
        let unmarked = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(unmarked[0].speed, Some(Speed::Fast));
        assert!(unmarked[0].speed_inferred);

        // A recorded tier wins over the config fallback.
        e.extract_line(&thread_settings_line(Some("standard")));
        let recorded = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(recorded[0].speed, Some(Speed::Standard));
        assert!(!recorded[0].speed_inferred);

        // No fallback and no recorded tier: standard, inferred.
        let mut bare = CodexUsageExtractor::default();
        let entries = bare.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(entries[0].speed, Some(Speed::Standard));
        assert!(entries[0].speed_inferred);
    }

    #[test]
    fn config_fallback_detects_explicit_fast_service_tier_values() {
        // ccusage `detects_explicit_fast_service_tier_values`.
        assert_eq!(
            config_fallback_speed(r#"service_tier = "fast""#),
            Some(Speed::Fast)
        );
        assert_eq!(
            config_fallback_speed(r#"service_tier = 'priority' # use higher tier"#),
            Some(Speed::Fast)
        );
    }

    #[test]
    fn config_fallback_ignores_unrelated_or_substring_service_tier_values() {
        // ccusage `ignores_unrelated_or_substring_service_tier_values`; a
        // configured "standard" resolves like an absent key.
        assert_eq!(
            config_fallback_speed(r#"service_tier_override = "fast""#),
            None
        );
        assert_eq!(config_fallback_speed(r#"service_tier = "breakfast""#), None);
        assert_eq!(config_fallback_speed(r#"service_tier = "standard""#), None);
        assert_eq!(config_fallback_speed(""), None);
    }

    #[test]
    fn service_tier_roundtrips_through_persisted_state() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&thread_settings_line(Some("fast")));
        let state = e.state_json().unwrap();

        let mut restored = CodexUsageExtractor::default();
        assert!(restored.restore_state(&state));
        let entries = restored.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 150),
        ));
        assert_eq!(entries[0].speed, Some(Speed::Fast));
        assert!(!entries[0].speed_inferred);
    }

    #[test]
    fn pre_speed_persisted_state_still_restores() {
        // State written before the service-tier fields existed must restore
        // (serde defaults), not reset the cursor.
        let mut e = CodexUsageExtractor::default();
        assert!(e.restore_state(
            r#"{"model":"gpt-5.1","prev_totals":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":150},"replay":{"kind":"done"}}"#
        ));
        let entries = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(entries[0].tokens.input, 100);
        assert_eq!(entries[0].speed, Some(Speed::Standard));
        assert!(entries[0].speed_inferred);
    }

    #[test]
    fn prefilter_matches_relevant_lines() {
        let e = CodexUsageExtractor::default();
        assert!(e.wants_line(&token_count_line("2026-01-01T00:00:00Z", (1, 0, 1, 0, 2))));
        assert!(e.wants_line(&turn_context_line("gpt-5.1")));
        assert!(e.wants_line(r#"{"type":"session_meta","payload":{"id":"x"}}"#));
        assert!(e.wants_line(&thread_settings_line(Some("fast"))));
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
        assert!(first[0].entry_key.starts_with("codex:"));

        let second = e.extract_line(&token_count_line(
            "2026-01-01T00:01:10Z",
            (300, 140, 90, 30, 390),
        ));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].tokens.input, 100); // (300-100) - (140-40)
        assert_eq!(second[0].tokens.cache_read, 100);
        assert_eq!(second[0].tokens.output, 40);
        assert_eq!(second[0].tokens.reasoning_output, Some(20));
        assert_ne!(second[0].entry_key, first[0].entry_key);
    }

    #[test]
    fn entry_keys_are_content_derived_for_cross_file_dedup() {
        // The same event replayed in another file of the same rollup session
        // (fork/subagent) must map to the same key so the database dedups it,
        // while distinct turns never collide (ccusage's identity-based key).
        let line = token_count_line("2026-01-01T00:00:10Z", (100, 40, 50, 10, 150));
        let a = CodexUsageExtractor::default().extract_line(&line);
        let b = CodexUsageExtractor::default().extract_line(&line);
        assert_eq!(a[0].entry_key, b[0].entry_key);

        let other = CodexUsageExtractor::default().extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 51, 10, 151),
        ));
        assert_ne!(a[0].entry_key, other[0].entry_key);
    }

    #[test]
    fn zero_total_is_derived_from_input_plus_output() {
        // ccusage: a recorded zero total is unusable and derives to
        // input + output (reasoning is a subset of output).
        let mut e = CodexUsageExtractor::default();
        let entries = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (100, 40, 50, 10, 0),
        ));
        assert_eq!(entries[0].tokens.total, 150);
    }

    #[test]
    fn accepts_aliased_and_string_encoded_token_fields() {
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"prompt_tokens":"100","cached_tokens":40,"completion_tokens":"50","reasoning_tokens":10}}}}"#;
        let entries = e.extract_line(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tokens.input, 60);
        assert_eq!(entries[0].tokens.cache_read, 40);
        assert_eq!(entries[0].tokens.output, 50);
        assert_eq!(entries[0].tokens.reasoning_output, Some(10));
        assert_eq!(entries[0].tokens.total, 150); // derived: no total recorded
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
    fn flush_releases_a_single_turn_forked_session_after_the_burst_window() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .is_empty()
        );
        assert!(e.has_pending());
        // Within the burst window nothing is released yet.
        let ts_ms = 1_767_225_601_000_i64;
        assert!(e.flush(ts_ms + 500).is_empty());
        assert!(e.has_pending());
        // Past the window no later event can join a burst: the buffered turn
        // is real usage.
        let flushed = e.flush(ts_ms + 1_500);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].tokens.input, 10);
        assert!(!e.has_pending());
        assert!(e.flush(ts_ms + 2_000).is_empty());
        // The session's own next turn (necessarily past the window the flush
        // waited out) counts normally.
        let own = e.extract_line(&token_count_line(
            "2026-01-01T00:00:05Z",
            (30, 0, 15, 0, 45),
        ));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].tokens.input, 20);
    }

    #[test]
    fn late_burst_partners_after_a_flush_release_are_skipped() {
        // The split-burst misfire: a pass's flush releases the parked replay
        // because >1s of wall clock passed, but Codex then writes the rest of
        // the rewritten burst (recorded timestamps still sub-second apart).
        // The flush leaves the skip machine armed at the released event's
        // timestamp, so the late partners are skipped — the over-count is
        // bounded to the one released event, not the whole replayed history.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .is_empty()
        );
        let flushed = e.flush(1_767_225_601_000 + 1_500);
        assert_eq!(flushed.len(), 1, "parked replay released (the misfire)");
        // Burst partners 2..N, chained within the window of the release.
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.500Z",
                (20, 0, 10, 0, 30)
            ))
            .is_empty()
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:02.200Z",
                (30, 0, 15, 0, 45)
            ))
            .is_empty()
        );
        // The child's own first turn after a real pause counts, with the
        // skipped burst absorbed into the cumulative baseline.
        let own = e.extract_line(&token_count_line(
            "2026-01-01T00:00:10Z",
            (50, 0, 25, 0, 75),
        ));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].tokens.input, 20);
        assert_eq!(own[0].tokens.total, 30);
    }

    #[test]
    fn session_meta_and_aliased_model_fields_resolve_the_model() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"s","model_id":"gpt-5.3"}}"#,
        );
        let entries = e.extract_line(&token_count_line("2026-01-01T00:00:10Z", (1, 0, 1, 0, 2)));
        assert_eq!(entries[0].model, "gpt-5.3");

        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"modelId":"gpt-5.4"}}"#,
        );
        let entries = e.extract_line(&token_count_line("2026-01-01T00:00:10Z", (1, 0, 1, 0, 2)));
        assert_eq!(entries[0].model, "gpt-5.4");
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
                assert!(e.restore_state(json));
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
        assert!(!e.restore_state("{not json"));
        assert!(e.state.prev_totals.is_none());
        assert!(matches!(e.state.replay, ReplayState::Done));
    }

    #[test]
    fn zero_delta_repeats_still_anchor_the_rewritten_burst() {
        // ccusage's burst detection scans raw usage events, so a cumulative
        // repeat (zero delta) 100ms after the first replayed event still
        // marks the pair as a burst and the first event is skipped.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.000Z",
                (10, 0, 5, 0, 15)
            ))
            .is_empty()
        );
        // Exact repeat of the cumulative totals: no delta, but real activity.
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.100Z",
                (10, 0, 5, 0, 15)
            ))
            .is_empty()
        );
        // The child's own first turn after a real pause is the only usage.
        let own = e.extract_line(&token_count_line(
            "2026-01-01T00:00:20Z",
            (30, 0, 15, 0, 45),
        ));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].tokens.input, 20);
    }

    #[test]
    fn zero_delta_repeats_do_not_extend_an_active_burst() {
        // ccusage's skip machine never sees zero-delta repeats, so they must
        // not bridge the burst window across the child's real first turn.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        // Burst: two delta events back to back.
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.000Z",
                (10, 0, 5, 0, 15)
            ))
            .is_empty()
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.100Z",
                (20, 0, 10, 0, 30)
            ))
            .is_empty()
        );
        // Zero-delta heartbeat 0.9s later: discarded, but must not extend.
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:02.000Z",
                (20, 0, 10, 0, 30)
            ))
            .is_empty()
        );
        // Child's real first turn 1.7s after the last DELTA event (but only
        // 0.8s after the heartbeat): counted, matching upstream.
        let own = e.extract_line(&token_count_line(
            "2026-01-01T00:00:02.800Z",
            (30, 0, 15, 0, 45),
        ));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].tokens.input, 10);
    }

    #[test]
    fn repeated_last_usage_with_unchanged_total_is_skipped() {
        // Codex duplicates final snapshots on close/compaction: a non-zero
        // last_token_usage alongside an UNCHANGED cumulative total must not
        // re-count the final turn (ccusage
        // `skips_repeated_last_usage_when_cumulative_total_is_unchanged`).
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":150},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":150}}}}"#;
        assert_eq!(e.extract_line(line).len(), 1);
        // Exact duplicate snapshot: cumulative unchanged, last repeated.
        assert!(e.extract_line(line).is_empty());
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:05:00Z",
                (100, 0, 50, 0, 150)
            ))
            .is_empty()
        );
    }

    #[test]
    fn hostile_shapes_skip_the_line_but_preserve_parser_state() {
        let mut e = CodexUsageExtractor::default();
        e.extract_line(&turn_context_line("gpt-5.1"));
        assert_eq!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:10Z",
                (100, 0, 50, 0, 150)
            ))
            .len(),
            1
        );

        // Unexpected shapes: scalar payload, array info, scalar metadata,
        // array-valued last usage. Each line is skipped whole (documented
        // deviation) without corrupting model or cumulative state.
        for hostile in [
            r#"{"timestamp":"2026-01-01T00:00:11Z","type":"event_msg","payload":"token_count"}"#,
            r#"{"timestamp":"2026-01-01T00:00:12Z","type":"event_msg","payload":{"type":"token_count","info":[1,2,3]}}"#,
            r#"{"timestamp":"2026-01-01T00:00:13Z","type":"event_msg","payload":{"type":"token_count","metadata":"auto","info":{"total_token_usage":{"input_tokens":[1],"output_tokens":50}}}}"#,
            r#"{"timestamp":"2026-01-01T00:00:14Z","type":"turn_context","payload":"gpt-oops"}"#,
        ] {
            assert!(e.extract_line(hostile).is_empty(), "line: {hostile}");
        }

        // The next good event still deltas from the last good totals under
        // the sticky model.
        let next = e.extract_line(&token_count_line(
            "2026-01-01T00:01:00Z",
            (150, 0, 70, 0, 220),
        ));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].tokens.input, 50);
        assert_eq!(next[0].tokens.output, 20);
        assert_eq!(next[0].model, "gpt-5.1");
    }

    #[test]
    fn float_and_string_counts_are_tolerated_per_field() {
        // Wrong-typed individual counts fall back to 0 (lossy per-field, like
        // ccusage) instead of failing the whole line.
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12.5,"cached_input_tokens":"4","output_tokens":50,"reasoning_output_tokens":null,"total_tokens":0}}}}"#;
        let entries = e.extract_line(line);
        assert_eq!(entries.len(), 1);
        // 12.5 is not a u64: lossy -> 0; cached clamps to input (0).
        assert_eq!(entries[0].tokens.input, 0);
        assert_eq!(entries[0].tokens.cache_read, 0);
        assert_eq!(entries[0].tokens.output, 50);
        assert_eq!(entries[0].tokens.total, 50); // derived: input + output
    }

    #[test]
    fn fork_states_roundtrip_through_persisted_state() {
        // AwaitingSecond-with-pending survives a state save/restore (the
        // production shape: park in one pass, decide in a later one).
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .is_empty()
        );
        assert!(e.has_pending());
        let saved = e.state_json().unwrap();

        let mut resumed = CodexUsageExtractor::default();
        assert!(resumed.restore_state(&saved));
        assert!(resumed.has_pending());
        // A burst-confirming second event in the next pass still discards
        // both...
        assert!(
            resumed
                .extract_line(&token_count_line(
                    "2026-01-01T00:00:01.500Z",
                    (20, 0, 10, 0, 30)
                ))
                .is_empty()
        );
        assert!(matches!(
            resumed.state.replay,
            ReplayState::SkippingBurst { .. }
        ));
        // ...and SkippingBurst also survives persistence.
        let saved = resumed.state_json().unwrap();
        let mut resumed = CodexUsageExtractor::default();
        assert!(resumed.restore_state(&saved));
        let own = resumed.extract_line(&token_count_line(
            "2026-01-01T00:00:20Z",
            (30, 0, 15, 0, 45),
        ));
        assert_eq!(own.len(), 1);

        // The alternative branch: a wall-clock flush in a later pass
        // releases a parked single turn.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)));
        let saved = e.state_json().unwrap();
        let mut resumed = CodexUsageExtractor::default();
        assert!(resumed.restore_state(&saved));
        let flushed = resumed.flush(1_767_225_601_000 + 5_000);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].tokens.input, 10);
    }

    #[test]
    fn burst_window_has_millisecond_precision() {
        // 00.986 -> 01.009 is 23ms apart: inside the window even though the
        // integer seconds differ (a seconds-truncating parser would
        // misclassify real turns straddling a second boundary).
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}"#,
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:00.986Z",
                (10, 0, 5, 0, 15)
            ))
            .is_empty()
        );
        assert!(
            e.extract_line(&token_count_line(
                "2026-01-01T00:00:01.009Z",
                (20, 0, 10, 0, 30)
            ))
            .is_empty(),
            "23ms apart must be a burst"
        );
    }

    #[test]
    fn string_encoded_near_max_counts_saturate() {
        let mut e = CodexUsageExtractor::default();
        let max = u64::MAX;
        let line = format!(
            r#"{{"timestamp":"2026-01-01T00:00:10Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":"{max}","cached_input_tokens":0,"output_tokens":"{max}","reasoning_output_tokens":0,"total_tokens":0}}}}}}}}"#
        );
        let entries = e.extract_line(&line);
        assert_eq!(entries.len(), 1);
        // The derived total saturates instead of wrapping.
        assert_eq!(entries[0].tokens.total, u64::MAX);
    }

    #[test]
    fn self_parent_fork_is_not_a_fork() {
        // ccusage guards a session listing itself as its own parent: treating
        // it as a fork would burst-skip its real first turns.
        let mut e = CodexUsageExtractor::default();
        e.extract_line(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"same","forked_from_id":"same"}}"#,
        );
        assert!(matches!(e.state.replay, ReplayState::Done));
        assert_eq!(
            e.extract_line(&token_count_line("2026-01-01T00:00:01Z", (10, 0, 5, 0, 15)))
                .len(),
            1
        );
    }

    #[test]
    fn real_codex_fixture_shapes_parse() {
        // Conformance over a captured rollout: session_meta/turn_context in
        // their real shapes must resolve the model and not misfire the fork
        // detector (no forked_from_id in this session).
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-session-updated.jsonl");
        let content = std::fs::read_to_string(fixture).unwrap();
        let mut e = CodexUsageExtractor::default();
        for line in content.lines() {
            if e.wants_line(line) {
                assert!(e.extract_line(line).is_empty()); // no token_count events
            }
        }
        assert!(matches!(e.state.replay, ReplayState::Done));
        assert!(e.state.model.is_some(), "model from real turn_context");
        // Usage arriving after the real prelude prices under that model.
        let entries = e.extract_line(&token_count_line("2026-02-11T05:54:00Z", (10, 0, 5, 0, 15)));
        assert_eq!(entries.len(), 1);
        assert_ne!(entries[0].model, FALLBACK_MODEL);
    }

    #[test]
    fn golden_codex_token_count_extraction_is_pinned() {
        // Golden extraction over real captured token_count lines (sanitized
        // rollout excerpt): the serde shape, delta math, model resolution,
        // and dedup keys are all pinned against production Codex output —
        // a field rename or type change upstream fails here, not silently.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-session-token-count.jsonl");
        let content = std::fs::read_to_string(fixture).unwrap();
        let mut e = CodexUsageExtractor::default();
        let mut entries = Vec::new();
        for line in content.lines() {
            if e.wants_line(line) {
                entries.extend(e.extract_line(line));
            }
        }
        let mut sums = (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut models = std::collections::BTreeSet::new();
        let mut keys = std::collections::BTreeSet::new();
        for entry in &entries {
            sums.0 += entry.tokens.input;
            sums.1 += entry.tokens.output;
            sums.2 += entry.tokens.cache_read;
            sums.3 += entry.tokens.reasoning_output.unwrap();
            sums.4 += entry.tokens.total;
            models.insert(entry.model.clone());
            keys.insert(entry.entry_key.clone());
        }
        assert_eq!(keys.len(), entries.len(), "content-derived keys distinct");
        insta::assert_debug_snapshot!((entries.len(), models, sums));
    }

    #[test]
    fn numeric_timestamps_are_supported() {
        let mut e = CodexUsageExtractor::default();
        let line = r#"{"timestamp":1767225610,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}}}}"#;
        let entries = e.extract_line(line);
        assert_eq!(entries[0].ts, 1_767_225_610);
    }
}
