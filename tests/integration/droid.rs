use crate::test_utils::fixture_path;
use git_ai::error::GitAiError;
use git_ai::model::stream_watermark::HybridWatermark;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::DroidAgent;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;

fn parse_droid(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("droid")?.parse(hook_input, "t_test")
}

#[test]
fn test_droid_raw_event_fidelity() {
    let fixture = fixture_path("droid-session.jsonl");
    let agent = DroidAgent::new();
    let watermark = Box::new(HybridWatermark::new(0, 0, None));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Failed to parse JSONL");

    // Independently parse the fixture, applying the same "message"-only filter the Droid agent uses.
    let expected: Vec<serde_json::Value> = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["type"].as_str() == Some("message"))
        .collect();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
fn test_parse_droid_settings_model() {
    let fixture = fixture_path("droid-session.settings.json");
    let content = fs::read_to_string(fixture).expect("Failed to read settings.json");
    let settings: serde_json::Value =
        serde_json::from_str(&content).expect("Failed to parse settings.json");
    let model = settings["model"].as_str().map(|s| s.to_string());

    assert!(model.is_some(), "Model should be extracted from settings");
    assert_eq!(
        model.unwrap(),
        "custom:BYOK-GPT-5-MINI-0",
        "Model should match the fixture value"
    );
}

