use crate::repos::test_file::ExpectedLineExt;
use crate::test_utils::fixture_path;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use serde_json::json;
use std::fs;

fn parse_zcode(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("zcode")?.parse(hook_input, "t_test")
}

#[test]
fn test_zcode_preset_extracts_edited_filepath() {
    let hook_input = r##"{
        "transcript_path": "/tmp/zcode-claude-hook-abcd12/transcript.jsonl",
        "cwd": "/home/user/project",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "session_id": "sess-zcode-1",
        "tool_use_id": "tu-1",
        "tool_input": {"file_path": "src/main.rs"}
    }"##;

    let events = parse_zcode(hook_input).expect("parse should succeed");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "zcode");
            assert_eq!(e.context.agent_id.id, "sess-zcode-1");
            assert_eq!(
                e.file_paths,
                vec![std::path::PathBuf::from("/home/user/project/src/main.rs")]
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_zcode_preset_no_filepath_when_tool_input_missing() {
    let hook_input = r##"{
        "transcript_path": "/tmp/zcode-claude-hook-abcd12/transcript.jsonl",
        "cwd": "/home/user/project",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "session_id": "sess-zcode-1"
    }"##;

    let events = parse_zcode(hook_input).expect("parse should succeed");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => assert!(e.file_paths.is_empty()),
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_zcode_e2e_file_edit_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let src_dir = repo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.rs");
    fs::write(&file_path, "fn main() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // ZCode keeps live transcripts under ~/.zcode/cli/rollout, keyed by the
    // session id; TestRepo isolates HOME so this fixture is the only rollout.
    let session_id = "sess-zcode-e2e";
    let rollout_dir = repo
        .test_home_path()
        .join(".zcode")
        .join("cli")
        .join("rollout");
    fs::create_dir_all(&rollout_dir).unwrap();
    fs::copy(
        fixture_path("zcode-rollout-simple.jsonl"),
        rollout_dir.join(format!("model-io-sess_{session_id}.jsonl")),
    )
    .unwrap();

    let pre_hook_input = json!({
        "transcript_path": "/tmp/zcode-claude-hook-abcd12/transcript.jsonl",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "session_id": session_id,
        "tool_use_id": "tu-pre",
        "tool_input": {"file_path": file_path.to_string_lossy().to_string()}
    })
    .to_string();

    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &pre_hook_input])
        .expect("pre-hook checkpoint should succeed");

    fs::write(
        &file_path,
        "fn greet() { println!(\"hello\"); }\nfn main() { greet(); }\n",
    )
    .unwrap();

    let post_hook_input = json!({
        "transcript_path": "/tmp/zcode-claude-hook-abcd12/transcript.jsonl",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "session_id": session_id,
        "tool_use_id": "tu-pre",
        "tool_input": {"file_path": file_path.to_string_lossy().to_string()}
    })
    .to_string();

    repo.git_ai(&["checkpoint", "zcode", "--hook-input", &post_hook_input])
        .expect("post-hook checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Apply zcode refactor")
        .expect("commit should succeed");

    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected one session record from the zcode edit context"
    );

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    assert_eq!(session.agent_id.tool, "zcode");
    assert_eq!(session.agent_id.id, session_id);
    assert_eq!(
        session.agent_id.model, "GLM-5.3",
        "Model should be extracted from the zcode rollout transcript"
    );

    let mut tracked_file = repo.filename("src/main.rs");
    tracked_file.assert_lines_and_blame(crate::lines![
        "fn greet() { println!(\"hello\"); }".ai(),
        "fn main() { greet(); }".ai(),
    ]);
}

#[test]
fn test_zcode_e2e_bash_pre_and_post_tool_use_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("script.sh");
    fs::write(&file_path, "echo v1\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let session_id = "sess-zcode-bash";
    let rollout_dir = repo
        .test_home_path()
        .join(".zcode")
        .join("cli")
        .join("rollout");
    fs::create_dir_all(&rollout_dir).unwrap();
    fs::copy(
        fixture_path("zcode-rollout-simple.jsonl"),
        rollout_dir.join(format!("model-io-sess_{session_id}.jsonl")),
    )
    .unwrap();

    let make_bash_hook_input = |event: &str| {
        json!({
            "transcript_path": "/tmp/zcode-claude-hook-abcd12/transcript.jsonl",
            "cwd": repo_root.to_string_lossy().to_string(),
            "hook_event_name": event,
            "tool_name": "Bash",
            "session_id": session_id,
            "tool_use_id": "tu-bash",
            "tool_input": {"command": "python - <<'PY'\nprint('edit from zcode bash')\nPY"}
        })
        .to_string()
    };

    repo.git_ai(&[
        "checkpoint",
        "zcode",
        "--hook-input",
        &make_bash_hook_input("PreToolUse"),
    ])
    .expect("pre-hook checkpoint should succeed");

    fs::write(&file_path, "echo v1\necho v2 from bash\n").unwrap();

    repo.git_ai(&[
        "checkpoint",
        "zcode",
        "--hook-input",
        &make_bash_hook_input("PostToolUse"),
    ])
    .expect("post-hook checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Apply zcode bash edit")
        .expect("commit should succeed");

    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected one session record from the zcode bash context"
    );

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    assert_eq!(session.agent_id.tool, "zcode");

    let mut tracked_file = repo.filename("script.sh");
    tracked_file.assert_lines_and_blame(crate::lines![
        "echo v1".unattributed_human(),
        "echo v2 from bash".ai(),
    ]);
}
