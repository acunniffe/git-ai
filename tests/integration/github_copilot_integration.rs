use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;
use std::fs;

/// Test human checkpoint via github-copilot preset with before_edit hook
#[test]
fn test_github_copilot_human_checkpoint_before_edit() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.ts");

    // Create initial file
    file.set_contents(crate::lines!["const x = 1;", "const y = 2;"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Simulate human making changes before AI edit
    file.insert_at(2, crate::lines!["const z = 3;", "const a = 4;"]);

    // Create human checkpoint using github-copilot preset with before_edit
    let file_path = repo.path().join("test.ts");
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [file_path.to_str().unwrap()],
        "dirtyFiles": {
            file_path.to_str().unwrap(): file.contents()
        }
    });

    repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &hook_input.to_string(),
    ])
    .unwrap();

    // Now commit the changes
    repo.stage_all_and_commit("Add constants").unwrap();

    // All lines should be human since we only did a human checkpoint
    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".human(),
        "const z = 3;".human(),
        "const a = 4;".human(),
    ]);
}

/// Test that human checkpoint with will_edit_filepaths scopes the attribution correctly
#[test]
fn test_github_copilot_human_checkpoint_scoped_to_files() {
    let repo = TestRepo::new();
    let mut file1 = repo.filename("file1.ts");
    let mut file2 = repo.filename("file2.ts");

    // Create initial files
    file1.set_contents(crate::lines!["// File 1"]);
    file2.set_contents(crate::lines!["// File 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Make changes to both files
    file1.insert_at(1, crate::lines!["const x = 1;"]);
    file2.insert_at(1, crate::lines!["const y = 2;"]);

    // Create human checkpoint only for file1
    let file1_path = repo.path().join("file1.ts");
    let file2_path = repo.path().join("file2.ts");
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [file1_path.to_str().unwrap()],
        "dirtyFiles": {
            file1_path.to_str().unwrap(): file1.contents(),
            file2_path.to_str().unwrap(): file2.contents()
        }
    });

    repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &hook_input.to_string(),
    ])
    .unwrap();

    // Commit the changes
    repo.stage_all_and_commit("Add constants").unwrap();

    // file1 should be human (was in will_edit_filepaths)
    file1.assert_lines_and_blame(crate::lines!["// File 1".human(), "const x = 1;".human(),]);

    // file2 should also be human (no AI checkpoint was made)
    file2.assert_lines_and_blame(crate::lines!["// File 2".human(), "const y = 2;".human(),]);
}

/// Test human checkpoint followed by AI checkpoint
#[test]
fn test_github_copilot_human_then_ai_checkpoint() {
    let repo = TestRepo::new();

    // Create initial file
    let file_path = repo.path().join("test.ts");
    std::fs::write(&file_path, "const x = 1;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.ts"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = crate::repos::test_file::TestFile::from_existing_file(file_path.clone(), &repo);

    // Human makes a change
    file.insert_at(1, crate::lines!["const y = 2;"]);

    // Human checkpoint
    let human_hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [file_path.to_str().unwrap()],
        "dirtyFiles": {
            file_path.to_str().unwrap(): file.contents()
        }
    });

    repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &human_hook_input.to_string(),
    ])
    .unwrap();

    // AI makes a change
    file.insert_at(2, crate::lines!["const z = 3;".ai()]);

    // Mock AI checkpoint (simulating after_edit, but we'll use mock_ai for simplicity)
    repo.git_ai(&["checkpoint", "mock_ai", file_path.to_str().unwrap()])
        .unwrap();

    // Commit the changes
    repo.stage_all_and_commit("Add constants").unwrap();

    // Verify attribution
    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".ai(), // Reattributed to AI after subsequent AI checkpoint
        "const z = 3;".ai(), // AI from mock_ai checkpoint
    ]);
}

/// Test multiple files with human checkpoint and dirty files
#[test]
fn test_github_copilot_multiple_files_with_dirty_files() {
    let repo = TestRepo::new();
    let mut file1 = repo.filename("file1.ts");
    let mut file2 = repo.filename("file2.ts");
    let mut file3 = repo.filename("file3.ts");

    // Create initial files
    file1.set_contents(crate::lines!["// File 1"]);
    file2.set_contents(crate::lines!["// File 2"]);
    file3.set_contents(crate::lines!["// File 3"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Make changes to all files
    file1.insert_at(1, crate::lines!["const a = 1;"]);
    file2.insert_at(1, crate::lines!["const b = 2;"]);
    file3.insert_at(1, crate::lines!["const c = 3;"]);

    // Create human checkpoint for file1 and file2, but not file3
    let file1_path = repo.path().join("file1.ts");
    let file2_path = repo.path().join("file2.ts");
    let file3_path = repo.path().join("file3.ts");
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [
            file1_path.to_str().unwrap(),
            file2_path.to_str().unwrap()
        ],
        "dirtyFiles": {
            file1_path.to_str().unwrap(): file1.contents(),
            file2_path.to_str().unwrap(): file2.contents(),
            file3_path.to_str().unwrap(): file3.contents()
        }
    });

    repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &hook_input.to_string(),
    ])
    .unwrap();

    // Commit the changes
    repo.stage_all_and_commit("Add constants").unwrap();

    // file1 and file2 should be human (in will_edit_filepaths)
    file1.assert_lines_and_blame(crate::lines!["// File 1".human(), "const a = 1;".human(),]);
    file2.assert_lines_and_blame(crate::lines!["// File 2".human(), "const b = 2;".human(),]);

    // file3 should be human too (no AI checkpoint was made)
    file3.assert_lines_and_blame(crate::lines!["// File 3".human(), "const c = 3;".human(),]);
}

