//! Model pricing sourced from the models.dev catalog.
//!
//! Pricing data comes from <https://models.dev/api.json>, reduced to a flat
//! model-id → per-million-token cost map over a first-party provider
//! allowlist. Lookup resolves from, in order:
//!
//! 1. A local cache (`~/.git-ai/internal/models_dev_pricing.json`), refreshed
//!    from models.dev by `git-ai usage` at most once per day (best-effort —
//!    failures fall through silently). The cache only ever holds fetched
//!    data, so machines that can never reach models.dev keep using the
//!    embedded snapshot of whatever binary they run.
//! 2. An embedded snapshot (`models_dev_pricing_snapshot.json`) baked into the
//!    binary at compile time. Regenerate it with:
//!    `cargo test regenerate_models_dev_pricing_snapshot -- --ignored`
//!
//! Within a catalog, a model id is matched by exact id, then by the longest
//! catalog id at token boundaries, then by family fallback (see
//! [`PricingCatalog::pricing_for`]).
//!
//! Beyond flat rates, entries carry the model's long-context pricing tier
//! (models.dev `cost.tiers` with a `context` bound: every token of a request
//! whose context exceeds the threshold bills at the above-threshold rates)
//! and a fast/priority speed multiplier. Neither vendors nor models.dev
//! publish fast-tier pricing, and models.dev's first-party Anthropic entries
//! omit their long-context tiers, so both are hand-tracked in override
//! tables below, applied when a catalog is loaded.

use crate::utils::{read_json_file, unix_timestamp_now, write_json_file};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const EMBEDDED_SNAPSHOT: &str = include_str!("models_dev_pricing_snapshot.json");
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const CACHE_FILE_NAME: &str = "models_dev_pricing.json";
/// Format version of the on-disk cache. Bump whenever `ModelPricing`'s
/// serialized shape changes meaning: caches written under another format are
/// ignored (the embedded snapshot applies) until a refresh rewrites them,
/// never reinterpreted — a pre-tier cache stored explicit 0.0 for unpublished
/// cache rates, which the optional-rate schema would read as published-free
/// pricing, and it carries no tiers or fast multipliers.
const PRICING_CACHE_VERSION: u32 = 2;
/// Minimum interval between refresh *attempts* (successful or not), so
/// offline machines don't stall on the fetch timeout at every invocation.
const REFRESH_INTERVAL_SECS: u64 = 24 * 3600;
const FETCH_TIMEOUT_SECS: u64 = 5;

/// Providers whose models are kept when trimming the full models.dev catalog.
/// Restricted to first-party providers so aggregator/reseller listings can't
/// shadow canonical model ids with different prices.
const PROVIDER_ALLOWLIST: [&str; 8] = [
    "anthropic",
    "openai",
    "google",
    "xai",
    "deepseek",
    "mistral",
    "moonshotai",
    "zai",
];

/// Family tokens used as a last-resort pricing fallback for model ids the
/// catalog doesn't know (legacy ids like "claude-3-5-sonnet-20241022" or
/// successors newer than the catalog like "claude-opus-4-9"). Covers the
/// model families the supported agents emit.
const FAMILY_FALLBACK_TOKENS: [&str; 7] =
    ["opus", "sonnet", "haiku", "fable", "gpt", "gemini", "grok"];

/// Fast/priority speed multipliers over a request's whole cost, ported from
/// ccusage's fast-multiplier-overrides.json (MIT License, Copyright (c) 2025
/// @ryoppippi). No machine-readable source publishes these (OpenAI's Codex
/// "fast" tier is not its API priority pricing), so they are hand-tracked
/// against vendor price sheets. Exact entries apply to that catalog id only —
/// "gpt-5.5-pro" must not inherit gpt-5.5's multiplier.
const FAST_MULTIPLIER_EXACT: [(&str, f64); 7] = [
    ("gpt-5.6-sol", 2.0),
    ("gpt-5.6-terra", 2.0),
    ("gpt-5.6-luna", 2.0),
    // The bare family id is an alias that bills as gpt-5.6-sol (ccusage
    // `pricing_alias`); models.dev catalogs it as its own entry, so it needs
    // its own multiplier row.
    ("gpt-5.6", 2.0),
    ("gpt-5.5", 2.5),
    ("gpt-5.4", 2.0),
    ("gpt-5.3-codex", 2.0),
];

/// Prefix entries also cover the base model's date-suffixed and tier-variant
/// catalog ids (ccusage's `normalized_prefix` section).
const FAST_MULTIPLIER_PREFIX: [(&str, f64); 3] = [
    ("claude-opus-4-6", 6.0),
    ("claude-opus-4-7", 6.0),
    ("claude-opus-4-8", 2.0),
];

/// Anthropic's published long-context premium: every token of a request whose
/// context exceeds 200K bills at these rates on 1M-context models. models.dev
/// only records these tiers under gateway spellings (google-vertex, azure)
/// that the provider allowlist drops, so they are hand-tracked here and
/// stamped onto first-party entries that carry no tiers of their own. Prefix
/// semantics like [`FAST_MULTIPLIER_PREFIX`]. Both override tables are
/// applied when a catalog is *loaded* (not when it is fetched/trimmed), so
/// editing them takes effect on upgrade even against a previously fetched
/// on-disk cache.
const CLAUDE_LONG_CONTEXT_PREFIX: [(&str, LongContextRates); 6] = [
    ("claude-sonnet-4", SONNET_LONG_CONTEXT),
    ("claude-sonnet-4-5", SONNET_LONG_CONTEXT),
    ("claude-sonnet-4-6", SONNET_LONG_CONTEXT),
    ("claude-opus-4-6", OPUS_LONG_CONTEXT),
    ("claude-opus-4-7", OPUS_LONG_CONTEXT),
    ("claude-opus-4-8", OPUS_LONG_CONTEXT),
];

