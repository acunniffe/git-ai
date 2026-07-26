use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;

// Helper to create a realistic Copilot transcript path matching actual VS Code format
fn fake_copilot_transcript_path(repo: &TestRepo) -> String {
    repo.path()
        .join("Library/Application Support/Code/User/workspaceStorage/abc123def456/GitHub.copilot-chat/transcripts/session-test-uuid.jsonl")
        .to_str()
        .unwrap()
        .to_string()
}

/// Test create_file PreToolUse correctly synthesizes empty dirty_files
/// This prevents the Pre checkpoint from reading stale disk content from concurrent tool calls
#[test]
fn test_create_file_pre_tool_use_empty_dirty_files() {
    let repo = TestRepo::new();

    // Create initial file for first commit
    let mut initial_file = repo.filename("README.md");
    initial_file.set_contents(crate::lines!["# Test repo"]);

    // Initial commit
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create the file first (simulating VS Code creating it)
    let mut file = repo.filename("new_file.py");
    file.set_contents(crate::lines!["print(\"hello world\")"]);

    // Simulate create_file PreToolUse hook (based on real captured data)
    let file_path = repo.path().join("new_file.py");
    let pre_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.881Z",
        "hook_event_name": "PreToolUse",
        "session_id": "b4a517c6-b9f0-4787-af3a-7c002539b448",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": file_path.to_str().unwrap(),
            "content": "print(\"hello world\")\n"
        },
        "tool_use_id": "call_Lxfk50uMklX0tVEVgxn4DVQD__vscode-1775710747664",
        "cwd": repo.path().to_str().unwrap()
    });

    // Run PreToolUse checkpoint
    repo.checkpoint_with_hook_input("github-copilot", &pre_hook_input.to_string())
        .unwrap();

    // Simulate create_file PostToolUse hook
    let post_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.970Z",
        "hook_event_name": "PostToolUse",
        "session_id": "b4a517c6-b9f0-4787-af3a-7c002539b448",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": file_path.to_str().unwrap(),
            "content": "print(\"hello world\")\n"
        },
        "tool_response": "",
        "tool_use_id": "call_Lxfk50uMklX0tVEVgxn4DVQD__vscode-1775710747664",
        "cwd": repo.path().to_str().unwrap()
    });

    repo.checkpoint_with_hook_input("github-copilot", &post_hook_input.to_string())
        .unwrap();

    // Sync daemon to ensure checkpoint is processed before commit
    repo.sync_daemon();

    // Commit
    repo.stage_all_and_commit("Create new file").unwrap();

    // File should be attributed to AI (not human Pre checkpoint)
    file.assert_lines_and_blame(crate::lines!["print(\"hello world\")".ai()]);
}

