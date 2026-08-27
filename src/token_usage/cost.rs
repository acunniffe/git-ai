//! Per-entry cost estimation (ccusage "auto" mode: a pre-computed transcript
//! cost wins, otherwise cost is derived from the pricing catalog with the
//! per-shape long-context tiering and fast-multiplier rules below).
//!
//! Known deviations from ccusage: no `codex-auto-review` release-date model
//! mapping, and no marginal-at-200K pricing arm (ccusage keeps LiteLLM
//! `above_200k` data marginal when a model has above-rates but no threshold;
//! git-ai's models.dev catalog always pairs above-rates with a threshold, so
//! that arm is unreachable). `git-ai usage` (src/metrics/local_stats.rs)
//! estimates from the same catalog but without tiering, multipliers, or the
//! 1h-cache split; TokenUsage events are the authoritative figures.

use super::types::{PricingShape, Speed, UsageEntry};
use crate::metrics::model_pricing::{ModelPricing, pricing_catalog_id, pricing_for};

/// 1-hour ephemeral cache writes are priced at 2x the input rate (ccusage
/// `CACHE_CREATE_1H_INPUT_MULTIPLIER`); the flat `cache_write` rate covers
/// the 5-minute TTL.
const CACHE_WRITE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Sanity ceiling for a single entry's cost: $10,000. Transcript `costUSD`
/// is attacker/corruption-controlled input; one garbled line must not
/// inflate a bucket by millions of dollars.
const MAX_ENTRY_COST_MICRO_USD: u64 = 10_000 * 1_000_000;

/// Everything the database stores about an entry's pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPricing {
    /// `None` when the model has no known pricing (and no transcript cost).
    pub cost_micro_usd: Option<u64>,
    /// The request selected its model's long-context tier (recorded even for
    /// transcript-priced entries: the wire's tier token splits cover every
    /// token, whatever priced it).
    pub long_context: bool,
    /// Catalog in effect when the cost was computed; `None` when the cost
    /// came from the transcript or the model has no pricing.
    pub pricing_catalog: Option<&'static str>,
}

/// Price one entry (ccusage "auto" mode): the transcript's own `costUSD`
/// wins, otherwise the catalog formula for the entry's [`PricingShape`],
/// with fast-speed entries billing the whole request at the model's fast
/// multiplier.
pub fn price_entry(entry: &UsageEntry) -> EntryPricing {
    let pricing = pricing_for(&entry.model);
    let long_context = pricing.is_some_and(|pricing| is_long_context(entry, &pricing));
    if let Some(cost) = entry.transcript_cost_micro_usd {
        return EntryPricing {
            cost_micro_usd: Some(cost),
            long_context,
            pricing_catalog: None,
        };
    }
    let Some(pricing) = pricing else {
        return EntryPricing {
            cost_micro_usd: None,
            long_context,
            pricing_catalog: None,
        };
    };
    EntryPricing {
        // The same tier decision selects the billed rates AND lands in the
        // recorded long_context column the wire splits are built from, so
        // the two can never diverge.
        cost_micro_usd: Some(tiered_cost_from_pricing(entry, &pricing, long_context)),
        long_context,
        pricing_catalog: Some(pricing_catalog_id()),
    }
}

/// Estimated cost of one entry in micro-USD ([`price_entry`]'s cost alone).
pub fn entry_cost_micro_usd(entry: &UsageEntry) -> Option<u64> {
    price_entry(entry).cost_micro_usd
}

/// Whether the request bills at its model's long-context tier: the whole
/// request switches once its context exceeds the threshold (strict; not a
/// marginal breakpoint). The context sum is per shape: Claude counts the
/// full input side — uncached input, cache reads, and cache writes (ccusage
/// `calculate_cost_from_pricing`) — while Codex compares the raw per-turn
/// input, which is uncached input plus cache reads in git-ai's normalized
/// counts (ccusage's codex aggregation splits on `event.input_tokens`;
/// output never selects the tier).
fn is_long_context(entry: &UsageEntry, pricing: &ModelPricing) -> bool {
    let Some(threshold) = pricing.long_context_threshold else {
        return false;
    };
    let context = entry
        .tokens
        .input
        .saturating_add(entry.tokens.cache_read)
        .saturating_add(match entry.pricing_shape {
            PricingShape::Claude => entry.tokens.cache_write,
            PricingShape::Codex => 0,
        });
    context > threshold
}

/// The catalog-pricing arm of ccusage's "auto" mode, split out so the rate
/// fallbacks are unit-testable with synthetic pricing (computes the tier
/// decision itself; production goes through [`price_entry`], which computes
/// it once for both the cost and the recorded flag).
#[cfg(test)]
fn cost_from_pricing(entry: &UsageEntry, pricing: &ModelPricing) -> u64 {
    tiered_cost_from_pricing(entry, pricing, is_long_context(entry, pricing))
}