#[test]
fn test_droid_preset_extracts_edited_filepath() {
    let fixture = fixture_path("droid-session.jsonl");
    let settings_fixture = fixture_path("droid-session.settings.json");

    let transcript_path = fixture.to_str().unwrap();
    let settings_path = settings_fixture.to_str().unwrap();

    let temp_dir = tempfile::tempdir().unwrap();
    let jsonl_path = temp_dir.path().join("session.jsonl");
    let temp_settings_path = temp_dir.path().join("session.settings.json");
    fs::copy(transcript_path, &jsonl_path).unwrap();
    fs::copy(settings_path, &temp_settings_path).unwrap();

    let hook_input = json!({
        "cwd": "/Users/testuser/projects/testing-git",
        "hookEventName": "PostToolUse",
        "sessionId": "052cb8d0-4616-488a-99fe-bfbbbe9429b3",
        "toolName": "ApplyPatch",
        "tool_input": {
            "file_path": "/Users/testuser/projects/testing-git/index.ts"
        },
        "transcriptPath": jsonl_path.to_str().unwrap()
    })
    .to_string();

    let events = parse_droid(&hook_input).expect("Failed to parse droid hook input");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "Should contain edited filepath, got: {:?}",
                e.file_paths
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_droid_preset_extracts_applypatch_filepath() {
    let temp_dir = tempfile::tempdir().unwrap();
    let jsonl_path = temp_dir.path().join("session.jsonl");
    let settings_path = temp_dir.path().join("session.settings.json");

    fs::write(&jsonl_path, "").unwrap();
    fs::write(&settings_path, r#"{"model":"test-model"}"#).unwrap();

    let hook_input = json!({
        "cwd": "/Users/testuser/projects/testing-git",
        "hookEventName": "PostToolUse",
        "sessionId": "test-session-id",
        "toolName": "ApplyPatch",
        "tool_input": "*** Begin Patch\n*** Update File: /Users/testuser/projects/testing-git/index.ts\n@@\n-// old\n+// new\n*** End Patch",
        "transcriptPath": jsonl_path.to_str().unwrap()
    })
    .to_string();

    let events = parse_droid(&hook_input).expect("Failed to parse droid hook input");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let path_strs: Vec<String> = e
                .file_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            assert!(
                path_strs.iter().any(|p| p.contains("index.ts")),
                "Should extract file path from ApplyPatch text, got: {:?}",
                path_strs
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_droid_preset_stores_metadata_paths() {
    let temp_dir = tempfile::tempdir().unwrap();
    let jsonl_path = temp_dir.path().join("session.jsonl");
    let settings_path = temp_dir.path().join("session.settings.json");

    let fixture = fixture_path("droid-session.jsonl");
    let settings_fixture = fixture_path("droid-session.settings.json");
    fs::copy(&fixture, &jsonl_path).unwrap();
    fs::copy(&settings_fixture, &settings_path).unwrap();

    let hook_input = json!({
        "cwd": "/Users/testuser/projects/testing-git",
        "hookEventName": "PostToolUse",
        "sessionId": "052cb8d0-4616-488a-99fe-bfbbbe9429b3",
        "toolName": "Read",
        "transcriptPath": jsonl_path.to_str().unwrap()
    })
    .to_string();

    let events = parse_droid(&hook_input).expect("Failed to parse droid hook input");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(
                e.context.metadata.contains_key("transcript_path"),
                "Metadata should contain transcript_path"
            );
            assert!(
                e.context.metadata.contains_key("settings_path"),
                "Metadata should contain settings_path"
            );
            assert_eq!(
                e.context.metadata["transcript_path"],
                jsonl_path.to_str().unwrap()
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_droid_preset_uses_raw_session_id() {
    let temp_dir = tempfile::tempdir().unwrap();
    let jsonl_path = temp_dir.path().join("session.jsonl");
    let settings_path = temp_dir.path().join("session.settings.json");

    fs::write(&jsonl_path, "").unwrap();
    fs::write(&settings_path, r#"{"model":"test-model"}"#).unwrap();

    let session_uuid = "052cb8d0-4616-488a-99fe-bfbbbe9429b3";

    let hook_input = json!({
        "cwd": "/Users/testuser/projects/testing-git",
        "hookEventName": "PostToolUse",
        "sessionId": session_uuid,
        "toolName": "Read",
        "transcriptPath": jsonl_path.to_str().unwrap()
    })
    .to_string();

    let events = parse_droid(&hook_input).expect("Failed to parse droid hook input");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.context.agent_id.id, session_uuid,
                "agent_id.id should be the raw session UUID"
            );
            assert_eq!(e.context.agent_id.tool, "droid");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_droid_e2e_prefers_latest_checkpoint_for_prompts() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();

    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();

    let src_dir = repo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.ts");
    fs::write(&file_path, "// initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("droid-session.jsonl");
    let settings_path = repo_root.join("droid-session.settings.json");

    fs::write(&transcript_path, "").unwrap();
    fs::write(&settings_path, r#"{"model":"custom:BYOK-GPT-5-MINI-0"}"#).unwrap();

    let hook_input = json!({
        "cwd": repo_root.to_string_lossy().to_string(),
        "hookEventName": "PostToolUse",
        "sessionId": "052cb8d0-4616-488a-99fe-bfbbbe9429b3",
        "toolName": "ApplyPatch",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcriptPath": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    fs::write(&file_path, "// initial\n// ai line one\n").unwrap();
    repo.checkpoint_with_hook_input("droid", &hook_input)
        .unwrap();

    let fixture = fixture_path("droid-session.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();
    fs::write(&file_path, "// initial\n// ai line one\n// ai line two\n").unwrap();
    repo.checkpoint_with_hook_input("droid", &hook_input)
        .unwrap();

    let commit = repo.stage_all_and_commit("Add AI lines").unwrap();

    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected a single session record"
    );
    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    assert_eq!(
        session_record.agent_id.model, "custom:BYOK-GPT-5-MINI-0",
        "Session record model should be extracted from droid settings fixture"
    );
}

#[test]
fn test_droid_preset_pretooluse_returns_human_checkpoint() {
    let temp_dir = tempfile::tempdir().unwrap();
    let jsonl_path = temp_dir.path().join("session.jsonl");
    let settings_path = temp_dir.path().join("session.settings.json");

    fs::write(&jsonl_path, "").unwrap();
    fs::write(&settings_path, r#"{"model":"test-model"}"#).unwrap();

    let hook_input = json!({
        "cwd": "/Users/testuser/projects/testing-git",
        "hookEventName": "PreToolUse",
        "sessionId": "052cb8d0-4616-488a-99fe-bfbbbe9429b3",
        "toolName": "ApplyPatch",
        "tool_input": {
            "file_path": "/Users/testuser/projects/testing-git/index.ts"
        },
        "transcriptPath": jsonl_path.to_str().unwrap()
    })
    .to_string();

    let events = parse_droid(&hook_input).expect("Failed to parse droid hook input");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(
                e.context.cwd,
                PathBuf::from("/Users/testuser/projects/testing-git")
            );
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "will_edit_filepaths should contain the target file"
            );
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

#[test]
fn test_droid_settings_missing_model_field() {
    let mut temp = NamedTempFile::new().unwrap();
    temp.write_all(b"{}").unwrap();
    let content = fs::read_to_string(temp.path()).expect("Should read settings file");
    let settings: serde_json::Value = serde_json::from_str(&content).expect("Should parse JSON");
    let model = settings["model"].as_str().map(|s| s.to_string());
    assert!(model.is_none(), "Missing model field should return None");
}

crate::reuse_tests_in_worktree!(
    test_droid_raw_event_fidelity,
    test_parse_droid_settings_model,
    test_droid_preset_extracts_edited_filepath,
    test_droid_preset_extracts_applypatch_filepath,
    test_droid_preset_stores_metadata_paths,
    test_droid_preset_uses_raw_session_id,
    test_droid_e2e_prefers_latest_checkpoint_for_prompts,
    test_droid_preset_pretooluse_returns_human_checkpoint,
    test_droid_settings_missing_model_field,
);
