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
//! Tiered pricing (e.g. higher rates above 200k context) is intentionally
//! ignored: recorded token usage carries no per-request context size, and the
//! resulting figures are estimates either way.

use crate::utils::{read_json_file, unix_timestamp_now, write_json_file};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const EMBEDDED_SNAPSHOT: &str = include_str!("models_dev_pricing_snapshot.json");
const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const CACHE_FILE_NAME: &str = "models_dev_pricing.json";
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

/// Per-million-token pricing for a model (USD). Cache fields default to 0 —
/// models.dev omits them for models without prompt caching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// A set of model pricing entries keyed by lowercased model id.
pub struct PricingCatalog {
    entries: BTreeMap<String, ModelPricing>,
}

impl PricingCatalog {
    fn from_entries(entries: BTreeMap<String, ModelPricing>) -> Self {
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
    pub fn pricing_for(&self, model: &str) -> Option<&ModelPricing> {
        let model = model.to_lowercase();
        if let Some(pricing) = self.entries.get(&model) {
            return Some(pricing);
        }
        self.entries
            .iter()
            .filter(|(id, _)| contains_at_token_boundary(&model, id))
            .max_by_key(|(id, _)| id.len())
            .map(|(_, pricing)| pricing)
            .or_else(|| self.family_fallback(&model))
    }

    /// Price an unknown model id like the median-priced catalog model of its
    /// family (by input rate, tie-broken by output rate then id). The median
    /// gives a representative family rate that's robust to outliers in either
    /// direction — legacy expensive entries (gpt-4 at ~24x gpt-5's input
    /// rate) as much as nano/mini variants — without parsing version numbers.
    fn family_fallback(&self, model: &str) -> Option<&ModelPricing> {
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
        family.get(family.len() / 2).map(|(_, pricing)| *pricing)
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
pub fn pricing_for(model: &str) -> Option<&'static ModelPricing> {
    static MEMO: OnceLock<Mutex<HashMap<String, Option<&'static ModelPricing>>>> = OnceLock::new();
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
    last_attempt_at: u64,
    models: BTreeMap<String, ModelPricing>,
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
/// fetched models are kept, but the embedded snapshot is never copied into
/// the cache: an empty cache keeps falling through to the (possibly newer)
/// snapshot shipped with the running binary, while the bumped attempt
/// timestamp still throttles the next fetch.
fn next_cache(
    existing: Option<PricingCache>,
    fetched: Result<BTreeMap<String, ModelPricing>, String>,
    now: u64,
) -> PricingCache {
    let models = match fetched {
        Ok(models) => models,
        Err(_) => existing.map(|cache| cache.models).unwrap_or_default(),
    };
    PricingCache {
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

/// Reduce a full models.dev `api.json` document to a flat model-id → pricing
/// map over the provider allowlist. Models without a parseable cost entry
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
            if let Ok(pricing) = serde_json::from_value::<ModelPricing>(cost.clone()) {
                entries.insert(model_id.to_lowercase(), pricing);
            }
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
        assert_eq!(
            catalog.pricing_for("claude-3-5-sonnet-20241022"),
            catalog.pricing_for("claude-sonnet-4-6"),
            "legacy sonnet ids price at the median sonnet rate"
        );
        assert_eq!(
            catalog.pricing_for("claude-opus-4-1"),
            catalog.pricing_for("claude-opus-5"),
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
        assert_eq!(pricing.cache_read, 0.1);
        assert_eq!(pricing.cache_write, 1.25);
    }

    #[test]
    fn trim_catalog_defaults_missing_cache_rates_to_zero() {
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
        assert_eq!(pricing.cache_read, 0.0);
        assert_eq!(pricing.cache_write, 0.0);
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
                cache_read: 1.0,
                cache_write: 12.5,
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
            last_attempt_at: 100,
            models: fetched_models(),
        };
        let cache = next_cache(Some(existing), Err("offline".to_string()), 200);
        assert_eq!(cache.last_attempt_at, 200);
        assert_eq!(cache.models, fetched_models());

        // Success replaces the models outright.
        let existing = PricingCache {
            last_attempt_at: 200,
            models: BTreeMap::new(),
        };
        let cache = next_cache(Some(existing), Ok(fetched_models()), 300);
        assert_eq!(cache.last_attempt_at, 300);
        assert_eq!(cache.models, fetched_models());
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
                last_attempt_at: 1234567890,
                models: fetched_models(),
            },
        );

        let cache: PricingCache = read_json_file(&path).unwrap();
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