const SONNET_LONG_CONTEXT: LongContextRates = LongContextRates {
    threshold: 200_000,
    input: Some(6.0),
    output: Some(22.5),
    cache_read: Some(0.6),
    cache_write: Some(7.5),
};

const OPUS_LONG_CONTEXT: LongContextRates = LongContextRates {
    threshold: 200_000,
    input: Some(10.0),
    output: Some(37.5),
    cache_read: Some(1.0),
    cache_write: Some(12.5),
};

/// One above-threshold pricing band (per-million-token USD).
#[derive(Debug, Clone, Copy)]
struct LongContextRates {
    threshold: u64,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

/// Per-million-token pricing for a model (USD). Cache rates are `None` when
/// the catalog doesn't publish them — consumers distinguish published rates
/// from the derived defaults of [`ModelPricing::cache_read_rate`] /
/// [`ModelPricing::cache_write_rate`]. The `*_above` rates and threshold
/// describe the model's long-context tier: a request whose context exceeds
/// the threshold bills every token at the above rate (whole-request, not
/// marginal), falling back per-rate to the base rate when unpublished.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context_threshold: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_above: Option<f64>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub fast_multiplier: f64,
}

fn one() -> f64 {
    1.0
}

fn is_one(value: &f64) -> bool {
    *value == 1.0
}

impl Default for ModelPricing {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: None,
            cache_write: None,
            long_context_threshold: None,
            input_above: None,
            output_above: None,
            cache_read_above: None,
            cache_write_above: None,
            fast_multiplier: 1.0,
        }
    }
}

impl ModelPricing {
    /// Published cache-read rate, or ccusage's models.dev-loader default of a
    /// tenth of the input rate (cache reads dominate Codex usage; $0 would
    /// badly undercount).
    pub fn cache_read_rate(&self) -> f64 {
        self.cache_read.unwrap_or(self.input * 0.1)
    }

    /// Published cache-write rate, or ccusage's models.dev-loader default of
    /// 1.25x the input rate.
    pub fn cache_write_rate(&self) -> f64 {
        self.cache_write.unwrap_or(self.input * 1.25)
    }
}

/// A set of model pricing entries keyed by lowercased model id.
pub struct PricingCatalog {
    entries: BTreeMap<String, ModelPricing>,
}

impl PricingCatalog {
    fn from_entries(mut entries: BTreeMap<String, ModelPricing>) -> Self {
        apply_overrides(&mut entries);
        Self { entries }
    }

    fn from_snapshot_json(json: &str) -> Result<Self, serde_json::Error> {
        let entries: BTreeMap<String, ModelPricing> = serde_json::from_str(json)?;
        Ok(Self::from_entries(
            entries
                .into_iter()
                .map(|(id, pricing)| (id.to_lowercase(), pricing))
                .collect(),
        ))
    }

    /// Look up pricing for a model id (case-insensitive). Resolution order:
    ///
    /// 1. Exact id match.
    /// 2. Longest catalog id occurring in the model id at token boundaries —
    ///    covers date-suffixed snapshots ("claude-sonnet-4-6-20250101") and
    ///    provider-prefixed ids ("us.anthropic.claude-fable-5").
    /// 3. Family fallback: ids the catalog doesn't know at all (legacy or
    ///    too new) are priced like the median-priced model of their family,
    ///    so they estimate at family rates instead of silently costing $0.
    pub fn pricing_for(&self, model: &str) -> Option<ModelPricing> {
        let model = model.to_lowercase();
        if let Some(pricing) = self.entries.get(&model) {
            return Some(*pricing);
        }
        // A token-boundary match returns the catalog entry verbatim,
        // including its multiplier/tier — that's how date-suffixed and
        // variant spellings inherit their base model's data, and it matches
        // ccusage's fuzzy lookup, which also returns full entries.
        self.entries
            .iter()
            .filter(|(id, _)| contains_at_token_boundary(&model, id))
            .max_by_key(|(id, _)| id.len())
            .map(|(_, pricing)| *pricing)
            .or_else(|| self.family_fallback(&model))
    }

    /// Price an unknown model id like the median-priced catalog model of its
    /// family (by input rate, tie-broken by output rate then id). The median
    /// gives a representative family rate that's robust to outliers in either
    /// direction — legacy expensive entries (gpt-4 at ~24x gpt-5's input
    /// rate) as much as nano/mini variants — without parsing version numbers.
    fn family_fallback(&self, model: &str) -> Option<ModelPricing> {
        let token = FAMILY_FALLBACK_TOKENS
            .iter()
            .find(|token| contains_at_token_boundary(model, token))?;
        let mut family: Vec<(&String, &ModelPricing)> = self
            .entries
            .iter()
            .filter(|(id, _)| contains_at_token_boundary(id, token))
            .collect();
        family.sort_by(|(id_a, a), (id_b, b)| {
            a.input
                .total_cmp(&b.input)
                .then(a.output.total_cmp(&b.output))
                .then(id_a.cmp(id_b))
        });
        // A family-median ESTIMATE must not inherit one specific model's
        // hand-tracked fast multiplier or long-context tier: an unknown
        // fast-tier model would otherwise bill at up to 6x the median rate
        // and corrupt the emitted long-context splits.
        family
            .get(family.len() / 2)
            .map(|(_, pricing)| ModelPricing {
                long_context_threshold: None,
                input_above: None,
                output_above: None,
                cache_read_above: None,
                cache_write_above: None,
                fast_multiplier: 1.0,
                ..**pricing
            })
    }
}