fn tiered_cost_from_pricing(entry: &UsageEntry, pricing: &ModelPricing, long_context: bool) -> u64 {
    let usd = match entry.pricing_shape {
        PricingShape::Claude => claude_cost_usd(entry, pricing, long_context),
        PricingShape::Codex => codex_cost_usd(entry, pricing, long_context),
    } / 1_000_000.0;
    // Fast-speed requests bill the whole request at the model's multiplier
    // (ccusage prices the fast bucket once at standard rates plus
    // `cost * (multiplier - 1)` on top — the same thing per entry).
    let multiplier = if entry.speed == Some(Speed::Fast) {
        pricing.fast_multiplier
    } else {
        1.0
    };
    micro_usd(usd * multiplier)
}

/// ccusage `calculate_cost_from_pricing`: in a long-context request every
/// rate switches to its above-threshold value, falling back per rate to the
/// base; 1h cache writes bill at 2x the selected tier's input rate.
fn claude_cost_usd(entry: &UsageEntry, pricing: &ModelPricing, long_context: bool) -> f64 {
    let cache_write_5m = entry
        .tokens
        .cache_write
        .saturating_sub(entry.cache_write_1h);
    let rate = |base: f64, above: Option<f64>| {
        if long_context {
            above.unwrap_or(base)
        } else {
            base
        }
    };
    entry.tokens.input as f64 * rate(pricing.input, pricing.input_above)
        + entry.tokens.output as f64 * rate(pricing.output, pricing.output_above)
        + cache_write_5m as f64 * rate(pricing.cache_write_rate(), pricing.cache_write_above)
        + entry.cache_write_1h as f64
            * rate(
                pricing.input * CACHE_WRITE_1H_INPUT_MULTIPLIER,
                pricing
                    .input_above
                    .map(|input| input * CACHE_WRITE_1H_INPUT_MULTIPLIER),
            )
        + entry.tokens.cache_read as f64 * rate(pricing.cache_read_rate(), pricing.cache_read_above)
}

