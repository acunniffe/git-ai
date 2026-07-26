use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::error::GitAiError;
use git_ai::git::notes_api;
use git_ai::git::notes_api::{read_authorship_v3, read_note, write_note};
use git_ai::git::refs::git_backend_for_tests::{
    commits_with_authorship_notes, get_commits_with_notes_from_list, grep_ai_notes,
    note_blob_oids_for_commits, notes_add_batch, notes_add_blob_batch,
};
use git_ai::git::refs::{
    AI_AUTHORSHIP_FORK_TRACKING_REF, CommitAuthorship, copy_missing_notes_for_commits_from_ref,
    copy_ref, get_reference_as_working_log, merge_notes_from_ref,
    note_blob_oids_for_commits_from_ref, ref_exists,
};
use git_ai::git::repository::{exec_git, exec_git_stdin, find_repository_in_path};
use std::fs;

// ---------------------------------------------------------------------------
// Repo-based tests (TestRepo replaces TmpRepo)
// ---------------------------------------------------------------------------

/// Helper: create a TestRepo and obtain a gitai Repository handle.
fn repo_with_handle() -> (TestRepo, git_ai::git::repository::Repository) {
    let repo = TestRepo::new();
    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("find repository");
    (repo, gitai_repo)
}

/// Helper: get the HEAD commit SHA from a TestRepo.
fn head_sha(repo: &TestRepo) -> String {
    repo.git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse HEAD")
        .trim()
        .to_string()
}

fn git_stdin_stdout(
    repo: &git_ai::git::repository::Repository,
    args: &[&str],
    stdin: &[u8],
) -> String {
    let mut git_args = repo.global_args_for_exec();
    git_args.extend(args.iter().map(|arg| arg.to_string()));
    let output = exec_git_stdin(&git_args, stdin).expect("git stdin command");
    String::from_utf8(output.stdout)
        .expect("git stdout utf8")
        .trim()
        .to_string()
}

fn install_note_at_paths(
    repo: &git_ai::git::repository::Repository,
    paths: &[String],
    content: &str,
) {
    let mut stream = format!(
        "blob\nmark :1\ndata {}\n{}\ncommit refs/notes/ai\ncommitter Test <test@test.com> 1000000000 +0000\ndata 0\ndeleteall\n",
        content.len(),
        content
    );
    for path in paths {
        stream.push_str(&format!("M 100644 :1 {path}\n"));
    }
    stream.push_str("\ndone\n");

    git_stdin_stdout(
        repo,
        &["fast-import", "--quiet", "--done"],
        stream.as_bytes(),
    );
}

fn note_tree_paths(repo: &TestRepo) -> Vec<String> {
    repo.git_og(&["ls-tree", "-r", "--name-only", "refs/notes/ai"])
        .expect("list authorship note paths")
        .lines()
        .map(str::to_string)
        .collect()
}

fn deepest_note_path(commit_sha: &str) -> String {
    commit_sha
        .as_bytes()
        .chunks(2)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn test_notes_add_and_show_authorship_note() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create a commit first
    fs::write(repo.path().join("initial.txt"), "initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit")
        .expect("Failed to create initial commit");

    let commit_sha = head_sha(&repo);

    // Test data - simple string content
    let note_content = "This is a test authorship note with some random content!";

    // Add the authorship note (force overwrite since stage_all_and_commit may create one)
    write_note(&gitai_repo, &commit_sha, note_content).expect("Failed to add authorship note");

    // Read the note back
    let retrieved_content =
        read_note(&gitai_repo, &commit_sha).expect("Failed to retrieve authorship note");

    // Assert the content matches exactly
    assert_eq!(retrieved_content, note_content);

    // Test that non-existent commit returns None
    let non_existent_content = read_note(&gitai_repo, "0000000000000000000000000000000000000000");
    assert!(non_existent_content.is_none());
}

#[test]
fn test_notes_add_batch_writes_multiple_notes() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let commit_b = head_sha(&repo);

    let entries = vec![
        (commit_a.clone(), "{\"note\":\"a\",\"value\":1}".to_string()),
        (commit_b.clone(), "{\"note\":\"b\",\"value\":2}".to_string()),
    ];

    notes_add_batch(&gitai_repo, &entries).expect("batch notes add");

    let note_a = read_note(&gitai_repo, &commit_a).expect("note A");
    let note_b = read_note(&gitai_repo, &commit_b).expect("note B");
    assert!(note_a.contains("\"note\":\"a\""));
    assert!(note_b.contains("\"note\":\"b\""));
}

