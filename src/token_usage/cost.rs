//! Per-entry cost estimation (ccusage "auto" mode: a pre-computed transcript
//! cost wins, otherwise cost is derived from the pricing catalog).
//!
//! Known deviations from ccusage, all stemming from pricing exclusively via
//! git-ai's models.dev catalog: no long-context tiered rates, no fast-speed
//! multipliers, and no `codex-auto-review` release-date model mapping.
//! `git-ai usage` (src/metrics/local_stats.rs) estimates from the same
//! catalog but without the 1h-cache multiplier or cache-read fallback (it
//! aggregates without the 5m/1h split); TokenUsage events are the
//! authoritative figures.

use super::types::UsageEntry;
use crate::metrics::model_pricing::pricing_for;

/// 1-hour ephemeral cache writes are priced at 2x the input rate (ccusage
/// `CACHE_CREATE_1H_INPUT_MULTIPLIER`); the flat `cache_write` rate covers
/// the 5-minute TTL.
const CACHE_WRITE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Catalog entries that omit a cache-read rate deserialize as 0.0; ccusage
/// defaults missing cache-read pricing to a tenth of the input rate, and
/// cache reads are the dominant Codex token class, so $0 would badly
/// undercount.
const CACHE_READ_INPUT_RATE_FALLBACK: f64 = 0.1;

/// Sanity ceiling for a single entry's cost: $10,000. Transcript `costUSD`
/// is attacker/corruption-controlled input; one garbled line must not
/// inflate a bucket by millions of dollars.
const MAX_ENTRY_COST_MICRO_USD: u64 = 10_000 * 1_000_000;

/// Estimated cost of one entry in micro-USD: the transcript's own `costUSD`
/// when present, otherwise computed from the models.dev pricing catalog.
/// `None` when the model has no known pricing.
pub fn entry_cost_micro_usd(entry: &UsageEntry) -> Option<u64> {
    if let Some(cost) = entry.transcript_cost_micro_usd {
        return Some(cost);
    }
    Some(cost_from_pricing(entry, pricing_for(&entry.model)?))
}

/// The catalog-pricing arm of ccusage's "auto" mode, split out so the rate
/// fallbacks are unit-testable with synthetic pricing.
fn cost_from_pricing(
    entry: &UsageEntry,
    pricing: &crate::metrics::model_pricing::ModelPricing,
) -> u64 {
    let cache_write_5m = entry
        .tokens
        .cache_write
        .saturating_sub(entry.cache_write_1h);
    let cache_read_rate = if pricing.cache_read > 0.0 {
        pricing.cache_read
    } else {
        pricing.input * CACHE_READ_INPUT_RATE_FALLBACK
    };
    let usd = (entry.tokens.input as f64 * pricing.input
        + entry.tokens.output as f64 * pricing.output
        + cache_write_5m as f64 * pricing.cache_write
        + entry.cache_write_1h as f64 * pricing.input * CACHE_WRITE_1H_INPUT_MULTIPLIER
        + entry.tokens.cache_read as f64 * cache_read_rate)
        / 1_000_000.0;
    micro_usd(usd)
}

/// Convert a USD amount to micro-USD (1e-6 USD), rounding to nearest and
/// clamping to the per-entry sanity ceiling. Non-finite and negative inputs
/// are worth nothing rather than something enormous.
pub fn micro_usd(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    ((usd * 1_000_000.0).round() as u64).min(MAX_ENTRY_COST_MICRO_USD)
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
    fn missing_cache_read_rate_falls_back_to_a_tenth_of_input() {
        use crate::metrics::model_pricing::ModelPricing;
        let mut e = entry(
            "any-model",
            TokenCounts {
                cache_read: 1_000_000,
                ..Default::default()
            },
        );
        e.transcript_cost_micro_usd = None;
        let no_cache_rate = ModelPricing {
            input: 2.0,
            output: 10.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        // ccusage defaults missing cache-read pricing to input * 0.1; $0
        // would badly undercount cache-heavy sessions.
        assert_eq!(cost_from_pricing(&e, &no_cache_rate), micro_usd(0.2));
        let explicit = ModelPricing {
            cache_read: 0.5,
            ..no_cache_rate
        };
        assert_eq!(cost_from_pricing(&e, &explicit), micro_usd(0.5));
    }

    #[test]
    fn micro_usd_rejects_garbage_and_clamps_absurd_costs() {
        assert_eq!(micro_usd(-1.0), 0);
        assert_eq!(micro_usd(f64::NAN), 0);
        assert_eq!(micro_usd(f64::INFINITY), 0);
        // One corrupt costUSD line must not inflate a bucket by millions.
        assert_eq!(micro_usd(1e15), MAX_ENTRY_COST_MICRO_USD);
        assert_eq!(micro_usd(1.25), 1_250_000);
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