/// True when `needle` occurs in `haystack` with non-alphanumeric characters
/// (or the string ends) on both sides, so "gpt-5" matches "openai/gpt-5" but
/// not "chatgpt-5" or "gpt-51".
fn contains_at_token_boundary(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|start| {
        let end = start + n.len();
        h[start..end] == *n
            && (start == 0 || !h[start - 1].is_ascii_alphanumeric())
            && (end == h.len() || !h[end].is_ascii_alphanumeric())
    })
}

/// Look up pricing for a model id in the global catalog (local cache when
/// present, embedded snapshot otherwise). Memoized per distinct id: misses of
/// the exact-match fast path scan the catalog linearly, and callers invoke
/// this once per recorded message.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    static MEMO: OnceLock<Mutex<HashMap<String, Option<ModelPricing>>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = memo.lock()
        && let Some(cached) = cache.get(model)
    {
        return *cached;
    }
    let result = catalog().pricing_for(model);
    if let Ok(mut cache) = memo.lock() {
        cache.insert(model.to_string(), result);
    }
    result
}

/// True when the process must not touch the user-level pricing cache: unit
/// tests run in-process (cfg!(test)) and integration-test subprocesses carry
/// the codebase-wide GIT_AI_TEST_DB_PATH marker. Both always use the embedded
/// snapshot so results don't depend on developer-machine state.
fn use_embedded_only() -> bool {
    cfg!(test) || std::env::var_os("GIT_AI_TEST_DB_PATH").is_some()
}

fn catalog() -> &'static PricingCatalog {
    static CATALOG: OnceLock<PricingCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        if !use_embedded_only()
            && let Some(cache) = cache_path().and_then(|path| read_json_file::<PricingCache>(&path))
            && cache.is_current_format()
            && !cache.models.is_empty()
        {
            return PricingCatalog::from_entries(cache.models);
        }
        embedded_catalog()
    })
}

fn embedded_catalog() -> PricingCatalog {
    PricingCatalog::from_snapshot_json(EMBEDDED_SNAPSHOT)
        .expect("embedded models.dev pricing snapshot must parse")
}

/// On-disk cache: the trimmed catalog plus the timestamp of the last refresh
/// attempt (used for throttling, so failed attempts are also spaced out).
#[derive(Serialize, Deserialize)]
struct PricingCache {
    /// See [`PRICING_CACHE_VERSION`]; pre-version files default to 0.
    #[serde(default)]
    version: u32,
    last_attempt_at: u64,
    models: BTreeMap<String, ModelPricing>,
}

impl PricingCache {
    fn is_current_format(&self) -> bool {
        self.version == PRICING_CACHE_VERSION
    }
}

fn cache_path() -> Option<PathBuf> {
    crate::config::internal_dir_path().map(|dir| dir.join(CACHE_FILE_NAME))
}

/// Best-effort refresh of the on-disk pricing cache from models.dev, called
/// by `git-ai usage` before stats are computed (i.e. before the global
/// catalog is first read). Skipped in tests and when the last attempt was
/// less than a day ago.
pub fn refresh_cache_if_stale() {
    if use_embedded_only() {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    let now = unix_timestamp_now();
    let existing: Option<PricingCache> = read_json_file(&path);
    if let Some(cache) = &existing
        && is_fresh(cache.last_attempt_at, now)
    {
        return;
    }
    write_json_file(&path, &next_cache(existing, fetch_and_trim_catalog(), now));
}

/// A refresh attempt at `last_attempt_at` is still fresh at `now` when it
/// lies within the past refresh interval. Future timestamps (clock skew, a
/// since-corrected clock) count as stale so a bogus timestamp can't block
/// refreshes indefinitely.
fn is_fresh(last_attempt_at: u64, now: u64) -> bool {
    last_attempt_at <= now && now - last_attempt_at < REFRESH_INTERVAL_SECS
}

/// Fold a fetch result into the next cache state. On failure the previously
/// fetched models are kept — unless they were written under another format,
/// which must not be relabeled as current — but the embedded snapshot is
/// never copied into the cache: an empty cache keeps falling through to the
/// (possibly newer) snapshot shipped with the running binary, while the
/// bumped attempt timestamp still throttles the next fetch.
fn next_cache(
    existing: Option<PricingCache>,
    fetched: Result<BTreeMap<String, ModelPricing>, String>,
    now: u64,
) -> PricingCache {
    let models = match fetched {
        Ok(models) => models,
        Err(_) => existing
            .filter(PricingCache::is_current_format)
            .map(|cache| cache.models)
            .unwrap_or_default(),
    };
    PricingCache {
        version: PRICING_CACHE_VERSION,
        last_attempt_at: now,
        models,
    }
}

fn fetch_and_trim_catalog() -> Result<BTreeMap<String, ModelPricing>, String> {
    let agent = crate::http::build_agent(Some(FETCH_TIMEOUT_SECS));
    let response = crate::http::send(agent.get(MODELS_DEV_API_URL))?;
    if response.status_code != 200 {
        return Err(format!("HTTP {}", response.status_code));
    }
    let body = response.as_str().map_err(|e| e.to_string())?;
    trim_catalog(body)
}

/// A model's `cost` object as models.dev publishes it. Unlike [`ModelPricing`]
/// it may lack input/output (skipped) and carries tier bands verbatim. Tiers
/// are held as raw JSON and parsed per band, so one novel-shaped band (or a
/// non-array `tiers`) degrades that model to its flat rates instead of
/// silently dropping it from the catalog.
#[derive(Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    #[serde(default)]
    tiers: serde_json::Value,
}