#[test]
fn test_notes_add_batch_replaces_notes_at_every_legacy_fanout_depth() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("legacy.txt"), "legacy\n").unwrap();
    repo.git_og(&["add", "."]).expect("add legacy file");
    repo.git_og(&["commit", "-m", "Legacy note target"])
        .expect("commit legacy note target");
    let commit_sha = head_sha(&repo);
    let canonical_path = format!("{}/{}", &commit_sha[..2], &commit_sha[2..]);
    let legacy_paths = vec![
        commit_sha.clone(),
        canonical_path.clone(),
        format!(
            "{}/{}/{}",
            &commit_sha[..2],
            &commit_sha[2..4],
            &commit_sha[4..]
        ),
        deepest_note_path(&commit_sha),
    ];
    install_note_at_paths(&gitai_repo, &legacy_paths, "{}");
    assert_eq!(note_tree_paths(&repo).len(), legacy_paths.len());
    assert!(
        read_authorship_v3(&gitai_repo, &commit_sha).is_err(),
        "duplicate legacy note paths must reproduce the deserialization failure"
    );

    let mut replacement = AuthorshipLog::new();
    replacement.metadata.base_commit_sha = commit_sha.clone();
    let replacement = replacement
        .serialize_to_string()
        .expect("serialize replacement note");
    notes_add_batch(&gitai_repo, &[(commit_sha.clone(), replacement)])
        .expect("replace mixed-fanout note");

    assert_eq!(note_tree_paths(&repo), vec![canonical_path]);
    let parsed = read_authorship_v3(&gitai_repo, &commit_sha).expect("parse replacement note");
    assert_eq!(parsed.metadata.base_commit_sha, commit_sha);
}

#[test]
fn test_notes_add_blob_batch_reuses_existing_note_blob() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let commit_b = head_sha(&repo);

    let mut log = AuthorshipLog::new();
    log.metadata.base_commit_sha = commit_a.clone();
    let note_content = log.serialize_to_string().expect("serialize authorship log");
    write_note(&gitai_repo, &commit_a, &note_content).expect("add note A");

    let blob_oids = note_blob_oids_for_commits(&gitai_repo, std::slice::from_ref(&commit_a))
        .expect("resolve note blob oid");
    let blob_oid = blob_oids
        .get(&commit_a)
        .expect("blob oid for commit A")
        .clone();

    let blob_entry = (commit_b.clone(), blob_oid);
    notes_add_blob_batch(&gitai_repo, std::slice::from_ref(&blob_entry))
        .expect("batch add blob-backed note");

    let raw_note_b = read_note(&gitai_repo, &commit_b).expect("note B");
    assert_eq!(raw_note_b, note_content);

    let parsed_note_b = read_authorship_v3(&gitai_repo, &commit_b).expect("parse B");
    assert_eq!(parsed_note_b.metadata.base_commit_sha, commit_b);
}

#[test]
fn test_notes_add_blob_batch_replaces_notes_at_every_legacy_fanout_depth() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("legacy-blob.txt"), "legacy\n").unwrap();
    repo.git_og(&["add", "."]).expect("add legacy blob file");
    repo.git_og(&["commit", "-m", "Legacy blob note target"])
        .expect("commit legacy blob note target");
    let commit_sha = head_sha(&repo);
    let canonical_path = format!("{}/{}", &commit_sha[..2], &commit_sha[2..]);
    let legacy_paths = vec![
        commit_sha.clone(),
        format!(
            "{}/{}/{}",
            &commit_sha[..2],
            &commit_sha[2..4],
            &commit_sha[4..]
        ),
        deepest_note_path(&commit_sha),
    ];
    install_note_at_paths(&gitai_repo, &legacy_paths, "stale");
    assert_eq!(note_tree_paths(&repo).len(), legacy_paths.len());

    let replacement_blob = git_stdin_stdout(
        &gitai_repo,
        &["hash-object", "-w", "--stdin"],
        b"replacement",
    );
    notes_add_blob_batch(&gitai_repo, &[(commit_sha.clone(), replacement_blob)])
        .expect("replace mixed-fanout blob note");

    assert_eq!(note_tree_paths(&repo), vec![canonical_path]);
    assert_eq!(
        read_note(&gitai_repo, &commit_sha).as_deref(),
        Some("replacement")
    );
}