/// Test rapid multi-file creation with concurrent hooks
/// This is the key regression test - ensures files don't cross-contaminate
#[test]
fn test_create_file_rapid_multi_file_no_contamination() {
    let repo = TestRepo::new();

    // Create initial file for first commit
    let mut initial_file = repo.filename("README.md");
    initial_file.set_contents(crate::lines!["# Test repo"]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let session_id = "c2e8d5f3-4a7b-4c9d-8e1f-2b3a4c5d6e7f";
    let transcript_path = fake_copilot_transcript_path(&repo);

    // Simulate 3 rapid create_file calls (like Copilot creating multiple files)
    let files = ["file1.py", "file2.py", "file3.py"];
    let contents = [
        "print(\"file 1\")\n",
        "print(\"file 2\")\n",
        "print(\"file 3\")\n",
    ];
    let tool_use_ids = [
        "call_AYUcJD0GxIIwGCaq2ayVkSxo__vscode-1775710747674",
        "call_p1lQdR2Tn7vSmIYBzLY5n1gg__vscode-1775710747675",
        "call_UMZRrEf033UfgZS7RpSWXDhr__vscode-1775710747676",
    ];

    for (i, ((filename, content), tool_use_id)) in files
        .iter()
        .zip(contents.iter())
        .zip(tool_use_ids.iter())
        .enumerate()
    {
        let file_path = repo.path().join(filename);

        // Create the actual file first
        let mut test_file = repo.filename(filename);
        test_file.set_contents(crate::lines![content.trim()]);

        // PreToolUse - should only include THIS file, not previous files from session
        let pre_hook = json!({
            "timestamp": format!("2026-04-09T17:36:0{}.100Z", i),
            "hook_event_name": "PreToolUse",
            "session_id": session_id,
            "transcript_path": &transcript_path,
            "tool_name": "create_file",
            "tool_input": {
                "filePath": file_path.to_str().unwrap(),
                "content": content
            },
            "tool_use_id": tool_use_id,
            "cwd": repo.path().to_str().unwrap()
        });

        repo.checkpoint_with_hook_input("github-copilot", &pre_hook.to_string())
            .unwrap();

        // PostToolUse
        let post_hook = json!({
            "timestamp": format!("2026-04-09T17:36:0{}.200Z", i),
            "hook_event_name": "PostToolUse",
            "session_id": session_id,
            "transcript_path": &transcript_path,
            "tool_name": "create_file",
            "tool_input": {
                "filePath": file_path.to_str().unwrap(),
                "content": content
            },
            "tool_response": "",
            "tool_use_id": tool_use_id,
            "cwd": repo.path().to_str().unwrap()
        });

        repo.checkpoint_with_hook_input("github-copilot", &post_hook.to_string())
            .unwrap();
    }

    // Sync daemon to ensure all checkpoints are processed before commit
    repo.sync_daemon();

    // Commit all files
    repo.stage_all_and_commit("Create multiple files").unwrap();

    // All files should be attributed to AI, none should be human from Pre checkpoints
    let mut file1 = repo.filename("file1.py");
    file1.assert_lines_and_blame(crate::lines!["print(\"file 1\")".ai()]);

    let mut file2 = repo.filename("file2.py");
    file2.assert_lines_and_blame(crate::lines!["print(\"file 2\")".ai()]);

    let mut file3 = repo.filename("file3.py");
    file3.assert_lines_and_blame(crate::lines!["print(\"file 3\")".ai()]);
}

/// Test that create_file doesn't pull in session-level detected_edited_filepaths
/// from transcript parsing (regression test for old behavior)
#[test]
fn test_create_file_ignores_transcript_session_files() {
    let repo = TestRepo::new();

    // Create an existing file that was edited in a previous tool call
    let mut existing_file = repo.filename("existing.py");
    existing_file.set_contents(crate::lines!["print(\"existing\")"]);
    repo.stage_all_and_commit("Add existing file").unwrap();

    // Now create a NEW file
    let new_file_path = repo.path().join("new.py");

    // The hook payload should ONLY reference new.py in tool_input
    // (transcript might have existing.py in session history, but we ignore that now)
    let pre_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.881Z",
        "hook_event_name": "PreToolUse",
        "session_id": "d3f4e5a6-7b8c-9d0e-1f2a-3b4c5d6e7f8a",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": new_file_path.to_str().unwrap(),
            "content": "print(\"new file\")\n"
        },
        "tool_use_id": "call_7b5JYKtxLvgUBhLDfJcKFsfk__vscode-1775710747711",
        "cwd": repo.path().to_str().unwrap()
    });

    // Create the new file
    let mut new_file = repo.filename("new.py");
    new_file.set_contents(crate::lines!["print(\"new file\")"]);

    repo.checkpoint_with_hook_input("github-copilot", &pre_hook_input.to_string())
        .unwrap();

    let post_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.970Z",
        "hook_event_name": "PostToolUse",
        "session_id": "d3f4e5a6-7b8c-9d0e-1f2a-3b4c5d6e7f8a",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": new_file_path.to_str().unwrap(),
            "content": "print(\"new file\")\n"
        },
        "tool_response": "",
        "tool_use_id": "call_7b5JYKtxLvgUBhLDfJcKFsfk__vscode-1775710747711",
        "cwd": repo.path().to_str().unwrap()
    });

    repo.checkpoint_with_hook_input("github-copilot", &post_hook_input.to_string())
        .unwrap();

    // Sync daemon to ensure checkpoint is processed before commit
    repo.sync_daemon();

    repo.stage_all_and_commit("Create new file").unwrap();

    // Only new.py should be attributed to AI
    new_file.assert_lines_and_blame(crate::lines!["print(\"new file\")".ai()]);

    // existing.py should still be human (not touched by this checkpoint)
    existing_file.assert_lines_and_blame(crate::lines!["print(\"existing\")".human()]);
}

