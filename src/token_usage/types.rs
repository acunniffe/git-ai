//! Shared types for token-usage extraction.

use serde::{Deserialize, Serialize};

/// Width of a token-usage bucket: 5 minutes, UTC-aligned.
pub const BUCKET_SECS: u32 = 300;

/// Floor a unix timestamp (seconds) to the start of its 5-minute UTC bucket.
pub fn bucket_ts(ts: u32) -> u32 {
    ts - ts % BUCKET_SECS
}

/// Normalized token counts for a single usage entry, following ccusage's
/// Claude-shaped normalization: `input` excludes cached tokens, which are
/// reported as `cache_read`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Reasoning tokens reported separately by some agents (Codex). A subset
    /// of `output`, not an additional amount.
    pub reasoning_output: Option<u64>,
    pub total: u64,
}

/// Request speed / service tier (ccusage `Speed`): fast/priority requests
/// bill at the model's fast multiplier over the whole request cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    #[default]
    Standard,
    Fast,
}

/// Which of ccusage's two cost formulas prices an entry — the producing
/// extractor's shape, not the model's (the paths differ in how the
/// long-context tier is selected and how unpublished cache-read rates
/// default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingShape {
    Claude,
    Codex,
}

/// One usage entry extracted from a transcript, before deduplication.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEntry {
    /// Dedup key, unique within a session (see the per-agent extractors).
    pub entry_key: String,
    /// Assistant message id when the source format has one (Claude). Used for
    /// the message-id-only dedup fallback on sidechain replays.
    pub message_id: Option<String>,
    /// Event timestamp, unix seconds (UTC).
    pub ts: u32,
    pub model: String,
    pub tokens: TokenCounts,
    /// Portion of `tokens.cache_write` written with a 1-hour TTL (Claude
    /// only); priced at 2x the input rate, matching ccusage.
    pub cache_write_1h: u64,
    /// Pre-computed cost from the transcript's own `costUSD` field, in
    /// micro-USD. Takes precedence over computed pricing (ccusage "auto"
    /// mode).
    pub transcript_cost_micro_usd: Option<u64>,
    /// Claude sidechain (subagent) entry; loses to non-sidechain duplicates.
    pub is_sidechain: bool,
    /// Request speed. `None` when the transcript carried no marker (Claude
    /// without `usage.speed`); a present marker wins ties on replacement.
    pub speed: Option<Speed>,
    /// The speed was not recorded in the transcript but resolved from
    /// configuration or the standard default (unmarked entries of any tool).
    pub speed_inferred: bool,
    /// Cost formula for this entry (see [`PricingShape`]).
    pub pricing_shape: PricingShape,
}

impl UsageEntry {
    /// Token total compared by the Claude replacement policy (ccusage
    /// `usage_token_total`).
    pub fn dedupe_token_total(&self) -> u64 {
        self.tokens
            .input
            .saturating_add(self.tokens.output)
            .saturating_add(self.tokens.cache_write)
            .saturating_add(self.tokens.cache_read)
    }
}

/// Parse an RFC3339 timestamp to unix seconds, rejecting pre-epoch values.
pub(crate) fn parse_rfc3339_secs(value: &str) -> Option<u32> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|dt| u32::try_from(dt.timestamp()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_ts_floors_to_five_minute_boundaries() {
        assert_eq!(bucket_ts(0), 0);
        assert_eq!(bucket_ts(299), 0);
        assert_eq!(bucket_ts(300), 300);
        assert_eq!(bucket_ts(1_700_000_123), 1_700_000_100);
    }

    #[test]
    fn parses_rfc3339_timestamps() {
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T00:00:00.000Z"),
            Some(1_767_225_600)
        );
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T01:00:00+01:00"),
            Some(1_767_225_600)
        );
        assert_eq!(parse_rfc3339_secs("1969-12-31T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_secs("not-a-timestamp"), None);
    }
}