/// Test that empty will_edit_filepaths fails validation
#[test]
fn test_github_copilot_empty_will_edit_filepaths_fails() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.ts");

    file.set_contents(crate::lines!["const x = 1;"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [],
        "dirtyFiles": {}
    });

    let result = repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &hook_input.to_string(),
    ]);

    // Should exit 0 (to avoid interrupting agent hooks) but print error message
    let output = result.unwrap();
    assert!(
        output.contains("will_edit_filepaths cannot be empty"),
        "Expected error message about empty will_edit_filepaths, got: {}",
        output
    );
}

/// Test human checkpoint preserves file contents even when file isn't dirty
#[test]
fn test_github_copilot_human_checkpoint_with_clean_file() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.ts");

    // Create initial file
    file.set_contents(crate::lines!["const x = 1;"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Human checkpoint with the file in will_edit_filepaths but not modified yet
    let file_path = repo.path().join("test.ts");
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "workspaceFolder": repo.path().to_str().unwrap(),
        "will_edit_filepaths": [file_path.to_str().unwrap()],
        "dirtyFiles": {
            file_path.to_str().unwrap(): file.contents()
        }
    });

    repo.git_ai(&[
        "checkpoint",
        "github-copilot",
        "--hook-input",
        &hook_input.to_string(),
    ])
    .unwrap();

    // Now make a change
    file.insert_at(1, crate::lines!["const y = 2;"]);

    // Commit the change
    repo.stage_all_and_commit("Add y").unwrap();

    // The new line should be human
    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".human(),
    ]);
}

fn setup_vscode_copilot_session(
    session_id: &str,
    request_model: &str,
    resolved_model: Option<&str>,
) -> (tempfile::TempDir, String, std::path::PathBuf) {
    let storage = tempfile::tempdir().unwrap();
    let workspace_storage = storage.path().join("workspaceStorage/workspace-id");
    let transcript_path = workspace_storage
        .join("GitHub.copilot-chat/transcripts")
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
    fs::write(
        &transcript_path,
        format!(r#"{{"type":"session.start","data":{{"sessionId":"{session_id}"}}}}\n"#),
    )
    .unwrap();

    let chat_session_path = workspace_storage
        .join("chatSessions")
        .join(format!("{session_id}.jsonl"));
    fs::create_dir_all(chat_session_path.parent().unwrap()).unwrap();
    write_vscode_chat_session(
        &chat_session_path,
        session_id,
        request_model,
        resolved_model,
    );

    let models_path = workspace_storage
        .join("GitHub.copilot-chat/debug-logs")
        .join(session_id)
        .join("models.json");
    fs::create_dir_all(models_path.parent().unwrap()).unwrap();
    fs::write(
        models_path,
        r#"[{"id":"gpt-5.3-codex","is_chat_default":true}]"#,
    )
    .unwrap();

    (
        storage,
        transcript_path.to_string_lossy().into_owned(),
        chat_session_path,
    )
}

fn write_vscode_chat_session(
    chat_session_path: &std::path::Path,
    session_id: &str,
    request_model: &str,
    resolved_model: Option<&str>,
) {
    let snapshot = json!({
        "kind": 0,
        "v": {
            "sessionId": session_id,
            "inputState": { "selectedModel": { "identifier": request_model } },
            "requests": []
        }
    });
    let request_patch = json!({
        "kind": 2,
        "k": ["requests"],
        "v": [{
            "requestId": "request-1",
            "modelId": request_model,
            "result": {
                "metadata": {
                    "sessionId": session_id,
                    "resolvedModel": resolved_model
                }
            }
        }]
    });
    fs::write(
        chat_session_path,
        format!("{}\n{}\n", snapshot, request_patch),
    )
    .unwrap();
}

fn native_edit_hook_input(
    event_name: &str,
    repo: &TestRepo,
    file_path: &std::path::Path,
    transcript_path: &str,
    session_id: &str,
    tool_use_id: &str,
) -> String {
    json!({
        "hookEventName": event_name,
        "cwd": repo.path(),
        "toolName": "copilot_replaceString",
        "toolUseId": tool_use_id,
        "toolInput": { "file_path": file_path },
        "sessionId": session_id,
        "transcript_path": transcript_path
    })
    .to_string()
}

fn create_committed_human_file(repo: &TestRepo, filename: &str) -> std::path::PathBuf {
    let file_path = repo.path().join(filename);
    fs::write(&file_path, "const human = true;\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", filename])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.filename(filename)
        .assert_committed_lines(crate::lines!["const human = true;".human()]);
    file_path
}

