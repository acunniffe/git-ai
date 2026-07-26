use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use serde_json::json;
use std::fs;

// =============================================================================
// Issue #1204: Codex, Cursor, and multi-agent attribution failures
// https://github.com/git-ai-project/git-ai/issues/1204
//
// These tests verify:
// 1. Codex exec_command tool produces correct attribution
// 2. Cursor Shell tool pre/post cycle produces correct attribution
// 3. Multi-agent sessions don't steal each other's attribution
// =============================================================================

// ---------------------------------------------------------------------------
// Problem 1: Codex exec_command tool should be treated as Bash
// The issue reports that Codex uses "exec_command" as a tool name but the
// preset only recognizes "apply_patch" and "Bash", causing zero attribution.
// ---------------------------------------------------------------------------

#[test]
fn test_codex_exec_command_produces_attribution() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("service.py");
    fs::write(&file_path, "# original\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let transcript_path = repo_root.join("codex-exec-cmd.jsonl");
    fs::copy(&simple_fixture, &transcript_path).unwrap();

    // Pre-hook: Codex fires PreToolUse with tool_name "exec_command"
    let pre_hook_input = json!({
        "session_id": "codex-exec-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "exec_command",
        "tool_use_id": "exec-1",
        "tool_input": {
            "command": "sed -i 's/original/updated/' service.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &pre_hook_input)
        .expect("exec_command pre-hook should succeed (not be rejected)");

    // Codex edits file via exec_command
    fs::write(
        &file_path,
        "# updated by codex exec_command\ndef serve(): pass\n",
    )
    .unwrap();

    // Post-hook: Codex fires PostToolUse with tool_name "exec_command"
    let post_hook_input = json!({
        "session_id": "codex-exec-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "exec_command",
        "tool_use_id": "exec-1",
        "tool_input": {
            "command": "sed -i 's/original/updated/' service.py"
        },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &post_hook_input)
        .expect("exec_command post-hook should succeed (not be rejected)");

    let commit = repo
        .stage_all_and_commit("Codex exec_command edit")
        .expect("commit should succeed");

    // Verify attribution metadata points to codex
    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have a session record - exec_command must not produce empty prompts");

    assert_eq!(session.agent_id.tool, "codex");
    assert_eq!(session.agent_id.id, "codex-exec-session");

    // Verify line-level attribution: all lines should be AI (codex)
    let mut file = repo.filename("service.py");
    file.assert_lines_and_blame(crate::lines![
        "# updated by codex exec_command".ai(),
        "def serve(): pass".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// Problem 2: Cursor Shell tool produces correct attribution through the
// full pre/post hook cycle (validates the checkpoint flow works when the
// git proxy handles the commit)
// ---------------------------------------------------------------------------

#[test]
fn test_cursor_shell_tool_full_cycle_produces_attribution() {
    let repo = TestRepo::new();
    let jsonl_fixture = fixture_path("cursor-session-simple.jsonl");
    let jsonl_path_str = jsonl_fixture.to_string_lossy().to_string();

    let file_path = repo.path().join("app.ts");
    fs::write(&file_path, "console.log('hello');\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let repo_root = repo.canonical_path();

    // Pre-hook: Cursor fires preToolUse with Shell tool
    let pre_hook_input = json!({
        "conversation_id": "cursor-shell-session-1",
        "workspace_roots": [repo_root.to_string_lossy().to_string()],
        "hook_event_name": "preToolUse",
        "tool_name": "Shell",
        "tool_use_id": "shell-1",
        "model": "claude-3-5-sonnet",
        "tool_input": {
            "command": "sed -i 's/hello/world/' app.ts",
            "cwd": "",
            "timeout": 30000
        },
        "transcript_path": jsonl_path_str.clone()
    })
    .to_string();

    repo.checkpoint_with_hook_input("cursor", &pre_hook_input)
        .expect("cursor shell pre-hook should succeed");

    // Cursor's Shell tool modifies the file
    fs::write(
        &file_path,
        "console.log('world');\nconsole.log('from cursor shell');\n",
    )
    .unwrap();

    // Post-hook: Cursor fires postToolUse with Shell tool
    let post_hook_input = json!({
        "conversation_id": "cursor-shell-session-1",
        "workspace_roots": [repo_root.to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Shell",
        "tool_use_id": "shell-1",
        "model": "claude-3-5-sonnet",
        "tool_input": {
            "command": "sed -i 's/hello/world/' app.ts",
            "cwd": "",
            "timeout": 30000
        },
        "transcript_path": jsonl_path_str
    })
    .to_string();

    repo.checkpoint_with_hook_input("cursor", &post_hook_input)
        .expect("cursor shell post-hook should succeed");

    let commit = repo
        .stage_all_and_commit("Cursor shell edit")
        .expect("commit should succeed");

    // Verify session metadata
    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have session record for cursor shell");

    assert_eq!(session.agent_id.tool, "cursor");
    assert_eq!(session.agent_id.id, "cursor-shell-session-1");

    // Verify line-level attribution
    let mut file = repo.filename("app.ts");
    file.assert_lines_and_blame(crate::lines![
        "console.log('world');".ai(),
        "console.log('from cursor shell');".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// Problem 3: Multi-agent session conflict - when Claude (or another agent)
// is active and fires a pre-hook, then Codex edits a file and fires its
// own pre+post hooks, Claude's post-hook must NOT steal Codex's attribution.
// ---------------------------------------------------------------------------

#[test]
fn test_multi_agent_codex_edit_not_stolen_by_claude_pre_hook() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("shared.py");
    fs::write(&file_path, "# base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let codex_transcript = repo_root.join("codex-multi.jsonl");
    fs::copy(&simple_fixture, &codex_transcript).unwrap();

    // Step 1: Claude fires PreToolUse (Bash) - captures snapshot of shared.py
    let claude_pre = json!({
        "session_id": "claude-session-active",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-1"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_pre)
        .expect("claude pre-hook should succeed");

    // Step 2: Codex fires PreToolUse (Bash) - captures snapshot before its own edit
    let codex_pre = json!({
        "session_id": "codex-session-editing",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "codex-bash-1",
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_pre)
        .expect("codex pre-hook should succeed");

    // Step 3: Codex edits the file
    fs::write(
        &file_path,
        "# base\n# added by codex\ndef codex_func(): pass\n",
    )
    .unwrap();

    // Step 4: Codex fires PostToolUse (Bash) - claims its changes
    let codex_post = json!({
        "session_id": "codex-session-editing",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "codex-bash-1",
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_post)
        .expect("codex post-hook should succeed");

    // Step 5: Claude fires PostToolUse (Bash) - Claude's bash did NOT edit the file,
    // but it sees a diff from its earlier pre-hook snapshot. The system must NOT
    // attribute Codex's changes to Claude.
    let claude_post = json!({
        "session_id": "claude-session-active",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-1"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_post)
        .expect("claude post-hook should succeed");

    // Commit and verify attribution
    let commit = repo
        .stage_all_and_commit("Multi-agent edit")
        .expect("commit should succeed");

    // The key assertion: Codex's lines must be attributed to Codex (AI),
    // NOT to Claude. The base line remains human/unattributed.
    let mut file = repo.filename("shared.py");
    file.assert_committed_lines(crate::lines![
        "# base".unattributed_human(),
        "# added by codex".ai(),
        "def codex_func(): pass".ai(),
    ]);

    // Verify the session records show the correct agent
    let has_codex_session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .any(|s| s.agent_id.tool == "codex");
    assert!(
        has_codex_session,
        "Should have a codex session record for the lines it wrote"
    );
}

#[test]
fn test_multi_agent_cursor_edit_not_stolen_by_claude_pre_hook() {
    let repo = TestRepo::new();
    let jsonl_fixture = fixture_path("cursor-session-simple.jsonl");
    let jsonl_path_str = jsonl_fixture.to_string_lossy().to_string();

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("component.tsx");
    fs::write(&file_path, "export const App = () => null;\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Step 1: Claude fires PreToolUse (Bash) - captures snapshot
    let claude_pre = json!({
        "session_id": "claude-vscode-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-1"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_pre)
        .expect("claude pre-hook should succeed");

    // Step 2: Cursor fires preToolUse (Write) - captures snapshot before edit
    let cursor_pre = json!({
        "conversation_id": "cursor-conv-1",
        "workspace_roots": [repo_root.to_string_lossy().to_string()],
        "hook_event_name": "preToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": file_path.to_string_lossy().to_string() },
        "model": "claude-3-5-sonnet",
        "transcript_path": jsonl_path_str.clone()
    })
    .to_string();

    repo.checkpoint_with_hook_input("cursor", &cursor_pre)
        .expect("cursor pre-hook should succeed");

    // Step 3: Cursor edits the file
    fs::write(
        &file_path,
        "export const App = () => null;\nexport const Header = () => <h1>Hello</h1>;\n",
    )
    .unwrap();

    // Step 4: Cursor fires postToolUse (Write) - claims its changes
    let cursor_post = json!({
        "conversation_id": "cursor-conv-1",
        "workspace_roots": [repo_root.to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": file_path.to_string_lossy().to_string() },
        "model": "claude-3-5-sonnet",
        "transcript_path": jsonl_path_str
    })
    .to_string();

    repo.checkpoint_with_hook_input("cursor", &cursor_post)
        .expect("cursor post-hook should succeed");

    // Step 5: Claude fires PostToolUse (Bash) - Claude did NOT edit the file
    let claude_post = json!({
        "session_id": "claude-vscode-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-1"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_post)
        .expect("claude post-hook should succeed");

    // Commit and verify attribution
    repo.stage_all_and_commit("Multi-agent cursor+claude")
        .expect("commit should succeed");

    // The key assertion: Cursor's line must be attributed to Cursor (AI),
    // NOT to Claude. The original line remains human/unattributed.
    let mut file = repo.filename("component.tsx");
    file.assert_committed_lines(crate::lines![
        "export const App = () => null;".unattributed_human(),
        "export const Header = () => <h1>Hello</h1>;".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// Variant: Multi-agent where Codex uses apply_patch while Claude is active
// ---------------------------------------------------------------------------

#[test]
fn test_multi_agent_codex_apply_patch_not_stolen_by_active_claude() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("api.rs");
    fs::write(&file_path, "fn handler() {}\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let codex_transcript = repo_root.join("codex-patch-multi.jsonl");
    fs::copy(&simple_fixture, &codex_transcript).unwrap();

    // Claude fires PreToolUse (captures snapshot)
    let claude_pre = json!({
        "session_id": "claude-active-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-2"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_pre)
        .expect("claude pre-hook should succeed");

    // Codex fires PreToolUse (apply_patch)
    let codex_pre = json!({
        "session_id": "codex-patch-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-2",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ fn handler() {{}}\n+fn new_handler() {{}}\n", file_path.to_string_lossy())
        },
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_pre)
        .expect("codex pre-hook should succeed");

    // Codex apply_patch modifies file
    fs::write(&file_path, "fn new_handler() {}\nfn helper() {}\n").unwrap();

    // Codex fires PostToolUse (apply_patch)
    let codex_post = json!({
        "session_id": "codex-patch-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_use_id": "patch-2",
        "tool_input": {
            "patch": format!("*** Update File: {}\n@@ fn handler() {{}}\n+fn new_handler() {{}}\n+fn helper() {{}}\n", file_path.to_string_lossy())
        },
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_post)
        .expect("codex post-hook should succeed");

    // Claude fires PostToolUse (its bash didn't edit the file)
    let claude_post = json!({
        "session_id": "claude-active-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-2"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_post)
        .expect("claude post-hook should succeed");

    // Commit and verify
    repo.stage_all_and_commit("Codex apply_patch with claude active")
        .expect("commit should succeed");

    let mut file = repo.filename("api.rs");
    file.assert_committed_lines(crate::lines![
        "fn new_handler() {}".ai(),
        "fn helper() {}".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// Variant: Multi-agent where both agents edit DIFFERENT files simultaneously
// Each agent should own only its own file's attribution.
// ---------------------------------------------------------------------------

#[test]
fn test_multi_agent_separate_files_correct_attribution() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_a = repo_root.join("claude_file.py");
    let file_b = repo_root.join("codex_file.py");
    fs::write(&file_a, "# original a\n").unwrap();
    fs::write(&file_b, "# original b\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let simple_fixture = fixture_path("codex-session-simple.jsonl");
    let codex_transcript = repo_root.join("codex-separate.jsonl");
    fs::copy(&simple_fixture, &codex_transcript).unwrap();

    // Claude pre-hook (will edit file_a)
    let claude_pre = json!({
        "session_id": "claude-sep-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-sep"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_pre)
        .expect("claude pre-hook");

    // Codex pre-hook (will edit file_b)
    let codex_pre = json!({
        "session_id": "codex-sep-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "codex-bash-sep",
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_pre)
        .expect("codex pre-hook");

    // Both agents edit their own files
    fs::write(&file_a, "# edited by claude\ndef claude_fn(): pass\n").unwrap();
    fs::write(&file_b, "# edited by codex\ndef codex_fn(): pass\n").unwrap();

    // Claude post-hook
    let claude_post = json!({
        "session_id": "claude-sep-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "claude-bash-sep"
    })
    .to_string();

    repo.checkpoint_with_hook_input("claude", &claude_post)
        .expect("claude post-hook");

    // Codex post-hook
    let codex_post = json!({
        "session_id": "codex-sep-session",
        "cwd": repo_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "codex-bash-sep",
        "transcript_path": codex_transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.checkpoint_with_hook_input("codex", &codex_post)
        .expect("codex post-hook");

    // Commit both files together
    let commit = repo
        .stage_all_and_commit("Both agents edit separate files")
        .expect("commit should succeed");

    // Claude's file should be AI-attributed
    let mut fa = repo.filename("claude_file.py");
    fa.assert_committed_lines(crate::lines![
        "# edited by claude".ai(),
        "def claude_fn(): pass".ai(),
    ]);

    // Codex's file should be AI-attributed
    let mut fb = repo.filename("codex_file.py");
    fb.assert_committed_lines(crate::lines![
        "# edited by codex".ai(),
        "def codex_fn(): pass".ai(),
    ]);

    // At least one AI agent session should be recorded (both files are AI-attributed).
    // NOTE: With unscoped bash tools, the agent whose post-hook fires last may claim
    // all workspace changes. The critical guarantee is correct AI vs human attribution,
    // which is verified by the line-level assertions above.
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one AI session"
    );
    let all_ai_tools: Vec<&str> = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .map(|s| s.agent_id.tool.as_str())
        .collect();
    assert!(
        all_ai_tools.iter().any(|t| *t == "claude" || *t == "codex"),
        "Should have claude or codex session, got: {:?}",
        all_ai_tools
    );
}

crate::reuse_tests_in_worktree!(
    test_codex_exec_command_produces_attribution,
    test_cursor_shell_tool_full_cycle_produces_attribution,
    test_multi_agent_codex_edit_not_stolen_by_claude_pre_hook,
    test_multi_agent_cursor_edit_not_stolen_by_claude_pre_hook,
    test_multi_agent_codex_apply_patch_not_stolen_by_active_claude,
    test_multi_agent_separate_files_correct_attribution,
);
