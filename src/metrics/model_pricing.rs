//! Model pricing sourced from the models.dev catalog.
//!
//! Pricing data comes from <https://models.dev/api.json>, reduced to a flat
//! model-id → per-million-token cost map over a first-party provider
//! allowlist. Lookup resolves from, in order:
//!
//! 1. A local cache (`~/.git-ai/internal/models_dev_pricing.json`), refreshed
//!    from models.dev by `git-ai usage` at most once per day (best-effort —
//!    failures fall through silently).
//! 2. An embedded snapshot (`models_dev_pricing_snapshot.json`) baked into the
//!    binary at compile time. Regenerate it with:
//!    `cargo test regenerate_models_dev_pricing_snapshot -- --ignored`
//!
//! Tiered pricing (e.g. higher rates above 200k context) is intentionally
//! ignored: recorded token usage carries no per-request context size, and the
//! resulting figures are estimates either way.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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

    /// Look up pricing for a model id. Exact (case-insensitive) matches win;
    /// otherwise the longest catalog id that appears in the model id at token
    /// boundaries is used, which handles date-suffixed snapshots
    /// ("claude-sonnet-4-6-20250101") and provider-prefixed ids
    /// ("us.anthropic.claude-fable-5") without hardcoded family rules.
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
/// present, embedded snapshot otherwise).
pub fn pricing_for(model: &str) -> Option<&'static ModelPricing> {
    catalog().pricing_for(model)
}

fn catalog() -> &'static PricingCatalog {
    static CATALOG: OnceLock<PricingCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        // In-process unit tests always use the embedded snapshot so results
        // don't depend on the developer's local pricing cache.
        if !cfg!(test)
            && let Some(cache) = cache_path().and_then(|path| read_cache(&path))
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

fn read_cache(path: &std::path::Path) -> Option<PricingCache> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(path: &std::path::Path, cache: &PricingCache) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(cache) {
        let _ = std::fs::write(path, json);
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Best-effort refresh of the on-disk pricing cache from models.dev, called
/// by `git-ai usage` before stats are computed (i.e. before the global
/// catalog is first read). Skipped in tests and when the last attempt was
/// less than a day ago; on fetch failure the previous catalog is kept and the
/// attempt timestamp is still advanced.
pub fn refresh_cache_if_stale() {
    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_some() {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    let now = current_timestamp();
    let existing = read_cache(&path);
    if let Some(cache) = &existing
        && now.saturating_sub(cache.last_attempt_at) < REFRESH_INTERVAL_SECS
    {
        return;
    }
    let models = match fetch_and_trim_catalog() {
        Ok(models) => models,
        Err(_) => existing
            .map(|cache| cache.models)
            .unwrap_or_else(|| embedded_catalog().entries),
    };
    write_cache(
        &path,
        &PricingCache {
            last_attempt_at: now,
            models,
        },
    );
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
    fn lookup_rejects_non_boundary_substrings_and_unknown_models() {
        let catalog = embedded_catalog();
        // "gpt-5" appears in both, but not at a token boundary.
        assert_eq!(catalog.pricing_for("somegpt-5"), None);
        assert_eq!(catalog.pricing_for("gpt-51"), None);
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

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
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
        write_cache(
            &path,
            &PricingCache {
                last_attempt_at: 1234567890,
                models: models.clone(),
            },
        );

        let cache = read_cache(&path).unwrap();
        assert_eq!(cache.last_attempt_at, 1234567890);
        assert_eq!(cache.models, models);
    }

    #[test]
    fn read_cache_returns_none_for_missing_or_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        assert!(read_cache(&path).is_none());
        std::fs::write(&path, "not json").unwrap();
        assert!(read_cache(&path).is_none());
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
