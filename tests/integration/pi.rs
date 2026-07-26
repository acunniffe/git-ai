use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use git_ai::model::working_log::{Checkpoint, CheckpointKind};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn copy_fixture_to_temp(name: &str) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().unwrap();
    let session_path = temp_dir.path().join(name);
    fs::copy(fixture_path(name), &session_path).unwrap();
    (temp_dir, session_path)
}

fn read_checkpoints(repo: &TestRepo) -> Vec<Checkpoint> {
    let working_logs_dir = repo.path().join(".git").join("ai").join("working_logs");
    let mut checkpoints = Vec::new();

    let entries = fs::read_dir(&working_logs_dir).expect("working_logs directory should exist");
    for entry in entries.filter_map(|entry| entry.ok()) {
        let checkpoints_path = entry.path().join("checkpoints.jsonl");
        if !checkpoints_path.exists() {
            continue;
        }

        let content = fs::read_to_string(&checkpoints_path).expect("read checkpoints.jsonl");
        checkpoints.extend(
            content
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str::<Checkpoint>(line).expect("parse checkpoint line")
                }),
        );
    }

    checkpoints
}

fn append_assistant_message(session_path: &Path, provider: &str, model: &str, text: &str) {
    let content = fs::read_to_string(session_path).expect("read Pi session fixture");
    let next_parent = content
        .lines()
        .rev()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            value.get("id")?.as_str().map(|id| id.to_string())
        })
        .expect("session should contain at least one entry");

    let new_entry = json!({
        "type": "message",
        "id": "ffff6666",
        "parentId": next_parent,
        "timestamp": "2026-03-31T11:00:00.000Z",
        "message": {
            "role": "assistant",
            "provider": provider,
            "model": model,
            "content": [
                {"type": "text", "text": text}
            ],
            "timestamp": 1774954800000i64
        }
    });

    let mut next_content = content;
    if !next_content.ends_with('\n') {
        next_content.push('\n');
    }
    next_content.push_str(&serde_json::to_string(&new_entry).unwrap());
    next_content.push('\n');
    fs::write(session_path, next_content).expect("append assistant message");
}

#[test]
#[ignore] // DISABLED: transcript enrichment removed
#[serial_test::serial]
fn test_pi_before_edit_checkpoint_via_cli_creates_human_checkpoint() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let (_temp_dir, session_path) = copy_fixture_to_temp("pi-session-simple.jsonl");

    let hook_input = json!({
        "hook_event_name": "before_edit",
        "session_id": "pi-session-123",
        "session_path": session_path,
        "cwd": repo_root.to_string_lossy().to_string(),
        "model": "anthropic/claude-sonnet-4-5",
        "tool_name": "edit",
        "tool_name_raw": "edit",
        "will_edit_filepaths": [file_path.to_string_lossy().to_string()],
        "dirty_files": {
            file_path.to_string_lossy().to_string(): "fn main() {}\n"
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("pi", &hook_input).unwrap();

    fs::write(&file_path, "fn main() { println!(\"human\"); }\n").unwrap();
    let commit = repo.stage_all_and_commit("Human Pi edit").unwrap();

    let mut file = repo.filename("src/main.rs");
    file.assert_lines_and_blame(crate::lines!["fn main() { println!(\"human\"); }".human(),]);
    assert_eq!(
        commit.authorship_log.attestations.len(),
        0,
        "Pi before_edit checkpoints should keep subsequent manual edits human-authored"
    );
}

#[test]
#[ignore] // DISABLED: transcript enrichment removed
#[serial_test::serial]
fn test_pi_after_edit_checkpoint_via_cli_creates_ai_checkpoint() {
    let repo = TestRepo::new();
    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let (_temp_dir, session_path) = copy_fixture_to_temp("pi-session-tool.jsonl");
    fs::write(&file_path, "fn main() { println!(\"pi\"); }\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "session_id": "pi-session-456",
        "session_path": session_path,
        "cwd": repo_root.to_string_lossy().to_string(),
        "model": "",
        "tool_name": "edit",
        "tool_name_raw": "edit",
        "edited_filepaths": [file_path.to_string_lossy().to_string()],
        "dirty_files": {
            file_path.to_string_lossy().to_string(): "fn main() { println!(\"pi\"); }\n"
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("pi", &hook_input).unwrap();
    repo.sync_daemon_force();

    let checkpoints = read_checkpoints(&repo);
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].kind, CheckpointKind::AiAgent);
    // At checkpoint time, model stays "unknown" — file-based transcript reads
    // are skipped during checkpointing. Model resolution happens at commit time
    // via update_prompts_to_latest.
    assert_eq!(checkpoints[0].agent_id.as_ref().unwrap().model, "unknown");
    // Transcript field removed from Checkpoint struct
    assert_eq!(
        checkpoints[0]
            .agent_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("tool_name_raw"))
            .map(String::as_str),
        Some("edit")
    );

    // After commit, post-commit hook reads the transcript file and resolves the model
    let commit = repo.stage_all_and_commit("pi edit").unwrap();
    let sessions: Vec<_> = commit.authorship_log.metadata.sessions.values().collect();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].agent_id.model, "claude-sonnet-4-5",
        "Model should be resolved from transcript at commit time"
    );
}

#[test]
#[ignore] // DISABLED: transcript enrichment removed
#[serial_test::serial]
fn test_pi_post_commit_resyncs_latest_session_transcript() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.rs");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let (_temp_dir, session_path) = copy_fixture_to_temp("pi-session-simple.jsonl");
    fs::write(&file_path, "fn main() { println!(\"updated\"); }\n").unwrap();

    let hook_input = json!({
        "hook_event_name": "after_edit",
        "session_id": "pi-session-123",
        "session_path": session_path,
        "cwd": repo_root.to_string_lossy().to_string(),
        "model": "",
        "tool_name": "edit",
        "tool_name_raw": "edit",
        "edited_filepaths": [file_path.to_string_lossy().to_string()],
        "dirty_files": {
            file_path.to_string_lossy().to_string(): "fn main() { println!(\"updated\"); }\n"
        }
    })
    .to_string();

    repo.checkpoint_with_hook_input("pi", &hook_input).unwrap();

    append_assistant_message(
        &session_path,
        "openai",
        "gpt-5",
        "RESYNC_TEST_MESSAGE: Pi prompt refresh appended this assistant update.",
    );

    let commit = repo
        .stage_all_and_commit("Commit with Pi transcript resync")
        .unwrap();

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("expected a session record");

    assert_eq!(session_record.agent_id.tool, "pi");
    assert_eq!(session_record.agent_id.model, "gpt-5");
    // Note: Messages field has been removed from SessionRecord
}