fn checkpoint_native_copilot_edit(
    repo: &TestRepo,
    file_path: &std::path::Path,
    contents: &str,
    transcript_path: &str,
    session_id: &str,
    tool_use_id: &str,
) {
    for event_name in ["PreToolUse", "PostToolUse"] {
        if event_name == "PostToolUse" {
            fs::write(file_path, contents).unwrap();
        }
        let hook = native_edit_hook_input(
            event_name,
            repo,
            file_path,
            transcript_path,
            session_id,
            tool_use_id,
        );
        repo.git_ai(&["checkpoint", "github-copilot", "--hook-input", &hook])
            .unwrap();
    }
}

fn copilot_model(commit: &crate::repos::test_repo::NewCommit) -> &str {
    commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .find(|session| session.agent_id.tool == "github-copilot")
        .expect("Copilot session should be present")
        .agent_id
        .model
        .as_str()
}

#[test]
fn test_vscode_copilot_uses_selected_model_in_authorship() {
    let repo = TestRepo::new();
    let file_path = create_committed_human_file(&repo, "model.ts");
    let mut file = repo.filename("model.ts");
    let session_id = "selected-model-session";
    let (_storage, transcript_path, _chat_session_path) =
        setup_vscode_copilot_session(session_id, "copilot/claude-sonnet-5", None);
    checkpoint_native_copilot_edit(
        &repo,
        &file_path,
        "const human = true;\nconst generatedByClaude = true;\n",
        &transcript_path,
        session_id,
        "tool-1",
    );

    let commit = repo.stage_all_and_commit("Add generated line").unwrap();
    file.assert_committed_lines(crate::lines![
        "const human = true;".human(),
        "const generatedByClaude = true;".ai(),
    ]);
    assert_eq!(copilot_model(&commit), "copilot/claude-sonnet-5");
}

#[test]
fn test_vscode_copilot_auto_uses_resolved_model() {
    let repo = TestRepo::new();
    let file_path = create_committed_human_file(&repo, "auto.ts");
    let mut file = repo.filename("auto.ts");
    let session_id = "auto-model-session";
    let (_storage, transcript_path, _chat_session_path) =
        setup_vscode_copilot_session(session_id, "copilot/auto", Some("claude-sonnet-5"));
    checkpoint_native_copilot_edit(
        &repo,
        &file_path,
        "const human = true;\nconst generatedByAuto = true;\n",
        &transcript_path,
        session_id,
        "tool-auto",
    );

    let commit = repo
        .stage_all_and_commit("Add Auto-generated line")
        .unwrap();
    file.assert_committed_lines(crate::lines![
        "const human = true;".human(),
        "const generatedByAuto = true;".ai(),
    ]);
    assert_eq!(copilot_model(&commit), "claude-sonnet-5");
}

#[test]
fn test_vscode_copilot_caches_first_concrete_session_model() {
    let repo = TestRepo::new();
    let file_path = create_committed_human_file(&repo, "cached.ts");
    let mut file = repo.filename("cached.ts");
    let session_id = "cached-model-session";
    let (_storage, transcript_path, chat_session_path) =
        setup_vscode_copilot_session(session_id, "copilot/claude-sonnet-5", None);
    checkpoint_native_copilot_edit(
        &repo,
        &file_path,
        "const human = true;\nconst firstGeneratedLine = true;\n",
        &transcript_path,
        session_id,
        "tool-cache-1",
    );
    repo.sync_daemon_force();

    write_vscode_chat_session(&chat_session_path, session_id, "copilot/gpt-5.4", None);
    checkpoint_native_copilot_edit(
        &repo,
        &file_path,
        "const human = true;\nconst firstGeneratedLine = true;\nconst secondGeneratedLine = true;\n",
        &transcript_path,
        session_id,
        "tool-cache-2",
    );

    let commit = repo.stage_all_and_commit("Add cached-model lines").unwrap();
    file.assert_committed_lines(crate::lines![
        "const human = true;".human(),
        "const firstGeneratedLine = true;".ai(),
        "const secondGeneratedLine = true;".ai(),
    ]);
    assert_eq!(copilot_model(&commit), "copilot/claude-sonnet-5");
}

crate::reuse_tests_in_worktree!(
    test_github_copilot_human_checkpoint_before_edit,
    test_github_copilot_human_checkpoint_scoped_to_files,
    test_github_copilot_human_then_ai_checkpoint,
    test_github_copilot_multiple_files_with_dirty_files,
    test_github_copilot_empty_will_edit_filepaths_fails,
    test_github_copilot_human_checkpoint_with_clean_file,
    test_vscode_copilot_uses_selected_model_in_authorship,
    test_vscode_copilot_auto_uses_resolved_model,
    test_vscode_copilot_caches_first_concrete_session_model,
);
