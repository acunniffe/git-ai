use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::ContinueAgent;
use git_ai::streams::watermark::RecordIndexWatermark;
use serde_json::json;
use std::fs;

fn parse_continue(hook_input: &str) -> Result<Vec<ParsedHookEvent>, git_ai::error::GitAiError> {
    resolve_preset("continue-cli")?.parse(hook_input, "t_test")
}

fn assert_no_ai_attestations(log: &AuthorshipLog) {
    assert!(
        log.attestations
            .iter()
            .flat_map(|attestation| &attestation.entries)
            .all(|entry| entry.hash == "human" || entry.hash.starts_with("h_")),
        "Human checkpoint should not create AI attestations: {:?}",
        log.attestations
    );
    assert!(
        log.metadata.prompts.is_empty() && log.metadata.sessions.is_empty(),
        "Human checkpoint should not create AI metadata: {:?}",
        log.metadata
    );
}

#[test]
fn test_continue_cli_raw_event_fidelity() {
    let fixture = fixture_path("continue-cli-session-simple.json");
    let agent = ContinueAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse continue-cli session JSON");

    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture).unwrap()).unwrap();
    let expected: Vec<serde_json::Value> = parsed["history"].as_array().unwrap().clone();

    assert_eq!(result.events, expected);
}

#[test]
fn test_continue_cli_preset_extracts_model_from_hook_input() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "claude-3.5-sonnet");
            assert_eq!(e.context.agent_id.tool, "continue-cli");
            assert_eq!(
                e.context.external_session_id,
                "2dbfd673-096d-4773-b5f3-9023894a7355"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_cli_preset_defaults_to_unknown_model() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "unknown");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_cli_preset_extracts_edited_filepath() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts"))
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_cli_preset_no_filepath_when_tool_input_missing() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_cli_preset_human_checkpoint() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PreToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts"))
            );
        }
        _ => panic!("Expected PreFileEdit for human checkpoint"),
    }
}

#[test]
fn test_continue_cli_preset_ai_checkpoint() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_some());
            assert!(!e.file_paths.is_empty());
        }
        _ => panic!("Expected PostFileEdit for AI checkpoint"),
    }
}

#[test]
fn test_continue_cli_preset_stores_transcript_path_in_metadata() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let events = parse_continue(&hook_input).expect("Failed to run ContinueCliPreset");

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.context.metadata.get("transcript_path"),
                Some(&"tests/fixtures/continue-cli-session-simple.json".to_string())
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_continue_cli_preset_handles_missing_transcript_path() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet"
    })
    .to_string();

    let result = parse_continue(&hook_input);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("transcript_path"));
}

#[test]
fn test_continue_cli_preset_handles_invalid_json() {
    let result = parse_continue("{ invalid json }");
    assert!(result.is_err());
}

#[test]
fn test_continue_cli_preset_handles_missing_session_id() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "model": "claude-3.5-sonnet",
        "transcript_path": "tests/fixtures/continue-cli-session-simple.json"
    })
    .to_string();

    let result = parse_continue(&hook_input);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("session_id"));
}

#[test]
fn test_continue_cli_preset_handles_missing_file() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "PostToolUse",
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "model": "claude-3.5-sonnet",
        "transcript_path": "tests/fixtures/nonexistent.json"
    })
    .to_string();

    // The new parse() API succeeds (transcript is lazy via StreamSource::Path)
    let events = parse_continue(&hook_input).expect("Parse should succeed with lazy transcript");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "claude-3.5-sonnet");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ============================================================================
// End-to-end tests using TestRepo
// ============================================================================

#[test]
fn test_continue_cli_e2e_with_attribution() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("continue-cli-session-simple.json")
        .to_string_lossy()
        .to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/index.ts");
    let base_content = "console.log('Bonjour');\n\nconsole.log('hello world');\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let edited_content =
        "console.log('Bonjour');\n\nconsole.log('hello world');\nconsole.log('hello world');\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = json!({
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    let result = repo
        .git_ai(&["checkpoint", "continue-cli", "--hook-input", &hook_input])
        .unwrap();

    println!("Checkpoint output: {}", result);

    let commit = repo.stage_all_and_commit("Add continue-cli edits").unwrap();

    let mut file = repo.filename("src/index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('Bonjour');".human(),
        "".human(),
        "console.log('hello world');".human(),
        "console.log('hello world');".ai(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have at least one attestation"
    );

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one session record in metadata"
    );

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have at least one session record");

    assert_eq!(
        session_record.agent_id.model, "claude-3.5-sonnet",
        "Model should be 'claude-3.5-sonnet'"
    );
}

#[test]
fn test_continue_cli_e2e_human_checkpoint() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("continue-cli-session-simple.json")
        .to_string_lossy()
        .to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/index.ts");
    let base_content = "console.log('hello');\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let hook_input = json!({
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    let result = repo
        .git_ai(&["checkpoint", "continue-cli", "--hook-input", &hook_input])
        .unwrap();

    println!("Checkpoint output: {}", result);

    let human_content = "console.log('hello');\nconsole.log('human edit');\n";
    fs::write(&file_path, human_content).unwrap();

    let commit = repo.stage_all_and_commit("Human edit").unwrap();

    let mut file = repo.filename("src/index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('hello');".human(),
        "console.log('human edit');".human(),
    ]);

    assert_no_ai_attestations(&commit.authorship_log);
}

#[test]
fn test_continue_cli_e2e_multiple_tool_calls() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("continue-cli-session-simple.json")
        .to_string_lossy()
        .to_string();

    let file_path = repo.path().join("test.ts");
    let base_content = "const x = 1;\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let edited_content = "const x = 1;\nconst y = 2;\nconst z = 3;\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = json!({
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "model": "claude-3.5-sonnet",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "continue-cli", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Add multiple lines").unwrap();

    let mut file = repo.filename("test.ts");
    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".ai(),
        "const z = 3;".ai(),
    ]);

    assert!(!commit.authorship_log.attestations.is_empty());
}

#[test]
fn test_continue_cli_e2e_preserves_model_on_commit() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("continue-cli-session-simple.json")
        .to_string_lossy()
        .to_string();

    let file_path = repo.path().join("test.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "const x = 1;\nconst y = 2;\n").unwrap();

    let hook_input = json!({
        "session_id": "2dbfd673-096d-4773-b5f3-9023894a7355",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "model": "claude-opus-4",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "continue-cli", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Add line").unwrap();

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have a session record");

    assert_eq!(
        session_record.agent_id.model, "claude-opus-4",
        "Model should be preserved from hook_input"
    );
    assert_eq!(session_record.agent_id.tool, "continue-cli");
}

crate::reuse_tests_in_worktree!(
    test_continue_cli_raw_event_fidelity,
    test_continue_cli_preset_extracts_model_from_hook_input,
    test_continue_cli_preset_defaults_to_unknown_model,
    test_continue_cli_preset_extracts_edited_filepath,
    test_continue_cli_preset_no_filepath_when_tool_input_missing,
    test_continue_cli_preset_human_checkpoint,
    test_continue_cli_preset_ai_checkpoint,
    test_continue_cli_preset_stores_transcript_path_in_metadata,
    test_continue_cli_preset_handles_missing_transcript_path,
    test_continue_cli_preset_handles_invalid_json,
    test_continue_cli_preset_handles_missing_session_id,
    test_continue_cli_preset_handles_missing_file,
    test_continue_cli_e2e_with_attribution,
    test_continue_cli_e2e_human_checkpoint,
    test_continue_cli_e2e_multiple_tool_calls,
    test_continue_cli_e2e_preserves_model_on_commit,
);
