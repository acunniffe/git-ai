use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::streams::agent::Agent;
use git_ai::streams::agents::WindsurfAgent;
use git_ai::streams::watermark::ByteOffsetWatermark;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::thread;
use std::time::Duration;

fn parse_windsurf(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("windsurf")?.parse(hook_input, "t_test")
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

// ============================================================================
// Preset routing tests
// ============================================================================

#[test]
fn test_windsurf_preset_human_checkpoint() {
    let hook_input = json!({
        "trajectory_id": "traj-abc-123",
        "agent_action_name": "pre_write_code",
        "model_name": "GPT 4.1",
        "tool_info": {
            "file_path": "/home/user/project/main.rs"
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("Failed to run WindsurfPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("main.rs")),
                "Should have will_edit_filepaths"
            );
            assert_eq!(e.context.agent_id.tool, "windsurf");
            assert_eq!(e.context.agent_id.id, "traj-abc-123");
            assert_eq!(e.context.agent_id.model, "GPT 4.1");
        }
        _ => panic!("Expected PreFileEdit for pre_write_code"),
    }
}

#[test]
fn test_windsurf_preset_ai_checkpoint_post_write_code() {
    let hook_input = json!({
        "trajectory_id": "traj-abc-123",
        "agent_action_name": "post_write_code",
        "tool_info": {
            "file_path": "/home/user/project/main.rs"
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("Failed to run WindsurfPreset");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(
                e.file_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("main.rs")),
                "Should have edited_filepaths"
            );
            assert!(e.stream_source.is_some());
            assert_eq!(e.context.agent_id.tool, "windsurf");
            // No model_name in hook input -> falls back to "unknown"
            assert_eq!(e.context.agent_id.model, "unknown");
        }
        _ => panic!("Expected PostFileEdit for post_write_code"),
    }
}

#[test]
fn test_windsurf_preset_extracts_model_name_from_hook() {
    let hook_input = json!({
        "trajectory_id": "traj-abc-123",
        "agent_action_name": "post_write_code",
        "model_name": "Claude Sonnet 4",
        "tool_info": {
            "file_path": "/home/user/project/main.rs"
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("Failed to run WindsurfPreset");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.model, "Claude Sonnet 4");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_windsurf_preset_ignores_unknown_model_name() {
    let hook_input = json!({
        "trajectory_id": "traj-abc-123",
        "agent_action_name": "post_write_code",
        "model_name": "Unknown",
        "tool_info": {
            "file_path": "/home/user/project/main.rs"
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("Failed to run WindsurfPreset");
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            // "Unknown" (capital U) is filtered to "unknown" so transcript-based model
            // resolution can override it downstream in checkpoint.rs
            assert_eq!(e.context.agent_id.model, "unknown");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_windsurf_preset_ai_checkpoint_post_cascade() {
    let mut temp_file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        temp_file,
        r#"{{"status":"done","type":"user_input","user_input":{{"user_response":"Hello AI"}}}}"#
    )
    .unwrap();
    writeln!(temp_file, r#"{{"planner_response":{{"response":"I will help you"}},"status":"done","type":"planner_response"}}"#).unwrap();
    let temp_path = temp_file.path().to_str().unwrap().to_string();

    let hook_input = json!({
        "trajectory_id": "traj-abc-123",
        "agent_action_name": "post_cascade_response_with_transcript",
        "tool_info": {
            "transcript_path": temp_path
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("Failed to run WindsurfPreset");
    assert_eq!(events.len(), 1);
    // post_cascade_response_with_transcript is an AI checkpoint variant
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.stream_source.is_some());
        }
        _ => panic!("Expected PostFileEdit for post_cascade_response_with_transcript"),
    }
}

#[test]
fn test_windsurf_preset_missing_trajectory_id() {
    let hook_input = json!({
        "agent_action_name": "post_write_code"
    })
    .to_string();

    let result = parse_windsurf(&hook_input);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("trajectory_id not found")
    );
}

#[test]
fn test_windsurf_preset_invalid_json() {
    let result = parse_windsurf("{ invalid json }");
    assert!(result.is_err());
}

// ============================================================================
// Transcript parser tests
// ============================================================================

#[test]
fn test_windsurf_raw_event_fidelity() {
    let fixture = crate::test_utils::fixture_path("windsurf-session-simple.jsonl");
    let agent = WindsurfAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse windsurf JSONL");

    let expected: Vec<serde_json::Value> = std::fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
fn test_windsurf_transcript_parser_handles_malformed_lines() {
    let mut temp_file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        temp_file,
        r#"{{"status":"done","type":"user_input","user_input":{{"user_response":"Hello"}}}}"#
    )
    .unwrap();
    writeln!(temp_file, "not valid json at all").unwrap();
    writeln!(temp_file, r#"{{"planner_response":{{"response":"Hi there"}},"status":"done","type":"planner_response"}}"#).unwrap();

    let agent = WindsurfAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent.read_incremental(temp_file.path(), watermark, "test");

    // Malformed JSON lines are skipped; valid lines before and after are returned
    let batch = result.expect("malformed lines should be skipped, not cause errors");
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.events[0]["type"].as_str(), Some("user_input"));
    assert_eq!(batch.events[1]["type"].as_str(), Some("planner_response"));
}

#[test]
fn test_windsurf_transcript_parser_empty_file() {
    let temp_file = tempfile::NamedTempFile::new().unwrap();

    let agent = WindsurfAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(temp_file.path(), watermark, "test")
        .expect("Failed to parse empty JSONL");

    assert!(result.events.is_empty());
}

// ============================================================================
// End-to-end tests using TestRepo
// ============================================================================

#[test]
fn test_windsurf_e2e_with_attribution() {
    let repo = TestRepo::new();

    let mut temp_transcript = tempfile::NamedTempFile::new().unwrap();
    writeln!(temp_transcript, r#"{{"status":"done","type":"user_input","user_input":{{"user_response":"add a greeting"}}}}"#).unwrap();
    writeln!(temp_transcript, r#"{{"planner_response":{{"response":"I'll add a greeting line."}},"status":"done","type":"planner_response"}}"#).unwrap();
    writeln!(temp_transcript, r#"{{"code_action":{{"path":"file:///index.ts","new_content":"console.log('hi');"}},"status":"done","type":"code_action"}}"#).unwrap();
    let transcript_path = temp_transcript.path().to_str().unwrap().to_string();

    let file_path = repo.path().join("index.ts");
    fs::write(&file_path, "console.log('hello');\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    fs::write(&file_path, "console.log('hello');\nconsole.log('hi');\n").unwrap();

    let hook_input = json!({
        "trajectory_id": "traj-001",
        "agent_action_name": "post_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string(),
            "transcript_path": transcript_path
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &hook_input])
        .unwrap();

    let commit = repo.stage_all_and_commit("Add windsurf edit").unwrap();

    let mut file = repo.filename("index.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('hello');".human(),
        "console.log('hi');".ai(),
    ]);

    assert!(!commit.authorship_log.attestations.is_empty());
    assert!(!commit.authorship_log.metadata.sessions.is_empty());

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have a session record");

    assert_eq!(session_record.agent_id.tool, "windsurf");
}

#[test]
fn test_windsurf_e2e_human_checkpoint() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("index.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let hook_input = json!({
        "trajectory_id": "traj-002",
        "agent_action_name": "pre_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &hook_input])
        .unwrap();

    fs::write(&file_path, "const x = 1;\nconst y = 2;\n").unwrap();

    let commit = repo.stage_all_and_commit("Human edit").unwrap();

    let mut file = repo.filename("index.ts");
    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".human(),
    ]);

    assert_no_ai_attestations(&commit.authorship_log);
}

// ============================================================================
// run_command (bash) hook tests
// ============================================================================

#[test]
fn test_windsurf_preset_pre_run_command_captures_bash_snapshot() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let hook_input = json!({
        "trajectory_id": "traj-bash-pre",
        "execution_id": "exec-bash-1",
        "agent_action_name": "pre_run_command",
        "model_name": "GPT 4.1",
        "tool_info": {
            "command_line": "git status --short",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();

    let events = parse_windsurf(&hook_input).expect("pre_run_command should run");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "windsurf");
            assert_eq!(e.context.agent_id.id, "traj-bash-pre");
            assert_eq!(e.context.agent_id.model, "GPT 4.1");
            assert_eq!(e.tool_use_id, "exec-bash-1");
        }
        _ => panic!("Expected PreBashCall for pre_run_command"),
    }
}

#[test]
fn test_windsurf_preset_post_run_command_detects_changed_files() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Pre-run command via CLI (need snapshot captured first)
    let pre_hook_input = json!({
        "trajectory_id": "traj-bash-post",
        "execution_id": "exec-bash-2",
        "agent_action_name": "pre_run_command",
        "tool_info": {
            "command_line": "echo changed >> src/main.rs",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &pre_hook_input])
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    fs::write(&file_path, "fn main() { println!(\"hi\"); }\n").unwrap();

    let post_hook_input = json!({
        "trajectory_id": "traj-bash-post",
        "execution_id": "exec-bash-2",
        "agent_action_name": "post_run_command",
        "tool_info": {
            "command_line": "echo changed >> src/main.rs",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();

    // Post-run also via CLI since the bash tool state is in the repo
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &post_hook_input])
        .unwrap();

    // Verify that files were changed (commit and check attribution)
    let commit = repo.stage_all_and_commit("Post run command edit").unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "post_run_command should produce AI attestations"
    );
}

#[test]
fn test_windsurf_preset_post_run_command_without_snapshot_falls_back_gracefully() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    // No pre_run_command hook fired -- snapshot is missing.
    let hook_input = json!({
        "trajectory_id": "traj-orphan-post",
        "execution_id": "exec-orphan",
        "agent_action_name": "post_run_command",
        "tool_info": {
            "command_line": "pwd",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();

    // Use CLI to ensure it doesn't error
    let result = repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &hook_input]);
    assert!(
        result.is_ok(),
        "orphan post_run_command should not error: {:?}",
        result.err()
    );
}

#[test]
fn test_windsurf_e2e_run_command_attribution() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();

    let file_path = repo_root.join("index.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let pre_hook = json!({
        "trajectory_id": "traj-e2e-bash",
        "execution_id": "exec-e2e-1",
        "agent_action_name": "pre_run_command",
        "tool_info": {
            "command_line": "sed -i '' 's/1;/2;/' index.ts",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &pre_hook])
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    fs::write(&file_path, "const x = 2;\n").unwrap();

    let post_hook = json!({
        "trajectory_id": "traj-e2e-bash",
        "execution_id": "exec-e2e-1",
        "agent_action_name": "post_run_command",
        "tool_info": {
            "command_line": "sed -i '' 's/1;/2;/' index.ts",
            "cwd": repo_root.to_string_lossy().to_string(),
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &post_hook])
        .unwrap();

    let commit = repo.stage_all_and_commit("Windsurf bash edit").unwrap();

    let mut file = repo.filename("index.ts");
    file.assert_lines_and_blame(crate::lines!["const x = 2;".ai()]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "run_command edits should produce AI attestations"
    );
}

// ============================================================================
// Checkpoint race condition tests
// ============================================================================

/// Simulates the Windsurf race condition where the IDE fires a KnownHuman
/// checkpoint between the pre-edit (WillEdit) and post-edit (Edited) AI
/// checkpoints. The KnownHuman should be suppressed because the file has
/// a pending AI edit in-flight.
#[test]
fn test_windsurf_known_human_suppressed_during_pending_ai_edit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("race.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 1: Pre-edit checkpoint via Windsurf preset (pretooluse).
    // This has agent_id + WillEdit path_role, registering pending AI edit state.
    let pre_hook = json!({
        "trajectory_id": "traj-race-001",
        "agent_action_name": "pre_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &pre_hook])
        .unwrap();

    // Step 2: Windsurf edits the file.
    fs::write(&file_path, "original line\nAI added line\n").unwrap();

    // Step 3: VS Code extension fires KnownHuman (spurious IDE save event).
    // This should be suppressed because race.txt has a pending AI edit.
    repo.git_ai(&["checkpoint", "mock_known_human", "race.txt"])
        .unwrap();

    // Step 4: Windsurf fires posttooluse (PostFileEdit / AI checkpoint).
    let post_hook = json!({
        "trajectory_id": "traj-race-001",
        "agent_action_name": "post_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &post_hook])
        .unwrap();

    // Commit and verify attribution.
    repo.stage_all_and_commit("Windsurf race edit").unwrap();
    let mut file = repo.filename("race.txt");
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "AI added line".ai(),
    ]);
}

/// Verifies that KnownHuman checkpoints still work when there is NO pending
/// AI edit. This ensures the suppression logic doesn't over-suppress.
#[test]
fn test_known_human_not_suppressed_without_pending_ai_edit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("human_edit.txt");

    fs::write(&file_path, "first line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Genuine human edit — no pre-edit AI checkpoint was fired.
    fs::write(&file_path, "first line\nhuman added line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "human_edit.txt"])
        .unwrap();

    repo.stage_all_and_commit("Human edit").unwrap();
    let mut file = repo.filename("human_edit.txt");
    file.assert_committed_lines(crate::lines![
        "first line".unattributed_human(),
        "human added line".human(),
    ]);
}

/// Verifies that after the AI post-edit checkpoint completes (clearing pending
/// state), subsequent KnownHuman checkpoints on the same file are no longer
/// suppressed.
#[test]
fn test_known_human_works_after_ai_edit_completes() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("mixed.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Full AI edit cycle via Windsurf preset: pre-edit → edit → post-edit.
    let pre_hook = json!({
        "trajectory_id": "traj-mixed-001",
        "agent_action_name": "pre_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &pre_hook])
        .unwrap();
    fs::write(&file_path, "base\nai line\n").unwrap();
    let post_hook = json!({
        "trajectory_id": "traj-mixed-001",
        "agent_action_name": "post_write_code",
        "tool_info": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "windsurf", "--hook-input", &post_hook])
        .unwrap();

    // Now a genuine human edit after the AI edit completed.
    fs::write(&file_path, "base\nai line\nhuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "mixed.txt"])
        .unwrap();

    repo.stage_all_and_commit("Mixed edit").unwrap();
    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "ai line".ai(),
        "human line".human(),
    ]);
}
