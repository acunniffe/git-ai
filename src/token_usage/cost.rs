//! Per-entry cost estimation (ccusage "auto" mode: a pre-computed transcript
//! cost wins, otherwise cost is derived from the pricing catalog).

use super::types::UsageEntry;
use crate::metrics::model_pricing::pricing_for;

/// 1-hour ephemeral cache writes are priced at 2x the input rate (ccusage
/// `CACHE_CREATE_1H_INPUT_MULTIPLIER`); the flat `cache_write` rate covers
/// the 5-minute TTL.
const CACHE_WRITE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Estimated cost of one entry in micro-USD: the transcript's own `costUSD`
/// when present, otherwise computed from the models.dev pricing catalog.
/// `None` when the model has no known pricing.
pub fn entry_cost_micro_usd(entry: &UsageEntry) -> Option<u64> {
    if let Some(cost) = entry.transcript_cost_micro_usd {
        return Some(cost);
    }
    let pricing = pricing_for(&entry.model)?;
    let cache_write_5m = entry
        .tokens
        .cache_write
        .saturating_sub(entry.cache_write_1h);
    let usd = (entry.tokens.input as f64 * pricing.input
        + entry.tokens.output as f64 * pricing.output
        + cache_write_5m as f64 * pricing.cache_write
        + entry.cache_write_1h as f64 * pricing.input * CACHE_WRITE_1H_INPUT_MULTIPLIER
        + entry.tokens.cache_read as f64 * pricing.cache_read)
        / 1_000_000.0;
    Some(micro_usd(usd))
}

/// Convert a USD amount to micro-USD (1e-6 USD), rounding to nearest.
pub fn micro_usd(usd: f64) -> u64 {
    (usd * 1_000_000.0).round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_usage::types::TokenCounts;

    fn entry(model: &str, tokens: TokenCounts) -> UsageEntry {
        UsageEntry {
            entry_key: "k".to_string(),
            message_id: None,
            ts: 0,
            model: model.to_string(),
            tokens,
            cache_write_1h: 0,
            transcript_cost_micro_usd: None,
            is_sidechain: false,
            has_speed: false,
        }
    }

    #[test]
    fn transcript_cost_takes_precedence_over_computed() {
        let mut e = entry(
            "claude-sonnet-4-20250514",
            TokenCounts {
                input: 1_000_000,
                ..Default::default()
            },
        );
        e.transcript_cost_micro_usd = Some(123);
        assert_eq!(entry_cost_micro_usd(&e), Some(123));
    }

    #[test]
    fn computes_cost_from_pricing_catalog() {
        // claude-sonnet-4 is in the embedded models.dev snapshot.
        let pricing = pricing_for("claude-sonnet-4-20250514").expect("snapshot pricing");
        let e = entry(
            "claude-sonnet-4-20250514",
            TokenCounts {
                input: 1_000_000,
                output: 2_000_000,
                cache_read: 500_000,
                cache_write: 250_000,
                reasoning_output: None,
                total: 3_750_000,
            },
        );
        let expected = micro_usd(
            pricing.input
                + 2.0 * pricing.output
                + 0.5 * pricing.cache_read
                + 0.25 * pricing.cache_write,
        );
        assert_eq!(entry_cost_micro_usd(&e), Some(expected));
    }

    #[test]
    fn one_hour_cache_writes_cost_double_the_input_rate() {
        let pricing = pricing_for("claude-sonnet-4-20250514").expect("snapshot pricing");
        let mut e = entry(
            "claude-sonnet-4-20250514",
            TokenCounts {
                cache_write: 1_000_000,
                ..Default::default()
            },
        );
        e.cache_write_1h = 400_000;
        let expected = micro_usd(0.6 * pricing.cache_write + 0.4 * pricing.input * 2.0);
        assert_eq!(entry_cost_micro_usd(&e), Some(expected));
    }

    #[test]
    fn unknown_model_has_no_cost() {
        let e = entry(
            "totally-unknown-model-xyz",
            TokenCounts {
                input: 100,
                ..Default::default()
            },
        );
        assert_eq!(entry_cost_micro_usd(&e), None);
    }
}