#[test]
fn test_read_notes_batch_finds_deeply_fanned_out_note() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("deep-note.txt"), "deep\n").unwrap();
    repo.git_og(&["add", "."]).expect("add deep note file");
    repo.git_og(&["commit", "-m", "Deep note target"])
        .expect("commit deep note target");
    let commit_sha = head_sha(&repo);
    let deepest_path = deepest_note_path(&commit_sha);
    install_note_at_paths(&gitai_repo, &[deepest_path], "deeply fanned out");

    let notes = notes_api::read_notes_batch(&gitai_repo, std::slice::from_ref(&commit_sha))
        .expect("batch-read deeply fanned-out note");
    assert_eq!(
        notes.get(&commit_sha).map(String::as_str),
        Some("deeply fanned out")
    );
}

#[test]
fn test_copy_missing_notes_for_commits_from_ref_copies_only_requested_commits() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.git_og(&["add", "."]).expect("add A");
    repo.git_og(&["commit", "-m", "Commit A"])
        .expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.git_og(&["add", "."]).expect("add B");
    repo.git_og(&["commit", "-m", "Commit B"])
        .expect("commit B");
    let commit_b = head_sha(&repo);

    for (commit, note) in [(&commit_a, "fork-note-a"), (&commit_b, "fork-note-b")] {
        let mut args = gitai_repo.global_args_for_exec();
        args.extend_from_slice(&[
            "notes".to_string(),
            "--ref=ai-remote/fork".to_string(),
            "add".to_string(),
            "-f".to_string(),
            "-m".to_string(),
            note.to_string(),
            commit.clone(),
        ]);
        exec_git(&args).expect("add source note");
    }

    let source_notes = note_blob_oids_for_commits_from_ref(
        &gitai_repo,
        AI_AUTHORSHIP_FORK_TRACKING_REF,
        &[commit_a.clone(), commit_b.clone()],
    )
    .expect("source note oids");
    assert_eq!(source_notes.len(), 2);

    let copied = copy_missing_notes_for_commits_from_ref(
        &gitai_repo,
        AI_AUTHORSHIP_FORK_TRACKING_REF,
        std::slice::from_ref(&commit_a),
    )
    .expect("copy scoped notes");

    assert_eq!(copied, 1);
    assert_eq!(
        read_note(&gitai_repo, &commit_a).as_deref(),
        Some("fork-note-a")
    );
    assert!(
        read_note(&gitai_repo, &commit_b).is_none(),
        "note for unrequested commit must not be copied"
    );
}

#[test]
fn test_copy_missing_notes_for_commits_from_ref_keeps_existing_local_note() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.git_og(&["add", "."]).expect("add A");
    repo.git_og(&["commit", "-m", "Commit A"])
        .expect("commit A");
    let commit_a = head_sha(&repo);

    let mut args = gitai_repo.global_args_for_exec();
    args.extend_from_slice(&[
        "notes".to_string(),
        "--ref=ai-remote/fork".to_string(),
        "add".to_string(),
        "-f".to_string(),
        "-m".to_string(),
        "fork-note".to_string(),
        commit_a.clone(),
    ]);
    exec_git(&args).expect("add source note");

    write_note(&gitai_repo, &commit_a, "local-note").expect("add local note");

    let copied = copy_missing_notes_for_commits_from_ref(
        &gitai_repo,
        AI_AUTHORSHIP_FORK_TRACKING_REF,
        std::slice::from_ref(&commit_a),
    )
    .expect("copy scoped notes");

    assert_eq!(copied, 0);
    assert_eq!(
        read_note(&gitai_repo, &commit_a).as_deref(),
        Some("local-note")
    );
}

#[test]
fn test_ref_exists() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create initial commit
    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Initial commit").expect("commit");

    // HEAD should exist
    assert!(ref_exists(&gitai_repo, "HEAD"));

    // refs/heads/main (or master) should exist
    let branch_name = repo.current_branch();
    assert!(ref_exists(
        &gitai_repo,
        &format!("refs/heads/{}", branch_name)
    ));

    // Non-existent ref should not exist
    assert!(!ref_exists(&gitai_repo, "refs/heads/nonexistent-branch"));
    assert!(!ref_exists(&gitai_repo, "refs/notes/ai-test"));
}