/// Test create_file with content from payload (not disk)
/// Ensures we use tool_input.content to bypass disk timing issues
#[test]
fn test_create_file_uses_payload_content_not_disk() {
    let repo = TestRepo::new();

    // Create initial file for first commit
    let mut initial_file = repo.filename("README.md");
    initial_file.set_contents(crate::lines!["# Test repo"]);

    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("test.py");
    let expected_content = "print(\"from payload\")\n";

    // Create the file on disk first (even though we want to test payload content,
    // the file must exist for the checkpoint to process it)
    let mut file = repo.filename("test.py");
    file.set_contents(crate::lines!["print(\"from payload\")"]);

    // PreToolUse hook
    let pre_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.881Z",
        "hook_event_name": "PreToolUse",
        "session_id": "e5f6a7b8-9c0d-1e2f-3a4b-5c6d7e8f9a0b",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": file_path.to_str().unwrap(),
            "content": expected_content
        },
        "tool_use_id": "call_rVvpsnooeumKXuCmuzVtuPmM__vscode-1775710747712",
        "cwd": repo.path().to_str().unwrap()
    });

    repo.checkpoint_with_hook_input("github-copilot", &pre_hook_input.to_string())
        .unwrap();

    // PostToolUse hook - checkpoint uses content from payload
    let post_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.970Z",
        "hook_event_name": "PostToolUse",
        "session_id": "e5f6a7b8-9c0d-1e2f-3a4b-5c6d7e8f9a0b",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": file_path.to_str().unwrap(),
            "content": expected_content
        },
        "tool_response": "",
        "tool_use_id": "call_rVvpsnooeumKXuCmuzVtuPmM__vscode-1775710747712",
        "cwd": repo.path().to_str().unwrap()
    });

    repo.checkpoint_with_hook_input("github-copilot", &post_hook_input.to_string())
        .unwrap();

    // Sync daemon to ensure checkpoint is processed before commit
    repo.sync_daemon();

    repo.stage_all_and_commit("Create file").unwrap();

    // Should be attributed correctly despite disk timing
    file.assert_lines_and_blame(crate::lines!["print(\"from payload\")".ai()]);
}

/// Test that hook_data.edited_filepaths/will_edit_filepaths are ignored
/// in favor of tool_input file paths (regression test)
#[test]
fn test_create_file_ignores_top_level_edited_filepaths() {
    let repo = TestRepo::new();

    // Create a file that might be in edited_filepaths from previous session state
    let mut old_file = repo.filename("old.py");
    old_file.set_contents(crate::lines!["print(\"old\")"]);
    repo.stage_all_and_commit("Add old file").unwrap();

    let new_file_path = repo.path().join("new.py");
    let old_file_path = repo.path().join("old.py");

    let mut new_file = repo.filename("new.py");
    new_file.set_contents(crate::lines!["print(\"new\")"]);

    // PreToolUse hook
    let pre_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.881Z",
        "hook_event_name": "PreToolUse",
        "session_id": "e5f6a7b8-9c0d-1e2f-3a4b-5c6d7e8f9a0b",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": new_file_path.to_str().unwrap(),
            "content": "print(\"new\")\n"
        },
        "tool_use_id": "call_rVvpsnooeumKXuCmuzVtuPmM__vscode-1775710747712",
        "cwd": repo.path().to_str().unwrap()
    });

    repo.checkpoint_with_hook_input("github-copilot", &pre_hook_input.to_string())
        .unwrap();

    // PostToolUse hook with stale edited_filepaths at top level (shouldn't be used)
    let post_hook_input = json!({
        "timestamp": "2026-04-09T17:36:05.970Z",
        "hook_event_name": "PostToolUse",
        "session_id": "e5f6a7b8-9c0d-1e2f-3a4b-5c6d7e8f9a0b",
        "transcript_path": fake_copilot_transcript_path(&repo),
        "tool_name": "create_file",
        "tool_input": {
            "filePath": new_file_path.to_str().unwrap(),
            "content": "print(\"new\")\n"
        },
        "tool_response": "",
        "tool_use_id": "call_rVvpsnooeumKXuCmuzVtuPmM__vscode-1775710747712",
        "cwd": repo.path().to_str().unwrap(),
        // These top-level fields should be IGNORED (old behavior would merge them)
        "edited_filepaths": [old_file_path.to_str().unwrap()],
    });

    repo.checkpoint_with_hook_input("github-copilot", &post_hook_input.to_string())
        .unwrap();

    // Sync daemon to ensure checkpoint is processed before commit
    repo.sync_daemon();

    repo.stage_all_and_commit("Create new file").unwrap();

    // Only new.py should be attributed (old.py should not be affected)
    new_file.assert_lines_and_blame(crate::lines!["print(\"new\")".ai()]);
    old_file.assert_lines_and_blame(crate::lines!["print(\"old\")".human()]);
}
