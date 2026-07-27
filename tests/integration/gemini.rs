use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::GeminiAgent;
use git_ai::streams::watermark::ByteOffsetWatermark;
use serde_json::json;
use std::fs;

fn parse_gemini(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("gemini")?.parse(hook_input, "t_test")
}

#[test]
fn test_gemini_raw_event_fidelity() {
    let fixture = fixture_path("gemini-session-simple.jsonl");
    let agent = GeminiAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .unwrap();

    let content = std::fs::read_to_string(&fixture).unwrap();
    let expected: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
fn test_gemini_preset_extracts_edited_filepath() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(!e.file_paths.is_empty());
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "Should contain edited filepath"
            );
        }
        _ => panic!("Expected PostFileEdit for AfterTool"),
    }
}

#[test]
fn test_gemini_preset_no_filepath_when_tool_input_missing() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(
                e.file_paths.is_empty(),
                "edited_filepaths should be empty when tool_input is missing"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_gemini_preset_human_checkpoint() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "BeforeTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("index.ts")),
                "Should have will_edit_filepaths"
            );
        }
        _ => panic!("Expected PreFileEdit for BeforeTool"),
    }
}

#[test]
fn test_gemini_preset_ai_checkpoint() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "tool_input": {
            "file_path": "/Users/svarlamov/projects/testing-git/index.ts"
        },
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_some(), "Should have transcript");
            assert!(!e.file_paths.is_empty(), "Should have edited_filepaths");
        }
        _ => panic!("Expected PostFileEdit for AfterTool"),
    }
}

#[test]
fn test_gemini_preset_extracts_model() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "gemini-2.5-flash");
            assert_eq!(e.context.agent_id.tool, "gemini");
            assert_eq!(
                e.context.agent_id.id,
                "18f475c0-690f-4bc9-b84e-88a0a1e9518f"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_gemini_preset_stores_transcript_path_in_metadata() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let events = parse_gemini(&hook_input).expect("Failed to run GeminiPreset");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.context.metadata.get("transcript_path"),
                Some(&"tests/fixtures/gemini-session-simple.jsonl".to_string())
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_gemini_preset_handles_missing_transcript_path() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f"
    })
    .to_string();

    let result = parse_gemini(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("transcript_path not found")
    );
}

#[test]
fn test_gemini_preset_handles_invalid_json() {
    let result = parse_gemini("{ invalid json }");
    assert!(result.is_err());
}

#[test]
fn test_gemini_preset_handles_missing_session_id() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "transcript_path": "tests/fixtures/gemini-session-simple.jsonl"
    })
    .to_string();

    let result = parse_gemini(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("session_id not found")
    );
}

#[test]
fn test_gemini_preset_handles_missing_file() {
    let hook_input = json!({
        "cwd": "/Users/svarlamov/projects/testing-git",
        "hook_event_name": "AfterTool",
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "transcript_path": "tests/fixtures/nonexistent.jsonl"
    })
    .to_string();

    let result = parse_gemini(&hook_input);
    assert!(result.is_ok());
    let events = result.unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "unknown");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

// ============================================================================
// End-to-end tests using TestRepo
// ============================================================================

#[test]
fn test_gemini_e2e_with_attribution() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/index.ts");
    let base_content = "console.log('Bonjour');\n\nconsole.log('hello world');\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let edited_content =
        "console.log('Bonjour');\n\nconsole.log('hello world');\nconsole.log('hello bob');\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = json!({
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "AfterTool",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Add gemini edits").unwrap();

    let mut file = repo.filename("src/index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('Bonjour');".human(),
        "".human(),
        "console.log('hello world');".human(),
        "console.log('hello bob');".ai(),
    ]);

    assert!(!commit.authorship_log.attestations.is_empty());
    assert!(!commit.authorship_log.metadata.sessions.is_empty());

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have at least one session record");

    assert_eq!(session_record.agent_id.model, "gemini-2.5-flash");
}

#[test]
fn test_gemini_e2e_human_checkpoint() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/index.ts");
    fs::write(&file_path, "console.log('hello');\n").unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let hook_input = json!({
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "BeforeTool",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &hook_input])
        .unwrap();

    fs::write(
        &file_path,
        "console.log('hello');\nconsole.log('human edit');\n",
    )
    .unwrap();

    let commit = repo.stage_all_and_commit("Human edit").unwrap();

    let mut file = repo.filename("src/index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('hello');".human(),
        "console.log('human edit');".human(),
    ]);

    assert_eq!(commit.authorship_log.attestations.len(), 0);
}

