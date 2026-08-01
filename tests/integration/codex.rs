use crate::repos::test_file::ExpectedLineExt;
use crate::test_utils::{CodexHookInput, checkpoint_codex, fixture_path};
use git_ai::error::GitAiError;
use git_ai::model::stream_watermark::ByteOffsetWatermark;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::CodexAgent;
use serde_json::json;
use std::fs;

fn parse_codex(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("codex")?.parse(hook_input, "t_test")
}

#[test]
fn test_codex_raw_event_fidelity() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let agent = CodexAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse codex JSONL");

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
fn test_codex_preset_structured_hook_input() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let hook_input = json!({
        "session_id": "session-abc-123",
        "cwd": "/Users/test/projects/git-ai",
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-1",
        "triggered_at": "2026-02-11T05:53:33Z",
        "hook_event": {
            "event_type": "after_agent",
            "thread_id": "thread-xyz-999",
            "turn_id": "turn-2",
            "input_messages": ["Refactor src/main.rs"],
            "last_assistant_message": "Done."
        },
        "transcript_path": fixture.to_str().unwrap()
    })
    .to_string();

    let events = parse_codex(&hook_input).expect("Codex preset should run");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "codex");
            assert_eq!(
                e.context.external_session_id, "session-abc-123",
                "session_id should be preferred when present"
            );
            assert_eq!(
                e.context.cwd.to_string_lossy(),
                "/Users/test/projects/git-ai"
            );
            assert!(e.stream_source.is_some());
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_codex_preset_bash_pre_tool_use_skips_checkpoint_after_capturing_snapshot() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let hook_input = json!({
        "session_id": "session-bash-pre",
        "cwd": "/tmp/test-project",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-1",
        "tool_input": {
            "command": "git status --short"
        },
        "transcript_path": fixture.to_str().unwrap()
    })
    .to_string();

    // In the new parse API, bash PreToolUse returns PreBashCall with SnapshotOnly strategy
    // instead of returning an error. The caller handles the side effects.
    let events = parse_codex(&hook_input).expect("should succeed with PreBashCall");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "codex");
            assert_eq!(e.context.external_session_id, "session-bash-pre");
            assert_eq!(e.tool_use_id, "bash-use-1");
            assert!(
                e.context.metadata.contains_key("transcript_path"),
                "metadata should preserve transcript path for commit-time recovery"
            );
        }
        _ => panic!("Expected PreBashCall for bash PreToolUse"),
    }
}

#[test]
fn test_codex_preset_bash_pre_tool_use_supports_camel_case_hook_event_name() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let hook_input = json!({
        "session_id": "session-bash-pre-camel",
        "cwd": "/tmp/test-project",
        "hookEventName": "PreToolUse",
        "toolName": "Bash",
        "toolUseId": "bash-use-camel-1",
        "tool_input": {
            "command": "git status --short"
        },
        "transcript_path": fixture.to_str().unwrap()
    })
    .to_string();

    // Camel-case fields should work the same as snake_case
    let events = parse_codex(&hook_input).expect("should succeed with PreBashCall");
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "codex");
            assert_eq!(e.context.external_session_id, "session-bash-pre-camel");
            assert_eq!(e.tool_use_id, "bash-use-camel-1");
        }
        _ => panic!("Expected PreBashCall for camel-case PreToolUse"),
    }
}

#[test]
fn test_codex_preset_bash_post_tool_use_detects_changed_files() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let post_hook_input = json!({
        "session_id": "session-bash-post",
        "cwd": "/tmp/test-project",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-2",
        "tool_input": {
            "command": "perl -0pi -e 's/fn main\\(\\) \\{\\}/fn main\\(\\) { println!(\"hello\"); }/' src/main.rs"
        },
        "transcript_path": fixture.to_str().unwrap()
    })
    .to_string();

    let events = parse_codex(&post_hook_input).expect("Codex preset post-hook should run");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostBashCall(e) => {
            assert!(e.stream_source.is_some());
            assert_eq!(e.context.agent_id.tool, "codex");
            assert_eq!(e.context.external_session_id, "session-bash-post");
            assert_eq!(e.tool_use_id, "bash-use-2");
        }
        _ => panic!("Expected PostBashCall"),
    }
}