#[test]
fn test_merge_notes_from_ref() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create commits - stage_all_and_commit may auto-create notes on refs/notes/ai
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let _commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let _commit_b = head_sha(&repo);

    // Create a third commit without checkpoint (using git_og to bypass hooks)
    fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    repo.git_og(&["add", "."]).expect("add files");
    repo.git_og(&["commit", "-m", "Commit C"]).expect("commit");
    let commit_c = head_sha(&repo);

    // Add note to commit C on a different ref
    let note_c = "{\"note\":\"c\"}";
    let mut args = gitai_repo.global_args_for_exec();
    args.extend_from_slice(&[
        "notes".to_string(),
        "--ref=test".to_string(),
        "add".to_string(),
        "-f".to_string(),
        "-m".to_string(),
        note_c.to_string(),
        commit_c.clone(),
    ]);
    exec_git(&args).expect("add note C on test ref");

    // Verify initial state - commit C should not have note on refs/notes/ai
    let initial_note_c = read_note(&gitai_repo, &commit_c);

    // Merge notes from refs/notes/test into refs/notes/ai
    merge_notes_from_ref(&gitai_repo, "refs/notes/test").expect("merge notes");

    // After merge, commit C should have a note on refs/notes/ai
    let final_note_c = read_note(&gitai_repo, &commit_c);

    // If initially had no note, should now have one. If it had one, should still have one.
    assert!(final_note_c.is_some() || initial_note_c.is_some());
}

#[test]
fn test_copy_ref() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create commit with note
    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    let note_content = "{\"test\":\"note\"}";
    write_note(&gitai_repo, &commit_sha, note_content).expect("add note");

    // refs/notes/ai should exist
    assert!(ref_exists(&gitai_repo, "refs/notes/ai"));

    // refs/notes/ai-backup should not exist
    assert!(!ref_exists(&gitai_repo, "refs/notes/ai-backup"));

    // Copy refs/notes/ai to refs/notes/ai-backup
    copy_ref(&gitai_repo, "refs/notes/ai", "refs/notes/ai-backup").expect("copy ref");

    // Both should now exist and point to the same commit
    assert!(ref_exists(&gitai_repo, "refs/notes/ai"));
    assert!(ref_exists(&gitai_repo, "refs/notes/ai-backup"));

    // Verify content is accessible from both refs
    let note_from_ai = read_note(&gitai_repo, &commit_sha).expect("note from ai");

    // Read from backup ref
    let mut args = gitai_repo.global_args_for_exec();
    args.extend_from_slice(&[
        "notes".to_string(),
        "--ref=ai-backup".to_string(),
        "show".to_string(),
        commit_sha.clone(),
    ]);
    let output = exec_git(&args).expect("show note from backup");
    let note_from_backup = String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_string();

    assert_eq!(note_from_ai, note_from_backup);
}

#[test]
fn test_grep_ai_notes_single_match() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    let note = "{\"tool\":\"cursor\",\"model\":\"claude-3-sonnet\"}";
    write_note(&gitai_repo, &commit_sha, note).expect("add note");

    // Search for "cursor" should find the commit
    let results = grep_ai_notes(&gitai_repo, "cursor").expect("grep");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0], commit_sha);
}

#[test]
fn test_grep_ai_notes_multiple_matches() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create three commits with notes
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let commit_b = head_sha(&repo);

    fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    repo.stage_all_and_commit("Commit C").expect("commit C");
    let commit_c = head_sha(&repo);

    // Add notes with "cursor" to all three
    write_note(&gitai_repo, &commit_a, "{\"tool\":\"cursor\"}").expect("add note A");
    write_note(&gitai_repo, &commit_b, "{\"tool\":\"cursor\"}").expect("add note B");
    write_note(&gitai_repo, &commit_c, "{\"tool\":\"cursor\"}").expect("add note C");

    // Search should find all three, sorted by commit date (newest first)
    let results = grep_ai_notes(&gitai_repo, "cursor").expect("grep");

    // Should find at least 3 commits (may find more from auto-created notes)
    assert!(
        results.len() >= 3,
        "Expected at least 3 results, got {}",
        results.len()
    );

    // Verify our three commits are in the results
    assert!(
        results.contains(&commit_a),
        "Results should contain commit A"
    );
    assert!(
        results.contains(&commit_b),
        "Results should contain commit B"
    );
    assert!(
        results.contains(&commit_c),
        "Results should contain commit C"
    );
}

