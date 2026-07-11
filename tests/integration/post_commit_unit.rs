use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

fn assert_no_ai_attestations(log: &AuthorshipLog) {
    assert!(
        log.attestations
            .iter()
            .flat_map(|attestation| &attestation.entries)
            .all(|entry| entry.hash == "human" || entry.hash.starts_with("h_")),
        "Expected no AI attestations: {:?}",
        log.attestations
    );
    assert!(
        log.metadata.prompts.is_empty() && log.metadata.sessions.is_empty(),
        "Expected no AI metadata: {:?}",
        log.metadata
    );
}

#[test]
fn test_post_commit_empty_repo_with_checkpoint() {
    // Create an empty repo (no commits yet)
    let repo = TestRepo::new();

    // Write file without staging
    std::fs::write(repo.path().join("test.txt"), "Hello, world!\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    // Append to file
    std::fs::write(repo.path().join("test.txt"), "Hello, world!\nSecond line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    // Stage and commit - this triggers the post-commit hook
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // The key assertion: post_commit didn't panic. We can verify by checking authorship note exists
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // If post_commit ran successfully via the git hook, an authorship note should exist
    let note_result = repo.read_authorship_note(&head_sha);

    // It should succeed (the note was created during commit)
    assert!(
        note_result.is_some(),
        "post_commit should handle empty repo without errors"
    );
}

#[test]
fn test_post_commit_empty_repo_no_checkpoint() {
    // Create an empty repo (no commits yet)
    let repo = TestRepo::new();

    // Create a file without checkpointing
    std::fs::write(repo.path().join("test.txt"), "Hello, world!\n").unwrap();

    // Stage and commit without prior checkpoint - this triggers the post-commit hook
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Should not panic or error even with no working log
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // With no checkpoints, authorship log should have empty attestations
    let note = repo.read_authorship_note(&head_sha);
    assert!(note.is_some(), "Should have authorship note");

    // No checkpoints = no AI attribution. Pure human recovery may add h_ attestations.
    let log = AuthorshipLog::deserialize_from_string(&note.unwrap()).unwrap();
    assert_no_ai_attestations(&log);
}

#[test]
fn test_post_commit_utf8_filename_with_ai_attribution() {
    // Create a repo with an initial commit
    let repo = TestRepo::new();

    // Create initial file and commit
    std::fs::write(repo.path().join("README.md"), "# Test\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create a file with Chinese characters in the filename
    let chinese_filename = "中文文件.txt";
    std::fs::write(repo.path().join(chinese_filename), "Hello, 世界!\n").unwrap();
    repo.git(&["add", chinese_filename]).unwrap();

    // Trigger AI checkpoint
    repo.git_ai(&["checkpoint", "mock_ai", chinese_filename])
        .unwrap();

    // Commit - this triggers the post-commit hook
    repo.stage_all_and_commit("Add Chinese file").unwrap();

    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let note = repo
        .read_authorship_note(&head_sha)
        .expect("should have authorship note");

    // The note should reference the Chinese filename
    // Deserialize and check attestations contain the file
    let log = AuthorshipLog::deserialize_from_string(&note).unwrap();

    // Debug output
    println!("Authorship log attestations: {:?}", log.attestations);

    // The attestation should include the Chinese filename
    assert_eq!(
        log.attestations.len(),
        1,
        "Should have 1 attestation for the Chinese-named file"
    );
    assert_eq!(
        log.attestations[0].file_path, chinese_filename,
        "File path should be the UTF-8 filename"
    );
}
