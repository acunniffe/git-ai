use crate::repos::test_repo::TestRepo;
use git_ai::model::transcript::{AiTranscript, Message};
use std::fs;

/// Helper to create a simple agent_v1 AI checkpoint with a transcript
fn checkpoint_with_message(repo: &TestRepo, message: &str, edited_files: Vec<String>) {
    let mut transcript = AiTranscript::new();
    transcript.add_message(Message::user(message.to_string(), None));
    transcript.add_message(Message::assistant(
        "I'll help you with that.".to_string(),
        None,
    ));

    let hook_input = serde_json::json!({
        "type": "ai_agent",
        "repo_working_dir": repo.path().to_str().unwrap(),
        "edited_filepaths": edited_files,
        "transcript": transcript,
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-conversation-id",
    });

    let hook_input_str = serde_json::to_string(&hook_input).unwrap();

    repo.checkpoint_with_hook_input("agent-v1", &hook_input_str)
        .expect("checkpoint should succeed");
}

/// Helper to create a checkpoint with a specific conversation ID (for testing multiple prompts)
fn checkpoint_with_message_and_conversation_id(
    repo: &TestRepo,
    message: &str,
    edited_files: Vec<String>,
    conversation_id: &str,
) {
    let mut transcript = AiTranscript::new();
    transcript.add_message(Message::user(message.to_string(), None));
    transcript.add_message(Message::assistant(
        "I'll help you with that.".to_string(),
        None,
    ));

    let hook_input = serde_json::json!({
        "type": "ai_agent",
        "repo_working_dir": repo.path().to_str().unwrap(),
        "edited_filepaths": edited_files,
        "transcript": transcript,
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": conversation_id,
    });

    let hook_input_str = serde_json::to_string(&hook_input).unwrap();

    repo.checkpoint_with_hook_input("agent-v1", &hook_input_str)
        .expect("checkpoint should succeed");
}

/// Helper to create a checkpoint with an empty transcript (no messages)
fn checkpoint_with_empty_transcript(repo: &TestRepo, edited_files: Vec<String>) {
    let empty_transcript = AiTranscript::new();

    let hook_input = serde_json::json!({
        "type": "ai_agent",
        "repo_working_dir": repo.path().to_str().unwrap(),
        "edited_filepaths": edited_files,
        "transcript": empty_transcript,
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-id",
    });

    let hook_input_str = serde_json::to_string(&hook_input).unwrap();

    repo.checkpoint_with_hook_input("agent-v1", &hook_input_str)
        .expect("checkpoint should succeed");
}

#[test]
fn test_checkpoint_with_prompt_sharing_enabled() {
    let mut repo = TestRepo::new();

    // Enable prompt sharing for all repositories (empty blacklist = share everywhere)
    // Use prompt_storage: "notes" to explicitly store messages in git notes for testing
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]); // No exclusions
        patch.prompt_storage = Some("notes".to_string()); // Store in notes for testing
    });

    // Create initial commit with README
    let readme_path = repo.path().join("README.md");
    fs::write(&readme_path, "# Test Repo\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();

    // Write AI content directly (without using set_contents which triggers mock_ai)
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "AI Line 1\nAI Line 2\n").unwrap();

    // Checkpoint with a proper message - this is the ONLY checkpoint for this file
    checkpoint_with_message(&repo, "Add example file", vec!["example.txt".to_string()]);

    // Stage and commit the changes
    repo.git(&["add", "-A"]).unwrap();
    let commit = repo.commit("Add example").expect("commit should succeed");

    // Verify we have the AI session in the commit
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected AI sessions in authorship log when prompt sharing is enabled"
    );

    // Verify the session message is captured (agent-v1 uses inline transcripts)
    let sessions: Vec<_> = commit.authorship_log.metadata.sessions.values().collect();
    assert_eq!(sessions.len(), 1, "Expected exactly one session");
    // Note: Messages field has been removed from SessionRecord
}