#[derive(Deserialize)]
struct ModelsDevTier {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tier: Option<ModelsDevTierBound>,
}

#[derive(Deserialize)]
struct ModelsDevTierBound {
    #[serde(rename = "type")]
    kind: Option<String>,
    size: Option<u64>,
}

impl ModelsDevCost {
    /// The model's long-context band: the `context`-bound tier with the
    /// lowest threshold. [`ModelPricing`] holds one above-base band, so the
    /// lowest threshold wins — a higher one would price everything between
    /// the two thresholds at the base rate (ccusage `long_context_tier`).
    fn long_context_rates(&self) -> Option<LongContextRates> {
        self.tiers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|band| ModelsDevTier::deserialize(band).ok())
            .filter_map(|tier| {
                let bound = tier.tier.as_ref()?;
                if bound.kind.as_deref() != Some("context") {
                    return None;
                }
                Some(LongContextRates {
                    threshold: bound.size.filter(|size| *size > 0)?,
                    input: tier.input,
                    output: tier.output,
                    cache_read: tier.cache_read,
                    cache_write: tier.cache_write,
                })
            })
            .min_by_key(|rates| rates.threshold)
    }
}

/// The override value whose key matches `model_id` exactly or as a
/// token-boundary prefix ("claude-opus-4-6" also covers
/// "claude-opus-4-6-20260205"). Catalog ids carry no provider prefixes, so
/// the boundary-containment helper is an exact-or-prefix match here. The
/// longest matching key wins (like [`PricingCatalog::pricing_for`]'s fuzzy
/// arm): "claude-sonnet-4" also prefixes "claude-sonnet-4-5", so first-match
/// would silently shadow the more specific row.
fn prefix_override<T: Copy>(overrides: &[(&str, T)], model_id: &str) -> Option<T> {
    overrides
        .iter()
        .filter(|(base, _)| contains_at_token_boundary(model_id, base))
        .max_by_key(|(base, _)| base.len())
        .map(|(_, value)| *value)
}

/// Stamp the hand-tracked overrides onto a loaded catalog: fast multipliers
/// always, the Claude long-context premium only where the entry publishes no
/// tiers of its own. Runs at load time so an override edit shipped in a new
/// binary applies even to a previously fetched cache (idempotent over caches
/// written before this moved out of the trim step).
fn apply_overrides(entries: &mut BTreeMap<String, ModelPricing>) {
    for (model_id, pricing) in entries.iter_mut() {
        pricing.fast_multiplier = fast_multiplier_override(model_id);
        if pricing.long_context_threshold.is_none()
            && let Some(rates) = prefix_override(&CLAUDE_LONG_CONTEXT_PREFIX, model_id)
        {
            pricing.long_context_threshold = Some(rates.threshold);
            pricing.input_above = rates.input;
            pricing.output_above = rates.output;
            pricing.cache_read_above = rates.cache_read;
            pricing.cache_write_above = rates.cache_write;
        }
    }
}

fn fast_multiplier_override(model_id: &str) -> f64 {
    FAST_MULTIPLIER_EXACT
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, multiplier)| *multiplier)
        .or_else(|| prefix_override(&FAST_MULTIPLIER_PREFIX, model_id))
        .unwrap_or(1.0)
}