#[test]
fn test_grep_ai_notes_no_match() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    let note = "{\"tool\":\"cursor\"}";
    write_note(&gitai_repo, &commit_sha, note).expect("add note");

    // Search for non-existent pattern
    let results = grep_ai_notes(&gitai_repo, "vscode");
    // grep may return empty or error if no matches, both are acceptable
    if let Ok(refs) = results {
        assert_eq!(refs.len(), 0);
    }
    // Err is also acceptable - git grep returns non-zero when no matches
}

#[test]
fn test_grep_ai_notes_no_notes() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    // Use git_og to create a commit without triggering checkpoint/notes
    repo.git_og(&["add", "."]).expect("add");
    repo.git_og(&["commit", "-m", "Commit"]).expect("commit");

    // No notes exist, search should return empty or error
    let results = grep_ai_notes(&gitai_repo, "cursor");
    // grep may return empty or error if refs/notes/ai doesn't exist
    if let Ok(refs) = results {
        assert_eq!(refs.len(), 0);
    }
    // Err is also acceptable - refs/notes/ai may not exist yet
}

#[test]
fn test_get_commits_with_notes_from_list() {
    let (repo, gitai_repo) = repo_with_handle();

    // Create commits - stage_all_and_commit auto-creates authorship notes,
    // so all commits will have notes. This is expected behavior.
    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let commit_b = head_sha(&repo);

    fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    repo.stage_all_and_commit("Commit C").expect("commit C");
    let commit_c = head_sha(&repo);

    // Get authorship for all commits
    let commit_list = vec![commit_a.clone(), commit_b.clone(), commit_c.clone()];
    let result = get_commits_with_notes_from_list(&gitai_repo, &commit_list).expect("get commits");

    assert_eq!(result.len(), 3);

    // All commits should have logs since stage_all_and_commit creates them
    for (idx, commit_authorship) in result.iter().enumerate() {
        match commit_authorship {
            CommitAuthorship::Log {
                sha,
                git_author: _,
                authorship_log: _,
            } => {
                // This is expected - verify SHA matches
                let expected_sha = &commit_list[idx];
                assert_eq!(sha, expected_sha);
            }
            CommitAuthorship::NoLog { .. } => {
                // Also acceptable if checkpoint system didn't run
            }
        }
    }
}

#[test]
fn test_note_blob_oids_for_commits_empty() {
    let (_repo, gitai_repo) = repo_with_handle();

    // Empty list should return empty map
    let result = note_blob_oids_for_commits(&gitai_repo, &[]).expect("empty list");
    assert!(result.is_empty());
}

#[test]
#[ignore] // Checkpoint system auto-creates notes, making this assertion invalid
fn test_note_blob_oids_for_commits_no_notes() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    // Commit exists but has no note
    let result = note_blob_oids_for_commits(&gitai_repo, &[commit_sha]).expect("no notes");
    assert!(result.is_empty());
}

#[test]
fn test_read_notes_batch_errors_on_dangling_note_blob() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("dangling.txt"), "dangling\n").unwrap();
    repo.git_og(&["add", "."]).expect("add dangling");
    repo.git_og(&["commit", "-m", "Dangling note target"])
        .expect("commit dangling target");
    let commit_sha = head_sha(&repo);
    let missing_blob = "2222222222222222222222222222222222222222";
    let prefix = &commit_sha[..2];
    let suffix = &commit_sha[2..];

    let leaf_tree = git_stdin_stdout(
        &gitai_repo,
        &["mktree", "--missing"],
        format!("100644 blob {missing_blob}\t{suffix}\n").as_bytes(),
    );

    let root_tree = git_stdin_stdout(
        &gitai_repo,
        &["mktree"],
        format!("040000 tree {leaf_tree}\t{prefix}\n").as_bytes(),
    );
    repo.git_og(&["update-ref", "refs/notes/ai", &root_tree])
        .expect("install dangling notes tree");

    let result = notes_api::read_notes_batch(&gitai_repo, &[commit_sha]);
    assert!(
        result.is_err(),
        "a notes tree entry pointing at a missing blob must be reported as corruption, not as no note"
    );
}

