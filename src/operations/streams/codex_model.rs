//! Codex model resolution fallbacks.
//!
//! Codex hooks may omit the model even though local evidence identifies the
//! effective one. Resolution order (all reads bounded): transcript tail
//! (newest `turn_context`/`session_meta` wins), transcript head, then the
//! `config.toml` under the resolved codex home — honoring the selected
//! profile's model before the root `model` key.

use crate::model::stream_types::StreamError;
use crate::operations::streams::jsonl_scan::scan_jsonl;
use crate::operations::streams::model_extraction::normalize_model;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_CODEX_CONFIG_BYTES: u64 = 1024 * 1024;

pub(crate) fn extract_model_from_codex_jsonl(path: &Path) -> Result<Option<String>, StreamError> {
    if let Some(model) = scan_jsonl(path, extract_model_from_codex_jsonl_line)? {
        return Ok(Some(model));
    }

    Ok(extract_model_from_codex_config(path))
}

fn extract_model_from_codex_jsonl_line(line: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    if !matches!(
        json.get("type").and_then(|v| v.as_str()),
        Some("session_meta" | "turn_context")
    ) {
        return None;
    }

    let payload = json.get("payload")?;
    ["model", "model_id", "modelId"]
        .iter()
        .find_map(|key| normalize_model(payload.get(key)?.as_str()?))
}

fn extract_model_from_codex_config(path: &Path) -> Option<String> {
    let codex_home = codex_home_from_transcript_path(path)?;
    let config_path = codex_home.join("config.toml");
    let file = File::open(config_path).ok()?;
    let mut content = String::new();
    file.take(MAX_CODEX_CONFIG_BYTES + 1)
        .read_to_string(&mut content)
        .ok()?;
    if content.len() as u64 > MAX_CODEX_CONFIG_BYTES {
        return None;
    }
    let config: toml::Value = toml::from_str(&content).ok()?;

    config
        .get("profile")
        .and_then(toml::Value::as_str)
        .and_then(|profile| {
            normalize_model(
                config
                    .get("profiles")?
                    .get(profile)?
                    .get("model")?
                    .as_str()?,
            )
        })
        .or_else(|| normalize_model(config.get("model")?.as_str()?))
}

fn codex_home_from_transcript_path(path: &Path) -> Option<PathBuf> {
    let configured_home = crate::operations::mdm::paths::codex_home_dir();
    if path.starts_with(&configured_home) {
        return Some(configured_home);
    }

    for ancestor in path.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) == Some(".codex") {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::operations::streams::model_extraction::extract_model;
    use crate::operations::streams::sweep::StreamFormat;
    use std::io::Write;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    const MODELLESS_TRANSCRIPT: &str = r#"{"type":"session_meta","payload":{"model":null}}"#;

    /// Builds a codex home named `home_dir_name` holding `config` and a
    /// transcript with `transcript_line`, then runs extraction on it.
    /// Returns the codex home alongside the result for env-var tests.
    fn extract_codex_model_in_home(
        home_dir_name: &str,
        config: &str,
        transcript_line: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, Option<String>) {
        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join(home_dir_name);
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(codex_home.join("config.toml"), config).unwrap();

        let transcript = session_dir.join("rollout-test.jsonl");
        std::fs::write(&transcript, transcript_line).unwrap();

        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        (dir, codex_home, result)
    }

    fn extract_codex_model_with_config(config: &str) -> Option<String> {
        extract_codex_model_in_home(".codex", config, MODELLESS_TRANSCRIPT).2
    }

    #[test]
    fn test_extract_model_codex_session_meta_model() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"model":"gpt-5.3-codex","model_provider":"openai_https"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5.3-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_turn_context_model() {
        let path = fixture_path("codex-session-simple.jsonl");
        let result = extract_model(&path, StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_latest_turn_context_model_wins() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"initial-model"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"switched-model"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("switched-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_head_skips_oversized_record() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        let oversized_record = serde_json::json!({ "padding": "x".repeat(51_200) });
        writeln!(file, "{oversized_record}").unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"model-after-limit"}}}}"#
        )
        .unwrap();
        writeln!(file, "{oversized_record}").unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("model-after-limit".to_string()));
    }

    #[test]
    fn test_extract_model_codex_skips_session_meta_without_payload() {
        let mut file = tempfile::NamedTempFile::with_suffix(".jsonl").unwrap();
        writeln!(file, r#"{{"type":"session_meta"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"model":"gpt-5.3-codex"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = extract_model(file.path(), StreamFormat::CodexJsonl, None).unwrap();
        assert_eq!(result, Some("gpt-5.3-codex".to_string()));
    }

    #[test]
    fn test_extract_model_codex_config_fallback_when_session_model_missing() {
        let result = extract_codex_model_with_config(
            r#"model = "gpt-5.5"
model_provider = "openai_https"

[profiles.default]
model = "wrong-profile-model"
"#,
        );

        assert_eq!(result, Some("gpt-5.5".to_string()));
    }

    #[test]
    fn test_extract_model_codex_selected_profile_overrides_root_model() {
        let result = extract_codex_model_with_config(
            r#"model = "root-model"
profile = "work"

[profiles.work]
model = "profile-model"
"#,
        );

        assert_eq!(result, Some("profile-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_selected_profile_can_supply_model() {
        let result = extract_codex_model_with_config(
            r#"profile = "work"

[profiles.work]
model = "profile-only-model"
"#,
        );

        assert_eq!(result, Some("profile-only-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_prefers_transcript_model_over_config() {
        let (_dir, _home, result) = extract_codex_model_in_home(
            ".codex",
            r#"model = "config-model""#,
            r#"{"type":"session_meta","payload":{"model":"transcript-model"}}"#,
        );
        assert_eq!(result, Some("transcript-model".to_string()));
    }

    #[test]
    fn test_extract_model_codex_rejects_oversized_config() {
        let mut config = String::from("model = \"oversized-config-model\"\n# ");
        config.push_str(&"x".repeat(1024 * 1024));

        assert_eq!(extract_codex_model_with_config(&config), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_model_codex_config_fallback_respects_codex_home() {
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let dir = tempfile::TempDir::new().unwrap();
        let codex_home = dir.path().join("custom-codex-home");
        let session_dir = codex_home.join("sessions/2026/06/30");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            r#"model = "custom-home-model""#,
        )
        .unwrap();
        let transcript = session_dir.join("rollout-test.jsonl");
        std::fs::write(&transcript, MODELLESS_TRANSCRIPT).unwrap();

        unsafe {
            std::env::set_var("CODEX_HOME", &codex_home);
        }
        let result = extract_model(&transcript, StreamFormat::CodexJsonl, None).unwrap();
        unsafe {
            match previous_codex_home {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        assert_eq!(result, Some("custom-home-model".to_string()));
    }
}