/// Reduce a full models.dev `api.json` document to a flat model-id → pricing
/// map over the provider allowlist, keeping each model's long-context tier.
/// Pure models.dev data — the hand-tracked override tables are applied at
/// load time by [`apply_overrides`]. Models without a parseable cost entry
/// (missing, or lacking input/output rates) are skipped.
fn trim_catalog(api_json: &str) -> Result<BTreeMap<String, ModelPricing>, String> {
    let providers: serde_json::Value = serde_json::from_str(api_json).map_err(|e| e.to_string())?;
    let mut entries = BTreeMap::new();
    for provider in PROVIDER_ALLOWLIST {
        let Some(models) = providers
            .get(provider)
            .and_then(|p| p.get("models"))
            .and_then(|m| m.as_object())
        else {
            continue;
        };
        for (model_id, model) in models {
            let Some(cost) = model.get("cost") else {
                continue;
            };
            let Ok(cost) = serde_json::from_value::<ModelsDevCost>(cost.clone()) else {
                continue;
            };
            let (Some(input), Some(output)) = (cost.input, cost.output) else {
                continue;
            };
            let long_context = cost.long_context_rates();
            entries.insert(
                model_id.to_lowercase(),
                ModelPricing {
                    input,
                    output,
                    cache_read: cost.cache_read,
                    cache_write: cost.cache_write,
                    long_context_threshold: long_context.map(|rates| rates.threshold),
                    input_above: long_context.and_then(|rates| rates.input),
                    output_above: long_context.and_then(|rates| rates.output),
                    cache_read_above: long_context.and_then(|rates| rates.cache_read),
                    cache_write_above: long_context.and_then(|rates| rates.cache_write),
                    fast_multiplier: 1.0,
                },
            );
        }
    }
    if entries.is_empty() {
        return Err("no priced models found in models.dev catalog".to_string());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses_and_covers_current_models() {
        let catalog = embedded_catalog();
        for model in [
            "claude-fable-5",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "gpt-5.6-sol",
        ] {
            let pricing = catalog
                .pricing_for(model)
                .unwrap_or_else(|| panic!("snapshot must price {model}"));
            assert!(pricing.input > 0.0, "{model} input rate must be positive");
            assert!(pricing.output > 0.0, "{model} output rate must be positive");
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let catalog = embedded_catalog();
        assert_eq!(
            catalog.pricing_for("Claude-Fable-5"),
            catalog.pricing_for("claude-fable-5")
        );
    }

    #[test]
    fn lookup_matches_date_suffixed_model_ids() {
        let catalog = embedded_catalog();
        assert_eq!(
            catalog.pricing_for("claude-fable-5-20260607"),
            catalog.pricing_for("claude-fable-5")
        );
    }

    #[test]
    fn lookup_matches_provider_prefixed_model_ids() {
        let catalog = embedded_catalog();
        assert_eq!(
            catalog.pricing_for("us.anthropic.claude-fable-5"),
            catalog.pricing_for("claude-fable-5")
        );
    }

    #[test]
    fn lookup_prefers_longest_boundary_match() {
        // "gpt-5.6-sol" contains catalog ids "gpt-5", "gpt-5.6", and
        // "gpt-5.6-sol" at token boundaries; the exact (longest) one must win.
        let catalog = embedded_catalog();
        let sol = catalog.pricing_for("gpt-5.6-sol").unwrap();
        let base = catalog.pricing_for("gpt-5").unwrap();
        assert_ne!(sol, base, "gpt-5.6-sol must not fall back to gpt-5 rates");
    }

    #[test]
    fn lookup_falls_back_to_median_family_pricing() {
        let catalog = embedded_catalog();
        // These ids are absent from the catalog (legacy, or newer than the
        // snapshot) but must estimate at family rates rather than $0. The
        // equality assertions pin the current family medians.
        // Family fallbacks estimate at the median member's BASE rates with
        // multipliers/tiers neutralized (see
        // family_fallback_never_inherits_multipliers_or_tiers).
        assert_eq!(
            catalog
                .pricing_for("claude-3-5-sonnet-20241022")
                .unwrap()
                .input,
            catalog.pricing_for("claude-sonnet-4-6").unwrap().input,
            "legacy sonnet ids price at the median sonnet rate"
        );
        assert_eq!(
            catalog.pricing_for("claude-opus-4-1").unwrap().input,
            catalog.pricing_for("claude-opus-4-7").unwrap().input,
            "uncataloged opus ids price at the median opus rate"
        );
        assert!(
            catalog.pricing_for("claude-opus-4-9").is_some(),
            "dash-versioned successors newer than the catalog must not price as $0"
        );
        // The median must not resolve to a family outlier: legacy gpt-4 costs
        // ~24x the input rate of current gpt-5-generation models.
        let unknown_gpt = catalog
            .pricing_for("gpt-51")
            .expect("gpt family must price");
        let gpt4 = catalog.pricing_for("gpt-4").expect("snapshot has gpt-4");
        assert!(
            unknown_gpt.input < gpt4.input,
            "unknown gpt ids must not price at legacy gpt-4 outlier rates"
        );
    }

    #[test]
    fn lookup_rejects_non_boundary_substrings_and_unknown_models() {
        let catalog = embedded_catalog();
        // "gpt" appears in "somegpt-5", but not at a token boundary.
        assert_eq!(catalog.pricing_for("somegpt-5"), None);
        assert_eq!(catalog.pricing_for("totally-unknown-model"), None);
        assert_eq!(catalog.pricing_for(""), None);
    }

    #[test]
    fn trim_catalog_keeps_allowlisted_priced_models_only() {
        let api_json = serde_json::json!({
            "anthropic": {
                "models": {
                    "Claude-Test-1": {
                        "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.1, "cache_write": 1.25}
                    },
                    "claude-no-cost": {},
                    "claude-partial-cost": {"cost": {"input": 1.0}}
                }
            },
            "some-reseller": {
                "models": {
                    "claude-test-1": {"cost": {"input": 99.0, "output": 99.0}}
                }
            }
        })
        .to_string();

        let entries = trim_catalog(&api_json).unwrap();
        assert_eq!(
            entries.keys().collect::<Vec<_>>(),
            vec!["claude-test-1"],
            "only allowlisted models with full cost entries are kept, keyed lowercase"
        );
        let pricing = &entries["claude-test-1"];
        assert_eq!(pricing.input, 1.0);
        assert_eq!(pricing.output, 2.0);
        assert_eq!(pricing.cache_read, Some(0.1));
        assert_eq!(pricing.cache_write, Some(1.25));
    }

    #[test]
    fn trim_catalog_keeps_unpublished_cache_rates_distinguishable() {
        let api_json = serde_json::json!({
            "openai": {
                "models": {
                    "gpt-test-pro": {"cost": {"input": 15.0, "output": 120.0}}
                }
            }
        })
        .to_string();

        let entries = trim_catalog(&api_json).unwrap();
        let pricing = &entries["gpt-test-pro"];
        // None (unpublished) must stay distinguishable from an explicit 0 —
        // the Codex cost path bills unpublished cache reads at the full input
        // rate but explicit rates as published.
        assert_eq!(pricing.cache_read, None);
        assert_eq!(pricing.cache_write, None);
        assert_eq!(pricing.cache_read_rate(), 1.5);
        assert_eq!(pricing.cache_write_rate(), 18.75);
        let explicit_zero = ModelPricing {
            input: 15.0,
            cache_read: Some(0.0),
            cache_write: Some(0.0),
            ..Default::default()
        };
        assert_eq!(explicit_zero.cache_read_rate(), 0.0);
        assert_eq!(explicit_zero.cache_write_rate(), 0.0);
    }

    #[test]
    fn trim_catalog_keeps_the_lowest_context_tier() {
        let api_json = serde_json::json!({
            "openai": {
                "models": {
                    "gpt-test": {"cost": {
                        "input": 4.0, "output": 20.0, "cache_read": 0.4,
                        "tiers": [
                            {"input": 16.0, "output": 60.0, "tier": {"type": "context", "size": 544000}},
                            {"input": 8.0, "output": 30.0, "cache_read": 0.8, "tier": {"type": "context", "size": 272000}},
                            {"input": 99.0, "output": 99.0, "tier": {"type": "tokens", "size": 100}},
                            {"input": 99.0, "output": 99.0, "tier": {"type": "context", "size": 0}}
                        ]
                    }}
                }
            }
        })
        .to_string();

        let pricing = &trim_catalog(&api_json).unwrap()["gpt-test"];
        // Two context bands: the lowest threshold wins (one above-base band —
        // keeping the higher one would price everything between the two
        // thresholds at the base rate). Non-context and zero-size bounds are
        // not usable as a token threshold.
        assert_eq!(pricing.long_context_threshold, Some(272_000));
        assert_eq!(pricing.input_above, Some(8.0));
        assert_eq!(pricing.output_above, Some(30.0));
        assert_eq!(pricing.cache_read_above, Some(0.8));
        assert_eq!(pricing.cache_write_above, None);
    }

    #[test]
    fn fast_multipliers_attach_exactly_and_by_prefix() {
        let api_json = serde_json::json!({
            "openai": {
                "models": {
                    "gpt-5.5": {"cost": {"input": 5.0, "output": 30.0}},
                    "gpt-5.5-pro": {"cost": {"input": 30.0, "output": 180.0}},
                    "gpt-5.6": {"cost": {"input": 4.0, "output": 20.0}},
                    "gpt-5.6-sol": {"cost": {"input": 4.0, "output": 20.0}}
                }
            },
            "anthropic": {
                "models": {
                    "claude-opus-4-6": {"cost": {"input": 5.0, "output": 25.0}},
                    "claude-opus-4-6-20260205": {"cost": {"input": 5.0, "output": 25.0}},
                    "claude-fable-5": {"cost": {"input": 10.0, "output": 50.0}}
                }
            }
        })
        .to_string();

        let entries = trim_catalog(&api_json).unwrap();
        // Trim output is pure models.dev data; the override tables apply at
        // load time so editing them takes effect against fetched caches.
        assert_eq!(entries["gpt-5.5"].fast_multiplier, 1.0);
        let catalog = PricingCatalog::from_entries(entries);
        assert_eq!(catalog.pricing_for("gpt-5.5").unwrap().fast_multiplier, 2.5);
        // gpt entries are exact-only: the pro variant has its own pricing and
        // no published fast tier.
        assert_eq!(
            catalog.pricing_for("gpt-5.5-pro").unwrap().fast_multiplier,
            1.0
        );
        // The bare gpt-5.6 alias bills as gpt-5.6-sol (ccusage
        // `pricing_alias`), fast multiplier included.
        assert_eq!(catalog.pricing_for("gpt-5.6").unwrap().fast_multiplier, 2.0);
        assert_eq!(
            catalog.pricing_for("gpt-5.6-sol").unwrap().fast_multiplier,
            2.0
        );
        assert_eq!(
            catalog
                .pricing_for("claude-opus-4-6")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        // Prefix entries cover date-suffixed spellings of the base model.
        assert_eq!(
            catalog
                .pricing_for("claude-opus-4-6-20260205")
                .unwrap()
                .fast_multiplier,
            6.0
        );
        assert_eq!(
            catalog
                .pricing_for("claude-fable-5")
                .unwrap()
                .fast_multiplier,
            1.0
        );
    }

    #[test]
    fn prefix_override_prefers_the_longest_matching_key() {
        // "claude-sonnet-4" also token-boundary-prefixes "claude-sonnet-4-5":
        // a more specific row must win regardless of table order, or edits to
        // it are silently shadowed (CLAUDE_LONG_CONTEXT_PREFIX lists both).
        let overrides = [("claude-sonnet-4", 1.0), ("claude-sonnet-4-5", 2.0)];
        assert_eq!(prefix_override(&overrides, "claude-sonnet-4-5"), Some(2.0));
        assert_eq!(
            prefix_override(&overrides, "claude-sonnet-4-5-20260601"),
            Some(2.0)
        );
        assert_eq!(prefix_override(&overrides, "claude-sonnet-4"), Some(1.0));
        assert_eq!(prefix_override(&overrides, "claude-opus-4"), None);
    }

    #[test]
    fn claude_long_context_premium_is_stamped_when_models_dev_lacks_tiers() {
        let api_json = serde_json::json!({
            "anthropic": {
                "models": {
                    "claude-sonnet-4-6": {"cost": {
                        "input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75
                    }},
                    "claude-sonnet-4-6-20260115": {"cost": {"input": 3.0, "output": 15.0}},
                    // A published tier must win over the hand-tracked one.
                    "claude-opus-4-8": {"cost": {
                        "input": 5.0, "output": 25.0,
                        "tiers": [{"input": 11.0, "output": 40.0, "tier": {"type": "context", "size": 250000}}]
                    }},
                    "claude-fable-5": {"cost": {"input": 10.0, "output": 50.0}}
                }
            }
        })
        .to_string();

        let entries = trim_catalog(&api_json).unwrap();
        assert_eq!(
            entries["claude-sonnet-4-6"].long_context_threshold, None,
            "trim output is pure models.dev data"
        );
        let catalog = PricingCatalog::from_entries(entries);
        let sonnet = catalog.pricing_for("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.long_context_threshold, Some(200_000));
        assert_eq!(sonnet.input_above, Some(6.0));
        assert_eq!(sonnet.output_above, Some(22.5));
        assert_eq!(sonnet.cache_read_above, Some(0.6));
        assert_eq!(sonnet.cache_write_above, Some(7.5));
        assert_eq!(
            catalog
                .pricing_for("claude-sonnet-4-6-20260115")
                .unwrap()
                .long_context_threshold,
            Some(200_000),
            "date-suffixed spellings inherit the premium"
        );
        let opus = catalog.pricing_for("claude-opus-4-8").unwrap();
        assert_eq!(
            opus.long_context_threshold,
            Some(250_000),
            "published tier wins"
        );
        assert_eq!(opus.input_above, Some(11.0));
        assert_eq!(
            catalog
                .pricing_for("claude-fable-5")
                .unwrap()
                .long_context_threshold,
            None,
            "models without a tracked premium stay flat"
        );
    }

    #[test]
    fn family_fallback_never_inherits_multipliers_or_tiers() {
        // A family-median ESTIMATE must not carry a specific model's
        // hand-tracked fast multiplier or long-context tier: an unknown
        // fast-tier opus id would otherwise bill 6x and flip the
        // long-context split.
        let catalog = embedded_catalog();
        let fallback = catalog.pricing_for("claude-opus-4-1").unwrap();
        assert_eq!(fallback.fast_multiplier, 1.0);
        assert_eq!(fallback.long_context_threshold, None);
        assert_eq!(fallback.input_above, None);
        let median = catalog.pricing_for("claude-opus-4-7").unwrap();
        assert_eq!(fallback.input, median.input);
        assert_eq!(fallback.output, median.output);
    }

    #[test]
    fn overrides_apply_idempotently_over_pre_split_caches() {
        // A current-format cache written before overrides moved to load time
        // carries them baked in; re-applying at load must be a no-op, and an
        // override-table edit must win over the baked value.
        let mut baked = BTreeMap::new();
        baked.insert(
            "claude-opus-4-8".to_string(),
            ModelPricing {
                input: 5.0,
                output: 25.0,
                long_context_threshold: Some(200_000),
                input_above: Some(10.0),
                fast_multiplier: 2.0,
                ..Default::default()
            },
        );
        let catalog = PricingCatalog::from_entries(baked);
        let pricing = catalog.pricing_for("claude-opus-4-8").unwrap();
        assert_eq!(pricing.fast_multiplier, 2.0);
        assert_eq!(pricing.long_context_threshold, Some(200_000));
        assert_eq!(pricing.input_above, Some(10.0), "baked tier kept as-is");
    }

    #[test]
    fn one_malformed_tier_band_degrades_to_flat_rates_not_a_dropped_model() {
        let api_json = serde_json::json!({
            "openai": {
                "models": {
                    "gpt-null-tiers": {"cost": {"input": 4.0, "output": 20.0, "tiers": null}},
                    "gpt-odd-band": {"cost": {
                        "input": 4.0, "output": 20.0,
                        "tiers": ["surprise", {"input": 8.0, "output": 30.0, "tier": {"type": "context", "size": 272000}}]
                    }}
                }
            }
        })
        .to_string();
        let entries = trim_catalog(&api_json).unwrap();
        // Novel-shaped tier data must not drop the model (flat rates parse),
        // and parseable bands next to a malformed one still apply.
        assert_eq!(entries["gpt-null-tiers"].input, 4.0);
        assert_eq!(entries["gpt-null-tiers"].long_context_threshold, None);
        assert_eq!(
            entries["gpt-odd-band"].long_context_threshold,
            Some(272_000)
        );
    }

    #[test]
    fn flat_snapshot_json_without_tier_fields_still_parses() {
        // The pre-tier snapshot/cache format: plain rates, no tier fields.
        let catalog = PricingCatalog::from_snapshot_json(
            r#"{"claude-test-1": {"input": 1.0, "output": 2.0, "cache_read": 0.1, "cache_write": 1.25}}"#,
        )
        .unwrap();
        let pricing = catalog.pricing_for("claude-test-1").unwrap();
        assert_eq!(pricing.cache_read, Some(0.1));
        assert_eq!(pricing.long_context_threshold, None);
        assert_eq!(pricing.fast_multiplier, 1.0);
    }

    #[test]
    fn embedded_snapshot_carries_tiers_and_fast_multipliers() {
        let catalog = embedded_catalog();
        let sol = catalog.pricing_for("gpt-5.6-sol").unwrap();
        assert_eq!(sol.long_context_threshold, Some(272_000));
        assert!(sol.input_above.is_some());
        assert_eq!(sol.fast_multiplier, 2.0);
        let sonnet = catalog.pricing_for("claude-sonnet-4-6").unwrap();
        assert_eq!(sonnet.long_context_threshold, Some(200_000));
        assert_eq!(sonnet.input_above, Some(6.0));
        let opus = catalog.pricing_for("claude-opus-4-8").unwrap();
        assert_eq!(opus.fast_multiplier, 2.0);
        assert_eq!(opus.long_context_threshold, Some(200_000));
        let fable = catalog.pricing_for("claude-fable-5").unwrap();
        assert_eq!(fable.fast_multiplier, 1.0);
        assert_eq!(fable.long_context_threshold, None);
    }

    #[test]
    fn trim_catalog_rejects_invalid_or_empty_documents() {
        assert!(trim_catalog("not json").is_err());
        assert!(trim_catalog("{}").is_err());
        assert!(trim_catalog(r#"{"anthropic": {"models": {}}}"#).is_err());
    }

    fn fetched_models() -> BTreeMap<String, ModelPricing> {
        let mut models = BTreeMap::new();
        models.insert(
            "claude-test-1".to_string(),
            ModelPricing {
                input: 10.0,
                output: 50.0,
                cache_read: Some(1.0),
                cache_write: Some(12.5),
                ..Default::default()
            },
        );
        models
    }

    #[test]
    fn refresh_failure_never_copies_embedded_data_into_the_cache() {
        // First-ever attempt fails: the cache records the attempt but stays
        // empty, so catalog() keeps using the running binary's snapshot.
        let cache = next_cache(None, Err("offline".to_string()), 100);
        assert_eq!(cache.last_attempt_at, 100);
        assert!(cache.models.is_empty());

        // A later failure keeps previously *fetched* models.
        let existing = PricingCache {
            version: PRICING_CACHE_VERSION,
            last_attempt_at: 100,
            models: fetched_models(),
        };
        let cache = next_cache(Some(existing), Err("offline".to_string()), 200);
        assert_eq!(cache.last_attempt_at, 200);
        assert_eq!(cache.models, fetched_models());

        // Success replaces the models outright.
        let existing = PricingCache {
            version: PRICING_CACHE_VERSION,
            last_attempt_at: 200,
            models: BTreeMap::new(),
        };
        let cache = next_cache(Some(existing), Ok(fetched_models()), 300);
        assert_eq!(cache.last_attempt_at, 300);
        assert_eq!(cache.models, fetched_models());
    }

    #[test]
    fn other_format_cache_files_are_ignored_not_reinterpreted() {
        // A cache written before cache rates became optional stores explicit
        // 0.0 for unpublished rates, which the optional-rate schema would
        // read as published-free pricing (and it carries no tiers or fast
        // multipliers). Such files must be discarded — the embedded snapshot
        // applies — never reinterpreted.
        let old: PricingCache = serde_json::from_str(
            r#"{"last_attempt_at":100,"models":{"gpt-5-pro":{"input":15.0,"output":120.0,"cache_read":0.0,"cache_write":0.0}}}"#,
        )
        .unwrap();
        assert!(!old.is_current_format(), "pre-version files default to 0");
        // A failed refresh drops the other-format models rather than
        // relabeling them as current.
        let next = next_cache(Some(old), Err("offline".to_string()), 200);
        assert!(next.models.is_empty());
        assert_eq!(next.version, PRICING_CACHE_VERSION);
    }

    #[test]
    fn staleness_treats_future_timestamps_as_stale() {
        let now = 1_000_000_000;
        assert!(is_fresh(now, now));
        assert!(is_fresh(now - REFRESH_INTERVAL_SECS + 1, now));
        assert!(!is_fresh(now - REFRESH_INTERVAL_SECS, now));
        // A clock that was skewed ahead when the cache was written must not
        // block refreshes after it is corrected.
        assert!(!is_fresh(now + 1, now));
        assert!(!is_fresh(u64::MAX, now));
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        write_json_file(
            &path,
            &PricingCache {
                version: PRICING_CACHE_VERSION,
                last_attempt_at: 1234567890,
                models: fetched_models(),
            },
        );

        let cache: PricingCache = read_json_file(&path).unwrap();
        assert!(cache.is_current_format());
        assert_eq!(cache.last_attempt_at, 1234567890);
        assert_eq!(cache.models, fetched_models());
    }

    /// Regenerates the embedded snapshot from the live models.dev catalog.
    /// Run manually when new models ship or prices change:
    /// `cargo test regenerate_models_dev_pricing_snapshot -- --ignored`
    #[test]
    #[ignore]
    fn regenerate_models_dev_pricing_snapshot() {
        let entries = fetch_and_trim_catalog().expect("fetching models.dev catalog must succeed");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/metrics/models_dev_pricing_snapshot.json");
        let mut json = serde_json::to_string_pretty(&entries).expect("snapshot must serialize");
        json.push('\n');
        std::fs::write(&path, json).expect("snapshot file must be writable");
    }
}