#[test]
fn test_find_rollout_path_for_session_in_home() {
    let fixture = fixture_path("codex-session-simple.jsonl");
    let temp = tempfile::tempdir().unwrap();

    let session_id = "019c4b43-1451-7af3-be4c-5576369bf1ba";
    let rollout_dir = temp.path().join("sessions/2026/02/11");
    fs::create_dir_all(&rollout_dir).unwrap();
    let rollout_path = rollout_dir.join(format!("rollout-2026-02-11T05-53-33-{session_id}.jsonl"));
    fs::copy(&fixture, &rollout_path).unwrap();

    let resolved = CodexAgent::find_rollout_path_for_session_in_home(session_id, temp.path())
        .expect("search should succeed")
        .expect("rollout should be found");

    assert_eq!(resolved, rollout_path);
}

#[test]
fn test_codex_commit_inside_bash_inflight_is_attributed_to_codex() {
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

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-rollout.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::pre_bash(
            "codex-bash-session",
            &repo_root,
            "bash-use-commit",
            "python - <<'PY'\nprint('commit from codex bash')\nPY",
        )
        .with_transcript_path(&transcript_path),
    );

    fs::write(
        &file_path,
        "fn greet() { println!(\"hello\"); }\nfn main() { greet(); }\n",
    )
    .unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::post_bash(
            "codex-bash-session",
            &repo_root,
            "bash-use-commit",
            "python - <<'PY'\nprint('commit from codex bash')\nPY",
        )
        .with_transcript_path(&transcript_path),
    );

    let commit = repo
        .stage_all_and_commit("Apply codex bash refactor")
        .expect("commit should succeed");

    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Expected one session record from the Codex bash context"
    );

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-bash-session");

    let mut tracked_file = repo.filename("src/main.rs");
    tracked_file.assert_lines_and_blame(crate::lines![
        "fn greet() { println!(\"hello\"); }".ai(),
        "fn main() { greet(); }".ai(),
    ]);
}