/// ccusage `calculate_codex_bucket_cost`, per entry (a request is entirely
/// long- or short-context, so the two-bucket split collapses): an
/// unpublished cache-read rate bills at the FULL input rate of the selected
/// tier — not the Claude path's 0.1x default — and Codex reports no cache
/// writes, so there are no write terms.
fn codex_cost_usd(entry: &UsageEntry, pricing: &ModelPricing, long_context: bool) -> f64 {
    let explicit = pricing.cache_read.is_some();
    let (input_rate, output_rate, cache_read_rate) = if long_context {
        let long_input = pricing.input_above.unwrap_or(pricing.input);
        let long_cache_read = if explicit {
            pricing
                .cache_read_above
                .unwrap_or_else(|| pricing.cache_read_rate())
        } else {
            long_input
        };
        (
            long_input,
            pricing.output_above.unwrap_or(pricing.output),
            long_cache_read,
        )
    } else {
        let cache_read = if explicit {
            pricing.cache_read_rate()
        } else {
            pricing.input
        };
        (pricing.input, pricing.output, cache_read)
    };
    entry.tokens.input as f64 * input_rate
        + entry.tokens.cache_read as f64 * cache_read_rate
        + entry.tokens.output as f64 * output_rate
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
            speed: None,
            speed_inferred: false,
            pricing_shape: PricingShape::Claude,
        }
    }

    fn codex_entry(model: &str, tokens: TokenCounts) -> UsageEntry {
        UsageEntry {
            pricing_shape: PricingShape::Codex,
            ..entry(model, tokens)
        }
    }

    /// Synthetic two-stage pricing: 200K threshold, above-rates double the
    /// base, published cache rates.
    fn tiered_pricing() -> ModelPricing {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: Some(3.75),
            long_context_threshold: Some(200_000),
            input_above: Some(6.0),
            output_above: Some(30.0),
            cache_read_above: Some(0.6),
            cache_write_above: Some(7.5),
            ..Default::default()
        }
    }

    #[test]
    fn transcript_cost_takes_precedence_over_computed() {
        let mut e = entry(
            "claude-sonnet-4-6-20260115",
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
        let pricing = pricing_for("claude-sonnet-4-6-20260115").expect("snapshot pricing");
        let e = entry(
            "claude-sonnet-4-6-20260115",
            TokenCounts {
                input: 1_000_000,
                output: 2_000_000,
                cache_read: 500_000,
                cache_write: 250_000,
                reasoning_output: None,
                total: 3_750_000,
            },
        );
        // 3.75M of context crosses the sonnet 200K threshold, so every rate
        // is the above-threshold one.
        let expected = micro_usd(
            pricing.input_above.unwrap()
                + 2.0 * pricing.output_above.unwrap()
                + 0.5 * pricing.cache_read_above.unwrap()
                + 0.25 * pricing.cache_write_above.unwrap(),
        );
        assert_eq!(entry_cost_micro_usd(&e), Some(expected));
    }

    #[test]
    fn one_hour_cache_writes_cost_double_the_input_rate() {
        let pricing = pricing_for("claude-sonnet-4-6-20260115").expect("snapshot pricing");
        let mut e = entry(
            "claude-sonnet-4-6-20260115",
            TokenCounts {
                cache_write: 1_000_000,
                ..Default::default()
            },
        );
        e.cache_write_1h = 400_000;
        // 1M of cache writes is long-context for the 200K-threshold sonnet:
        // 1h writes bill at 2x the above-threshold input rate.
        let expected = micro_usd(
            0.6 * pricing.cache_write_above.unwrap() + 0.4 * pricing.input_above.unwrap() * 2.0,
        );
        assert_eq!(entry_cost_micro_usd(&e), Some(expected));
    }

    #[test]
    fn long_context_requests_bill_every_token_at_the_above_rates() {
        // Whole-request tier selection (ccusage
        // `prices_two_stage_model_as_whole_request_at_long_context_rates`):
        // one token over the threshold flips every bucket, not just the
        // marginal tokens.
        let pricing = tiered_pricing();
        let below = entry(
            "m",
            TokenCounts {
                input: 100_000,
                output: 1_000,
                cache_read: 50_000,
                cache_write: 50_000,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&below, &pricing),
            micro_usd(0.1 * 3.0 + 0.001 * 15.0 + 0.05 * 0.3 + 0.05 * 3.75)
        );

        let above = entry(
            "m",
            TokenCounts {
                input: 100_001,
                output: 1_000,
                cache_read: 50_000,
                cache_write: 50_000,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&above, &pricing),
            micro_usd(0.100_001 * 6.0 + 0.001 * 30.0 + 0.05 * 0.6 + 0.05 * 7.5)
        );
    }

    #[test]
    fn cached_context_selects_the_long_context_tier() {
        // The vendor's tier is chosen by the request's whole context, cached
        // or not (ccusage `cached_context_selects_the_long_context_tier`).
        let pricing = tiered_pricing();
        let e = entry(
            "m",
            TokenCounts {
                input: 10_000,
                cache_read: 250_000,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&e, &pricing),
            micro_usd(0.01 * 6.0 + 0.25 * 0.6)
        );
    }

    #[test]
    fn missing_above_rates_fall_back_to_the_base_rate() {
        let pricing = ModelPricing {
            input_above: Some(6.0),
            output_above: None,
            cache_read_above: None,
            cache_write_above: None,
            ..tiered_pricing()
        };
        let e = entry(
            "m",
            TokenCounts {
                input: 300_000,
                output: 1_000,
                cache_read: 100_000,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&e, &pricing),
            micro_usd(0.3 * 6.0 + 0.001 * 15.0 + 0.1 * 0.3)
        );
    }

    #[test]
    fn codex_tier_is_selected_by_input_and_cache_reads_only() {
        // ccusage's codex split compares the raw per-turn input (uncached +
        // cached); output never selects the tier.
        let pricing = tiered_pricing();
        let output_heavy = codex_entry(
            "m",
            TokenCounts {
                input: 100_000,
                cache_read: 50_000,
                output: 500_000,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&output_heavy, &pricing),
            micro_usd(0.1 * 3.0 + 0.05 * 0.3 + 0.5 * 15.0)
        );

        let long = codex_entry(
            "m",
            TokenCounts {
                input: 180_000,
                cache_read: 20_001,
                output: 500,
                ..Default::default()
            },
        );
        assert_eq!(
            cost_from_pricing(&long, &pricing),
            micro_usd(0.18 * 6.0 + 0.020_001 * 0.6 + 0.000_5 * 30.0)
        );
    }

    #[test]
    fn codex_unpublished_cache_read_rate_bills_at_the_full_input_rate() {
        // ccusage `calculate_codex_bucket_cost`: without a published
        // cache-read rate, cached tokens bill like uncached input (and at the
        // above input rate in long-context requests) — not the Claude path's
        // 0.1x default.
        let pricing = ModelPricing {
            input: 2.0,
            output: 10.0,
            long_context_threshold: Some(200_000),
            input_above: Some(4.0),
            ..Default::default()
        };
        let short = codex_entry(
            "m",
            TokenCounts {
                cache_read: 1_000_000,
                ..Default::default()
            },
        );
        // 1M cached alone is long-context here (input + cache_read).
        assert_eq!(cost_from_pricing(&short, &pricing), micro_usd(1.0 * 4.0));
        let below = codex_entry(
            "m",
            TokenCounts {
                cache_read: 100_000,
                ..Default::default()
            },
        );
        assert_eq!(cost_from_pricing(&below, &pricing), micro_usd(0.1 * 2.0));
    }

    #[test]
    fn unpublished_cache_rates_fall_back_to_input_derived_defaults() {
        let mut e = entry(
            "any-model",
            TokenCounts {
                cache_read: 1_000_000,
                ..Default::default()
            },
        );
        e.transcript_cost_micro_usd = None;
        let no_cache_rates = ModelPricing {
            input: 2.0,
            output: 10.0,
            ..Default::default()
        };
        // The Claude path defaults unpublished cache-read pricing to
        // input * 0.1 (ccusage's models.dev loader); $0 would badly
        // undercount cache-heavy sessions.
        assert_eq!(cost_from_pricing(&e, &no_cache_rates), micro_usd(0.2));
        let explicit = ModelPricing {
            cache_read: Some(0.5),
            ..no_cache_rates
        };
        assert_eq!(cost_from_pricing(&e, &explicit), micro_usd(0.5));
        // An explicitly published zero rate really is free, unlike an
        // unpublished one.
        let explicit_zero = ModelPricing {
            cache_read: Some(0.0),
            ..no_cache_rates
        };
        assert_eq!(cost_from_pricing(&e, &explicit_zero), 0);
    }

    #[test]
    fn fast_entries_bill_the_whole_request_at_the_multiplier() {
        let pricing = ModelPricing {
            fast_multiplier: 2.5,
            ..tiered_pricing()
        };
        let mut e = codex_entry(
            "m",
            TokenCounts {
                input: 100_000,
                cache_read: 50_000,
                output: 1_000,
                ..Default::default()
            },
        );
        // 0.1M * 3 + 0.05M * 0.3 + 0.001M * 15 = $0.33
        let standard = cost_from_pricing(&e, &pricing);
        assert_eq!(standard, micro_usd(0.33));
        e.speed = Some(Speed::Fast);
        assert_eq!(cost_from_pricing(&e, &pricing), micro_usd(2.5 * 0.33));

        // Fast composes with long-context tiering.
        e.tokens.input = 300_000;
        assert_eq!(
            cost_from_pricing(&e, &pricing),
            micro_usd(2.5 * (0.3 * 6.0 + 0.05 * 0.6 + 0.001 * 30.0))
        );

        // A standard-speed entry never picks up the multiplier.
        e.speed = Some(Speed::Standard);
        assert_eq!(
            cost_from_pricing(&e, &pricing),
            micro_usd(0.3 * 6.0 + 0.05 * 0.6 + 0.001 * 30.0)
        );
    }

    #[test]
    fn codex_long_context_prices_from_the_embedded_snapshot() {
        // ccusage `prices_gpt_5_6_long_context_usage_from_embedded_pricing`:
        // a 300K-input gpt-5.6-sol turn crosses the 272K threshold and bills
        // entirely at the above rates.
        let pricing = pricing_for("gpt-5.6-sol").expect("snapshot pricing");
        let e = codex_entry(
            "gpt-5.6-sol",
            TokenCounts {
                input: 280_000,
                cache_read: 20_000,
                output: 1_000,
                ..Default::default()
            },
        );
        let expected = micro_usd(
            0.28 * pricing.input_above.unwrap()
                + 0.02 * pricing.cache_read_above.unwrap()
                + 0.001 * pricing.output_above.unwrap(),
        );
        assert_eq!(entry_cost_micro_usd(&e), Some(expected));
    }

    #[test]
    fn price_entry_records_the_tier_decision_and_catalog() {
        let long = entry(
            "claude-sonnet-4-6-20260115",
            TokenCounts {
                input: 300_000,
                ..Default::default()
            },
        );
        let priced = price_entry(&long);
        assert!(priced.long_context);
        assert_eq!(priced.pricing_catalog, Some(pricing_catalog_id()));
        assert!(priced.cost_micro_usd.unwrap() > 0);

        // Transcript-priced entries keep the tier decision (the wire's token
        // splits cover them) but claim no catalog.
        let mut transcript = long.clone();
        transcript.transcript_cost_micro_usd = Some(42);
        let priced = price_entry(&transcript);
        assert_eq!(priced.cost_micro_usd, Some(42));
        assert!(priced.long_context);
        assert_eq!(priced.pricing_catalog, None);

        let unknown = entry(
            "totally-unknown-model-xyz",
            TokenCounts {
                input: 100,
                ..Default::default()
            },
        );
        assert_eq!(
            price_entry(&unknown),
            EntryPricing {
                cost_micro_usd: None,
                long_context: false,
                pricing_catalog: None,
            }
        );
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