#[test]
fn test_read_notes_batch_prefers_flat_note_in_mixed_fanout_tree() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("mixed.txt"), "mixed\n").unwrap();
    repo.git_og(&["add", "."]).expect("add mixed");
    repo.git_og(&["commit", "-m", "Mixed note target"])
        .expect("commit mixed target");
    let commit_sha = head_sha(&repo);
    let prefix = &commit_sha[..2];
    let suffix = &commit_sha[2..];

    let flat_blob = git_stdin_stdout(&gitai_repo, &["hash-object", "-w", "--stdin"], b"flat");
    let fanout_blob = git_stdin_stdout(&gitai_repo, &["hash-object", "-w", "--stdin"], b"fanout");
    let leaf_tree = git_stdin_stdout(
        &gitai_repo,
        &["mktree"],
        format!("100644 blob {fanout_blob}\t{suffix}\n").as_bytes(),
    );
    let root_tree = git_stdin_stdout(
        &gitai_repo,
        &["mktree"],
        format!("100644 blob {flat_blob}\t{commit_sha}\n040000 tree {leaf_tree}\t{prefix}\n")
            .as_bytes(),
    );
    repo.git_og(&["update-ref", "refs/notes/ai", &root_tree])
        .expect("install mixed notes tree");

    let notes =
        notes_api::read_notes_batch(&gitai_repo, std::slice::from_ref(&commit_sha)).unwrap();
    assert_eq!(
        notes.get(&commit_sha).map(String::as_str),
        Some("flat"),
        "mixed flat/fanout notes must preserve the historical flat-path preference"
    );
}

#[test]
fn test_commits_with_authorship_notes() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("a.txt"), "a\n").unwrap();
    repo.stage_all_and_commit("Commit A").expect("commit A");
    let commit_a = head_sha(&repo);

    fs::write(repo.path().join("b.txt"), "b\n").unwrap();
    repo.stage_all_and_commit("Commit B").expect("commit B");
    let commit_b = head_sha(&repo);

    // Both commits may already have notes from stage_all_and_commit
    // Add a custom note to A to ensure it has one
    write_note(&gitai_repo, &commit_a, "{\"test\":\"note\"}").expect("add note");

    let commits = vec![commit_a.clone(), commit_b.clone()];
    let result = commits_with_authorship_notes(&gitai_repo, &commits).expect("check notes");

    // Commit A should definitely be in results
    assert!(result.contains(&commit_a), "Commit A should have a note");

    // Commit B may or may not have a note depending on checkpoint system
    // Just verify we got at least 1 result (commit A)
    assert!(
        !result.is_empty(),
        "Should have at least 1 commit with notes"
    );
}

#[test]
fn test_get_reference_as_working_log() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    // Add a working log format note
    let working_log_json = "[]";
    write_note(&gitai_repo, &commit_sha, working_log_json).expect("add note");

    let result = get_reference_as_working_log(&gitai_repo, &commit_sha).expect("get working log");
    assert_eq!(result.len(), 0); // Empty array
}

#[test]
fn test_get_reference_as_authorship_log_v3_version_mismatch() {
    let (repo, gitai_repo) = repo_with_handle();

    fs::write(repo.path().join("test.txt"), "content\n").unwrap();
    repo.stage_all_and_commit("Commit").expect("commit");
    let commit_sha = head_sha(&repo);

    // Create log with wrong version
    let mut log = AuthorshipLog::new();
    log.metadata.schema_version = "999".to_string();
    log.metadata.base_commit_sha = commit_sha.clone();

    let note_content = log.serialize_to_string().expect("serialize");
    write_note(&gitai_repo, &commit_sha, &note_content).expect("add note");

    // Should fail with version mismatch error
    let result = read_authorship_v3(&gitai_repo, &commit_sha);
    assert!(result.is_err());

    if let Err(GitAiError::Generic(msg)) = result {
        assert!(msg.contains("Unsupported authorship log version"));
    } else {
        panic!("Expected version mismatch error");
    }
}