#[test]
fn test_codex_commit_inside_bash_inflight_repeated_append_keeps_file_ai() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    repo.human_edit("README.md", "Project README\n");
    let mut readme = repo.filename("README.md");
    repo.stage_all_and_commit("Initial README")
        .expect("initial README commit should succeed");

    let repo_root = repo.canonical_path();
    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-append-rollout.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-bash-append-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("pre-hook checkpoint should succeed");

    readme.set_contents(crate::lines!["Project README", "Updated by Codex".ai()]);

    let post_hook_input = json!({
        "session_id": "codex-bash-append-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex append proof")
        .expect("Codex append commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".human(),
        "Updated by Codex".ai(),
    ]);

    let second_pre_hook_input = json!({
        "session_id": "codex-bash-append-session-2",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-2",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof 2'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &second_pre_hook_input)
        .expect("second pre-hook checkpoint should succeed");

    readme.set_contents(crate::lines![
        "Project README",
        "Updated by Codex".ai(),
        "Updated again by Codex".ai(),
    ]);

    let second_post_hook_input = json!({
        "session_id": "codex-bash-append-session-2",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-2",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof 2'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &second_post_hook_input)
        .expect("second post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex append proof 2")
        .expect("second Codex append commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".human(),
        "Updated by Codex".ai(),
        "Updated again by Codex".ai(),
    ]);
}

#[test]
fn test_codex_file_edit_then_bash_pretooluse_does_not_steal_ai_commit_attribution() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["Project README"]);
    repo.stage_all_and_commit("Initial README").unwrap();

    let repo_root = repo.canonical_path();
    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-status-rollout.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-status-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-status",
        "tool_input": {
            "command": "echo 'Updated by live Codex proof' >> README.md"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("pre-hook checkpoint should succeed");

    fs::write(
        repo_root.join("README.md"),
        "Project README\nUpdated by live Codex proof\n",
    )
    .unwrap();

    let post_hook_input = json!({
        "session_id": "codex-status-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-status",
        "tool_input": {
            "command": "echo 'Updated by live Codex proof' >> README.md"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex status commit")
        .expect("Codex status commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".ai(),
        "Updated by live Codex proof".ai(),
    ]);
}

#[test]
fn test_codex_file_edit_then_camel_case_bash_pretooluse_does_not_steal_ai_commit_attribution() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["Project README"]);
    repo.stage_all_and_commit("Initial README").unwrap();

    let repo_root = repo.canonical_path();
    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-status-rollout-camel.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-status-session-camel",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hookEventName": "PreToolUse",
        "toolName": "Bash",
        "toolUseId": "bash-use-status-camel",
        "tool_input": {
            "command": "echo 'Updated by live Codex proof camel' >> README.md"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("pre-hook checkpoint should succeed");

    fs::write(
        repo_root.join("README.md"),
        "Project README\nUpdated by live Codex proof camel\n",
    )
    .unwrap();

    let post_hook_input = json!({
        "session_id": "codex-status-session-camel",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hookEventName": "PostToolUse",
        "toolName": "Bash",
        "toolUseId": "bash-use-status-camel",
        "tool_input": {
            "command": "echo 'Updated by live Codex proof camel' >> README.md"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex status camel commit")
        .expect("Codex status camel commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".ai(),
        "Updated by live Codex proof camel".ai(),
    ]);
}

#[test]
fn test_codex_read_only_bash_post_tool_use_before_edit_does_not_steal_commit_attribution() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["Project README"]);
    repo.stage_all_and_commit("Initial README").unwrap();

    let repo_root = repo.canonical_path();
    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-live-readonly-rollout.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let which_git_pre = json!({
        "session_id": "codex-live-readonly-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "which-git",
        "tool_input": { "command": "which git" },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.checkpoint_with_hook_input("codex", &which_git_pre)
        .expect("read-only pre-hook should succeed");

    let which_git_post = json!({
        "session_id": "codex-live-readonly-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "which-git",
        "tool_input": { "command": "which git" },
        "tool_response": "/usr/bin/git\n",
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.checkpoint_with_hook_input("codex", &which_git_post)
        .expect("read-only post-hook should succeed");

    let commit_pre = json!({
        "session_id": "codex-live-readonly-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "commit-bash",
        "tool_input": {
            "command": "git add README.md && git commit -m \"Codex readonly bash commit\""
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.checkpoint_with_hook_input("codex", &commit_pre)
        .expect("commit pre-hook should succeed");

    fs::write(
        repo_root.join("README.md"),
        "Project README\nUpdated after read-only bash\n",
    )
    .unwrap();

    let commit_post = json!({
        "session_id": "codex-live-readonly-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "commit-bash",
        "tool_input": {
            "command": "git add README.md && git commit -m \"Codex readonly bash commit\""
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    repo.checkpoint_with_hook_input("codex", &commit_post)
        .expect("commit post-hook should succeed");

    repo.stage_all_and_commit("Codex readonly bash commit")
        .expect("commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".ai(),
        "Updated after read-only bash".ai(),
    ]);
}

#[test]
fn test_codex_commit_inside_bash_inflight_repeated_append_keeps_file_ai_standard_human() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["Project README".unattributed_human()]);
    repo.stage_all_and_commit("Initial README")
        .expect("initial README commit should succeed");

    let repo_root = repo.canonical_path();
    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-append-rollout-standard-human.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-bash-append-session-sh",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-sh",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("pre-hook checkpoint should succeed");

    let readme_path = repo_root.join("README.md");
    fs::write(&readme_path, "Project README\nUpdated by Codex").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-bash-append-session-sh",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-sh",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex append proof")
        .expect("Codex append commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".ai(),
        "Updated by Codex".ai(),
    ]);

    let second_pre_hook_input = json!({
        "session_id": "codex-bash-append-session-2-sh",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-2-sh",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof 2'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &second_pre_hook_input)
        .expect("second pre-hook checkpoint should succeed");

    fs::write(
        &readme_path,
        "Project README\nUpdated by Codex\nUpdated again by Codex",
    )
    .unwrap();

    let second_post_hook_input = json!({
        "session_id": "codex-bash-append-session-2-sh",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-use-append-commit-2-sh",
        "tool_input": {
            "command": "git add README.md && git commit -m 'Codex append proof 2'"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &second_post_hook_input)
        .expect("second post-hook checkpoint should succeed");

    repo.stage_all_and_commit("Codex append proof 2")
        .expect("second Codex append commit should succeed");

    readme.assert_lines_and_blame(crate::lines![
        "Project README".ai(),
        "Updated by Codex".ai(),
        "Updated again by Codex".ai(),
    ]);
}

#[test]
fn test_codex_e2e_bash_pre_and_post_tool_use_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("app.py");
    fs::write(&file_path, "print('hello')\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-bash-full-cycle.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-bash-full-cycle",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-full-1",
        "tool_input": {
            "command": "sed -i '' 's/hello/world/' app.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("bash pre-hook should succeed");

    fs::write(&file_path, "print('world')\nprint('from codex')\n").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-bash-full-cycle",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-full-1",
        "tool_input": {
            "command": "sed -i '' 's/hello/world/' app.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("bash post-hook should succeed");

    let commit = repo
        .stage_all_and_commit("Codex bash edit")
        .expect("commit should succeed");

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-bash-full-cycle");

    let mut tracked_file = repo.filename("app.py");
    tracked_file.assert_lines_and_blame(crate::lines![
        "print('world')".ai(),
        "print('from codex')".ai(),
    ]);
}

#[test]
fn test_codex_e2e_apply_patch_file_edit_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("lib.rs");
    fs::write(&file_path, "fn old() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-apply-patch.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::pre_file_edit(
            "codex-apply-patch-session",
            &repo_root,
            "patch-1",
            &file_path,
        )
        .with_patch(format!(
            "*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n",
            file_path.to_string_lossy()
        ))
        .with_transcript_path(&transcript_path),
    );

    fs::write(&file_path, "fn new_func() {}\nfn helper() {}\n").unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::post_file_edit(
            "codex-apply-patch-session",
            &repo_root,
            "patch-1",
            &file_path,
        )
        .with_patch(format!(
            "*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n+fn helper() {{}}\n",
            file_path.to_string_lossy()
        ))
        .with_transcript_path(&transcript_path),
    );

    let commit = repo
        .stage_all_and_commit("Codex apply_patch edit")
        .expect("commit should succeed");

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-apply-patch-session");

    let mut tracked_file = repo.filename("lib.rs");
    tracked_file.assert_lines_and_blame(crate::lines![
        "fn new_func() {}".ai(),
        "fn helper() {}".ai(),
    ]);
}

#[test]
fn test_codex_e2e_apply_patch_scoped_to_edited_file_only() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_a = repo_root.join("a.txt");
    let file_b = repo_root.join("b.txt");
    fs::write(&file_a, "original a\n").unwrap();
    fs::write(&file_b, "original b\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-scoped-patch.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-scoped-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-scoped-1",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ original a\n+patched a\n", file_a.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("scoped pre-hook should succeed");

    fs::write(&file_a, "patched a\n").unwrap();
    fs::write(&file_b, "modified b outside codex\n").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-scoped-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-scoped-1",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ original a\n+patched a\n", file_a.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("scoped post-hook should succeed");

    repo.stage_all_and_commit("Scoped codex edit")
        .expect("commit should succeed");

    let mut fa = repo.filename("a.txt");
    fa.assert_lines_and_blame(crate::lines!["patched a".ai(),]);

    let mut fb = repo.filename("b.txt");
    fb.assert_lines_and_blame(crate::lines![
        "modified b outside codex".unattributed_human(),
    ]);
}

#[test]
fn test_codex_e2e_bash_then_apply_patch_in_same_session() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("main.py");
    fs::write(&file_path, "# starter\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-mixed.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::pre_bash(
            "codex-mixed-session",
            &repo_root,
            "bash-mixed-1",
            "echo 'setup step'",
        )
        .with_transcript_path(&transcript_path),
    );

    checkpoint_codex(
        &repo,
        CodexHookInput::post_bash(
            "codex-mixed-session",
            &repo_root,
            "bash-mixed-1",
            "echo 'setup step'",
        )
        .with_tool_response("setup step\n")
        .with_transcript_path(&transcript_path),
    );

    checkpoint_codex(
        &repo,
        CodexHookInput::pre_file_edit(
            "codex-mixed-session",
            &repo_root,
            "patch-mixed-1",
            &file_path,
        )
        .with_patch(format!(
            "*** Update File: {}\n@@ # starter\n+# updated by codex\n",
            file_path.to_string_lossy()
        ))
        .with_transcript_path(&transcript_path),
    );

    fs::write(&file_path, "# updated by codex\ndef main(): pass\n").unwrap();

    checkpoint_codex(
        &repo,
        CodexHookInput::post_file_edit(
            "codex-mixed-session",
            &repo_root,
            "patch-mixed-1",
            &file_path,
        )
        .with_patch(format!(
            "*** Update File: {}\n@@ # starter\n+# updated by codex\n+def main(): pass\n",
            file_path.to_string_lossy()
        ))
        .with_transcript_path(&transcript_path),
    );

    let commit = repo
        .stage_all_and_commit("Mixed codex edit")
        .expect("commit should succeed");

    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        1,
        "Both tool uses share the same session"
    );

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-mixed-session");

    let mut tracked_file = repo.filename("main.py");
    tracked_file.assert_lines_and_blame(crate::lines![
        "# updated by codex".ai(),
        "def main(): pass".ai(),
    ]);
}

#[test]
fn test_codex_e2e_bash_modifies_multiple_files_all_attributed() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_a = repo_root.join("src").join("a.rs");
    let file_b = repo_root.join("src").join("b.rs");
    fs::create_dir_all(repo_root.join("src")).unwrap();
    fs::write(&file_a, "// a\n").unwrap();
    fs::write(&file_b, "// b\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-multi-file.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook = json!({
        "session_id": "codex-multi-file-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-multi-1",
        "tool_input": {
            "command": "find src -name '*.rs' -exec sed -i '' 's/\\/\\//\\/\\/ modified/' {} +"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook)
        .expect("pre-hook should succeed");

    fs::write(&file_a, "// modified a\nfn a() {}\n").unwrap();
    fs::write(&file_b, "// modified b\nfn b() {}\n").unwrap();

    let post_hook = json!({
        "session_id": "codex-multi-file-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "bash-multi-1",
        "tool_input": {
            "command": "find src -name '*.rs' -exec sed -i '' 's/\\/\\//\\/\\/ modified/' {} +"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook)
        .expect("post-hook should succeed");

    repo.stage_all_and_commit("Codex multi-file bash edit")
        .expect("commit should succeed");

    let mut fa = repo.filename("src/a.rs");
    fa.assert_lines_and_blame(crate::lines!["// modified a".ai(), "fn a() {}".ai(),]);

    let mut fb = repo.filename("src/b.rs");
    fb.assert_lines_and_blame(crate::lines!["// modified b".ai(), "fn b() {}".ai(),]);
}

#[test]
fn test_codex_e2e_apply_patch_preserves_human_lines() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("config.toml");

    repo.human_edit("config.toml", "# human config\nkey = \"value\"\n");
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut config = repo.filename("config.toml");
    config.assert_committed_lines(crate::lines![
        "# human config".human(),
        "key = \"value\"".human(),
    ]);

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-preserve-human.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-preserve-human-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-preserve-1",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ key = \"value\"\n+new_key = \"ai_value\"\n", file_path.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("pre-hook should succeed");

    fs::write(
        &file_path,
        "# human config\nkey = \"value\"\nnew_key = \"ai_value\"\n",
    )
    .unwrap();

    let post_hook_input = json!({
        "session_id": "codex-preserve-human-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-preserve-1",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ key = \"value\"\n+new_key = \"ai_value\"\n", file_path.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("post-hook should succeed");

    repo.stage_all_and_commit("Codex appends to config")
        .expect("commit should succeed");

    config.assert_lines_and_blame(crate::lines![
        "# human config".human(),
        "key = \"value\"".human(),
        "new_key = \"ai_value\"".ai(),
    ]);
}

#[test]
fn test_codex_e2e_namespaced_apply_patch_file_edit_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("lib.rs");
    fs::write(&file_path, "fn old() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-apply-patch-namespaced.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-apply-patch-namespaced-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "functions.apply_patch",
        "tool_use_id": "patch-namespaced-1",
        "tool_input": {
            "command": format!("*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n", file_path.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("namespaced apply_patch pre-hook should succeed");

    fs::write(&file_path, "fn new_func() {}\nfn helper() {}\n").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-apply-patch-namespaced-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "functions.apply_patch",
        "tool_use_id": "patch-namespaced-1",
        "tool_input": {
            "command": format!("*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n+fn helper() {{}}\n", file_path.to_string_lossy())
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("namespaced apply_patch post-hook should succeed");

    let commit = repo
        .stage_all_and_commit("Codex namespaced apply_patch edit")
        .expect("commit should succeed");

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-apply-patch-namespaced-session");

    let mut tracked_file = repo.filename("lib.rs");
    tracked_file.assert_lines_and_blame(crate::lines![
        "fn new_func() {}".ai(),
        "fn helper() {}".ai(),
    ]);
}

#[test]
fn test_codex_e2e_namespaced_exec_command_bash_full_cycle() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("app.py");
    fs::write(&file_path, "print('hello')\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-exec-command-namespaced.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-exec-command-namespaced-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "functions.exec_command",
        "tool_use_id": "exec-namespaced-1",
        "tool_input": {
            "command": "sed -i '' 's/hello/world/' app.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("namespaced exec_command pre-hook should succeed");

    fs::write(&file_path, "print('world')\nprint('from codex')\n").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-exec-command-namespaced-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "functions.exec_command",
        "tool_use_id": "exec-namespaced-1",
        "tool_input": {
            "command": "sed -i '' 's/hello/world/' app.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("namespaced exec_command post-hook should succeed");

    let commit = repo
        .stage_all_and_commit("Codex namespaced exec_command edit")
        .expect("commit should succeed");

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-exec-command-namespaced-session");

    let mut tracked_file = repo.filename("app.py");
    tracked_file.assert_lines_and_blame(crate::lines![
        "print('world')".ai(),
        "print('from codex')".ai(),
    ]);
}

#[test]
fn test_codex_e2e_multi_tool_use_parallel_wrapper() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("main.rs");
    fs::write(&file_path, "fn old() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-parallel-wrapper.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    let pre_hook_input = json!({
        "session_id": "codex-parallel-wrapper-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "multi_tool_use.parallel",
        "tool_use_id": "parallel-1",
        "tool_input": {
            "tool_uses": [
                {
                    "name": "functions.apply_patch",
                    "arguments": {"command": format!("*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n", file_path.to_string_lossy())}
                },
                {
                    "name": "functions.exec_command",
                    "arguments": {"command": "echo 'parallel'"}
                }
            ]
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("multi_tool_use.parallel pre-hook should succeed");

    fs::write(&file_path, "fn new_func() {}\n").unwrap();

    let post_hook_input = json!({
        "session_id": "codex-parallel-wrapper-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "multi_tool_use.parallel",
        "tool_use_id": "parallel-1",
        "tool_input": {
            "tool_uses": [
                {
                    "name": "functions.apply_patch",
                    "arguments": {"command": format!("*** Update File: {}\n@@ fn old() {{}}\n+fn new_func() {{}}\n", file_path.to_string_lossy())}
                },
                {
                    "name": "functions.exec_command",
                    "arguments": {"command": "echo 'parallel'"}
                }
            ]
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("multi_tool_use.parallel post-hook should succeed");

    let commit = repo
        .stage_all_and_commit("Codex multi_tool_use.parallel edit")
        .expect("commit should succeed");

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-parallel-wrapper-session");

    let mut tracked_file = repo.filename("main.rs");
    tracked_file.assert_lines_and_blame(crate::lines!["fn new_func() {}".ai(),]);
}

crate::reuse_tests_in_worktree!(
    test_codex_raw_event_fidelity,
    test_codex_preset_structured_hook_input,
    test_codex_preset_bash_pre_tool_use_skips_checkpoint_after_capturing_snapshot,
    test_codex_preset_bash_pre_tool_use_supports_camel_case_hook_event_name,
    test_codex_preset_bash_post_tool_use_detects_changed_files,
    test_find_rollout_path_for_session_in_home,
    test_codex_commit_inside_bash_inflight_is_attributed_to_codex,
    test_codex_commit_inside_bash_inflight_repeated_append_keeps_file_ai,
    test_codex_file_edit_then_bash_pretooluse_does_not_steal_ai_commit_attribution,
    test_codex_file_edit_then_camel_case_bash_pretooluse_does_not_steal_ai_commit_attribution,
    test_codex_read_only_bash_post_tool_use_before_edit_does_not_steal_commit_attribution,
    test_codex_commit_inside_bash_inflight_repeated_append_keeps_file_ai_standard_human,
    test_codex_e2e_bash_pre_and_post_tool_use_full_cycle,
    test_codex_e2e_apply_patch_file_edit_full_cycle,
    test_codex_e2e_apply_patch_scoped_to_edited_file_only,
    test_codex_e2e_bash_then_apply_patch_in_same_session,
    test_codex_e2e_bash_modifies_multiple_files_all_attributed,
    test_codex_e2e_apply_patch_preserves_human_lines,
    test_codex_e2e_namespaced_apply_patch_file_edit_full_cycle,
    test_codex_e2e_namespaced_exec_command_bash_full_cycle,
    test_codex_e2e_multi_tool_use_parallel_wrapper,
);