#[test]
fn test_issue_1951_gemini_ignores_internal_files() {
    let repo = TestRepo::new();
    let tracked_path = repo.path().join("tracked.txt");
    fs::write(&tracked_path, "tracked\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut tracked_file = repo.filename("tracked.txt");
    tracked_file.assert_committed_lines(crate::lines!["tracked".unattributed_human()]);

    let gemini_home = tempfile::tempdir().unwrap();
    let internal_path = gemini_home
        .path()
        .join(".gemini/tmp/project/memory/skills/example/SKILL.md");
    fs::create_dir_all(internal_path.parent().unwrap()).unwrap();
    fs::write(&internal_path, "internal memory\n").unwrap();

    for hook_event_name in ["BeforeTool", "AfterTool"] {
        let hook_input = json!({
            "session_id": "gemini-internal-file-session",
            "cwd": repo.canonical_path().to_string_lossy().to_string(),
            "hook_event_name": hook_event_name,
            "tool_name": "write_file",
            "tool_input": {
                "file_path": internal_path.to_string_lossy().to_string()
            },
            "transcript_path": fixture_path("gemini-session-simple.jsonl")
                .to_string_lossy()
                .to_string(),
        })
        .to_string();

        let output = repo
            .git_ai_with_env(
                &["checkpoint", "gemini", "--hook-input", &hook_input],
                &[("GEMINI_CLI_HOME", gemini_home.path().to_str().unwrap())],
            )
            .unwrap();
        assert!(
            output.is_empty(),
            "Gemini internal file hooks should be ignored silently, got: {output}"
        );
    }

    assert!(
        repo.current_working_logs()
            .all_ai_touched_files()
            .unwrap_or_default()
            .is_empty(),
        "Gemini internal files should not create repository checkpoints"
    );
}

#[test]
fn test_gemini_e2e_multiple_tool_calls() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();

    let file_path = repo.path().join("test.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "const x = 1;\nconst y = 2;\nconst z = 3;\n").unwrap();

    let hook_input = json!({
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "AfterTool",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &hook_input])
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
fn test_gemini_e2e_with_resync() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();

    let file_path = repo.path().join("test.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "const x = 1;\nconst y = 2;\n").unwrap();

    let hook_input = json!({
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "AfterTool",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Add gemini edits").unwrap();

    let mut file = repo.filename("test.ts");
    file.assert_lines_and_blame(crate::lines!["const x = 1;".human(), "const y = 2;".ai(),]);

    assert!(!commit.authorship_log.metadata.sessions.is_empty());

    let _session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have at least one session record");
}

#[test]
fn test_gemini_e2e_partial_staging() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();

    let file_path = repo.path().join("test.ts");
    fs::write(&file_path, "line1\nline2\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "line1\nline2\nai_line3\nai_line4\n").unwrap();

    repo.git(&["add", "test.ts"]).unwrap();

    fs::write(&file_path, "line1\nline2\nai_line3\nai_line4\nai_line5\n").unwrap();

    let hook_input = json!({
        "session_id": "18f475c0-690f-4bc9-b84e-88a0a1e9518f",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "AfterTool",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": fixture_path_str
    })
    .to_string();

    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.commit("Partial staging").unwrap();

    assert!(!commit.authorship_log.attestations.is_empty());

    let mut file = repo.filename("test.ts");
    file.assert_committed_lines(crate::lines![
        "line1".human(),
        "line2".human(),
        "ai_line3".ai(),
        "ai_line4".ai(),
    ]);
}

#[test]
fn test_gemini_preset_bash_tool_aftertool_detects_changes() {
    let repo = TestRepo::new();
    let fixture_path_str = fixture_path("gemini-session-simple.jsonl")
        .to_string_lossy()
        .to_string();
    let cwd = repo.canonical_path().to_string_lossy().to_string();
    let session_id = "gemini-bash-test-session";
    let tool_use_id = "tool-call-001";

    let file_path = repo.path().join("script.sh");
    fs::write(&file_path, "#!/bin/sh\necho hello\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let pre_hook_input = json!({
        "session_id": session_id,
        "tool_use_id": tool_use_id,
        "cwd": cwd,
        "hook_event_name": "BeforeTool",
        "tool_name": "shell",
        "tool_input": { "command": "echo modified > output.txt" },
        "transcript_path": fixture_path_str,
    })
    .to_string();
    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &pre_hook_input])
        .unwrap();

    let output_path = repo.path().join("output.txt");
    fs::write(&output_path, "modified\n").unwrap();

    let post_hook_input = json!({
        "session_id": session_id,
        "tool_use_id": tool_use_id,
        "cwd": cwd,
        "hook_event_name": "AfterTool",
        "tool_name": "shell",
        "tool_input": { "command": "echo modified > output.txt" },
        "transcript_path": fixture_path_str,
    })
    .to_string();
    repo.git_ai(&["checkpoint", "gemini", "--hook-input", &post_hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Gemini bash edit").unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "AfterTool with shell should produce AI attestations"
    );
}

crate::reuse_tests_in_worktree!(
    test_gemini_raw_event_fidelity,
    test_gemini_preset_extracts_edited_filepath,
    test_gemini_preset_no_filepath_when_tool_input_missing,
    test_gemini_preset_human_checkpoint,
    test_gemini_preset_ai_checkpoint,
    test_gemini_preset_extracts_model,
    test_gemini_preset_stores_transcript_path_in_metadata,
    test_gemini_preset_handles_missing_transcript_path,
    test_gemini_preset_handles_invalid_json,
    test_gemini_preset_handles_missing_session_id,
    test_gemini_preset_handles_missing_file,
    test_gemini_e2e_with_attribution,
    test_gemini_e2e_human_checkpoint,
    test_issue_1951_gemini_ignores_internal_files,
    test_gemini_e2e_multiple_tool_calls,
    test_gemini_e2e_with_resync,
    test_gemini_e2e_partial_staging,
    test_gemini_preset_bash_tool_aftertool_detects_changes,
);