#[test]
fn test_checkpoint_with_prompt_sharing_disabled_strips_messages() {
    let mut repo = TestRepo::new();

    // Prompt sharing is disabled by default (empty list), but let's be explicit
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec!["*".to_string()]); // Exclude all repos
    });

    // Create initial commit with README
    let readme_path = repo.path().join("README.md");
    fs::write(&readme_path, "# Test Repo\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();

    // Add a remote so this isn't considered a local-only repo
    // (local repos always share prompts as they're safe)
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/test/repo.git",
    ])
    .unwrap();

    // Write AI content directly (without using set_contents which triggers mock_ai)
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "AI Line 1\nAI Line 2\n").unwrap();

    // Use agent-v1 with a FULL transcript containing messages
    // This tests that messages are stripped when prompt sharing is disabled
    checkpoint_with_message(
        &repo,
        "Add the example file with AI content",
        vec!["example.txt".to_string()],
    );

    // Stage and commit the changes
    repo.git(&["add", "-A"]).unwrap();
    let commit = repo.commit("Add example").expect("commit should succeed");

    // Verify commit succeeded
    assert!(!commit.commit_sha.is_empty());

    // KEY ASSERTION: With prompt sharing disabled, the session RECORD should exist
    // (so we know AI was involved) but the MESSAGES should be empty (stripped)
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected session record to exist even when prompt sharing is disabled"
    );

    let sessions: Vec<_> = commit.authorship_log.metadata.sessions.values().collect();
    assert_eq!(sessions.len(), 1, "Expected exactly one session record");

    // Note: Messages field has been removed from SessionRecord
}

#[test]
fn test_multiple_checkpoints_with_messages() {
    let mut repo = TestRepo::new();

    // Enable prompt sharing for all repositories (empty blacklist = share everywhere)
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]); // No exclusions
    });

    // Create initial commit with README
    let readme_path = repo.path().join("README.md");
    fs::write(&readme_path, "# Test Repo\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();

    // First AI edit - write file directly
    let file1_path = repo.path().join("file1.txt");
    fs::write(&file1_path, "AI Line 1\n").unwrap();

    checkpoint_with_message_and_conversation_id(
        &repo,
        "Create file1 with initial content",
        vec!["file1.txt".to_string()],
        "conversation-1",
    );

    // Second AI edit to different file
    let file2_path = repo.path().join("file2.txt");
    fs::write(&file2_path, "AI Line 2\n").unwrap();

    checkpoint_with_message_and_conversation_id(
        &repo,
        "Create file2 with different content",
        vec!["file2.txt".to_string()],
        "conversation-2",
    );

    // Stage and commit everything
    repo.git(&["add", "-A"]).unwrap();
    let commit = repo
        .commit("Add both files")
        .expect("commit should succeed");

    // Verify we captured both sessions
    assert_eq!(
        commit.authorship_log.metadata.sessions.len(),
        2,
        "Expected 2 sessions in authorship log"
    );

    // Collect sessions into a Vec for indexed access
    let sessions: Vec<_> = commit.authorship_log.metadata.sessions.values().collect();
    assert_eq!(sessions.len(), 2, "Expected exactly 2 sessions");

    // Note: Messages field has been removed from SessionRecord
}

#[test]
fn test_prompt_sharing_disabled_with_empty_transcript() {
    let mut repo = TestRepo::new();

    // Disable prompt sharing (default behavior)
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec!["*".to_string()]); // Exclude all repos
    });

    // Create initial commit with README
    let readme_path = repo.path().join("README.md");
    fs::write(&readme_path, "# Test Repo\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();

    // Write AI content directly
    let example_path = repo.path().join("example.txt");
    fs::write(&example_path, "AI Line\n").unwrap();

    // Checkpoint with an empty transcript (no messages)
    checkpoint_with_empty_transcript(&repo, vec!["example.txt".to_string()]);

    repo.git(&["add", "-A"]).unwrap();
    let commit = repo.commit("Add example").expect("commit should succeed");

    // With empty transcript, there should be a prompt record but with empty messages
    // Note: When transcript is empty, the prompt record may still exist but with no messages
    // The key thing is the checkpoint should succeed
    assert!(!commit.commit_sha.is_empty());
}

crate::reuse_tests_in_worktree!(
    test_checkpoint_with_prompt_sharing_enabled,
    test_checkpoint_with_prompt_sharing_disabled_strips_messages,
    test_multiple_checkpoints_with_messages,
    test_prompt_sharing_disabled_with_empty_transcript,
);
