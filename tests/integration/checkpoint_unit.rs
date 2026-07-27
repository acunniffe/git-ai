use crate::repos::test_repo::TestRepo;
use git_ai::authorship::working_log::{AgentId, Checkpoint, CheckpointKind, WorkingLogEntry};
use git_ai::commands::checkpoint_agent::orchestrator::{
    BaseCommit, CheckpointFile, CheckpointRequest,
};
use git_ai::daemon::checkpoint::{
    PreparedPathRole, ResolvedCheckpointExecution, compute_file_line_stats,
    execute_resolved_checkpoint_from_daemon, is_ai_author_id,
};
use git_ai::git::repository::find_repository_in_path;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Helper function equivalent to TmpRepo::new_with_base_commit()
fn setup_repo_with_base_commit() -> (TestRepo, String, String) {
    let repo = TestRepo::new();

    let lines_content = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n21\n22\n23\n24\n25\n26\n";
    let alphabet_content =
        "A\nB\nC\nD\nE\nF\nG\nH\nI\nJ\nK\nL\nM\nN\nO\nP\nQ\nR\nS\nT\nU\nV\nW\nX\nY\nZ\n";

    std::fs::write(repo.path().join("lines.md"), lines_content).unwrap();
    std::fs::write(repo.path().join("alphabet.md"), alphabet_content).unwrap();
    repo.git(&["add", "lines.md", "alphabet.md"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "lines.md"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "alphabet.md"])
        .unwrap();
    repo.stage_all_and_commit("initial commit").unwrap();

    (repo, "lines.md".to_string(), "alphabet.md".to_string())
}

#[test]
fn test_checkpoint_with_staged_changes() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Make changes to the file
    let file_path = repo.path().join(&lines_file);
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("New line added by user\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    // Run checkpoint - it should track the changes even though they're staged
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Verify the checkpoint was created with correct entries
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    // The bug: when changes are staged, entries_len is 0 instead of 1
    assert_eq!(
        latest.entries.len(),
        1,
        "Should have 1 file entry in checkpoint (staged changes should be tracked)"
    );
}

#[test]
fn test_checkpoint_with_staged_changes_after_previous_checkpoint() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Make first changes and checkpoint
    let file_path = repo.path().join(&lines_file);
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("First change\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Make second changes - these are staged
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Second change\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    // Run checkpoint again - it should track the staged changes even after a previous checkpoint
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Verify the checkpoint was created with correct entries
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    assert_eq!(
        latest.entries.len(),
        1,
        "Second checkpoint: should have 1 file entry in checkpoint (staged changes should be tracked)"
    );
}

#[test]
fn test_checkpoint_with_only_staged_no_unstaged_changes() {
    use std::fs;

    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Get the file path
    let file_path = repo.path().join(&lines_file);

    // Manually modify the file (bypassing TmpFile's automatic staging)
    let mut content = fs::read_to_string(&file_path).unwrap();
    content.push_str("New line for staging test\n");
    fs::write(&file_path, &content).unwrap();

    // Now manually stage it using git (this is what "git add" does)
    repo.git(&["add", &lines_file]).unwrap();

    // At this point: HEAD has old content, index has new content, workdir has new content
    // And unstaged should be "Unmodified" because workdir == index

    // Now run checkpoint
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Verify the checkpoint was created with correct entries
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    // This should work: we should see 1 file with 1 entry
    assert_eq!(
        latest.entries.len(),
        1,
        "Should track the staged changes in checkpoint"
    );
}

#[test]
fn test_checkpoint_with_only_unstaged_changes_for_ai_without_pathspec() {
    use std::fs;

    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Manually modify the file without staging it
    let file_path = repo.path().join(&lines_file);
    let mut content = fs::read_to_string(&file_path).unwrap();
    content.push_str("New unstaged AI line\n");
    fs::write(&file_path, &content).unwrap();

    // Trigger AI checkpoint without edited_filepaths (pathspec-less flow used by some agents)
    repo.git_ai(&["checkpoint", "mock_ai", &lines_file])
        .unwrap();

    // Verify the checkpoint was created
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    assert_eq!(
        latest.entries.len(),
        1,
        "Should create an AI checkpoint entry for unstaged changes without pathspecs"
    );
}

#[test]
fn test_checkpoint_base_override_controls_head_context_for_entry_generation() {
    use std::fs;

    let (repo, lines_file, _) = setup_repo_with_base_commit();
    let file_path = repo.path().join(&lines_file);

    fs::write(&file_path, "line from commit A\n").unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.stage_all_and_commit("commit A").unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    fs::write(&file_path, "line from commit B\n").unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.stage_all_and_commit("commit B").unwrap();

    // Keep the worktree dirty so git status returns this file, but inject deterministic
    // content from commit B via the CheckpointFile content field.
    fs::write(&file_path, "line from uncommitted edit\n").unwrap();

    let checkpoint_request = CheckpointRequest {
        trace_id: "base-override-regression".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: Some(AgentId {
            tool: "mock_ai".to_string(),
            id: "base-override-regression".to_string(),
            model: "test".to_string(),
        }),
        files: vec![CheckpointFile {
            path: PathBuf::from(&lines_file),
            content: Some("line from commit B\n".to_string()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Sha(base_commit.clone()),
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    let mut dirty_files = HashMap::new();
    dirty_files.insert(lines_file.clone(), Arc::from("line from commit B\n"));

    let resolved = ResolvedCheckpointExecution {
        base_commit,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        files: vec![lines_file],
        dirty_files,
    };

    execute_resolved_checkpoint_from_daemon(
        &gitai_repo,
        "mock-ai",
        CheckpointKind::AiAgent,
        checkpoint_request,
        resolved,
    )
    .unwrap();
}

#[test]
fn test_ai_checkpoint_without_agent_id_is_rejected() {
    let (repo, lines_file, _) = setup_repo_with_base_commit();
    let file_path = repo.path().join(&lines_file);
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let content = "changed without agent identity\n";
    std::fs::write(&file_path, content).unwrap();

    let checkpoint_request = CheckpointRequest {
        trace_id: "missing-agent-regression".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: None,
        files: vec![CheckpointFile {
            path: PathBuf::from(&lines_file),
            content: Some(content.to_string()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Sha(base_commit.clone()),
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let resolved = ResolvedCheckpointExecution {
        base_commit,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        files: vec![lines_file.clone()],
        dirty_files: HashMap::from([(lines_file, Arc::from(content))]),
    };

    let error = execute_resolved_checkpoint_from_daemon(
        &gitai_repo,
        "mock-ai",
        CheckpointKind::AiAgent,
        checkpoint_request,
        resolved,
    )
    .expect_err("AI checkpoints must carry an agent_id");

    assert!(
        error.to_string().contains("missing agent_id"),
        "unexpected error: {error}"
    );
}

#[test]
fn test_checkpoint_records_conflicted_files() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Get the current branch name (whatever the default is)
    let base_branch = repo.current_branch();

    // Create a branch and make different changes on each branch to create a conflict
    repo.git(&["checkout", "-b", "feature-branch"]).unwrap();

    // On feature branch, modify the file
    let file_path = repo.path().join(&lines_file);
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Feature branch change\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();
    repo.stage_all_and_commit("Feature commit").unwrap();

    // Switch back to base branch and make conflicting changes
    repo.git(&["checkout", &base_branch]).unwrap();
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Main branch change\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();
    repo.stage_all_and_commit("Main commit").unwrap();

    // Attempt to merge feature-branch into base branch - this should create a conflict
    let output = repo.git_og(&["merge", "feature-branch"]);
    let has_conflicts = output.is_err();
    assert!(has_conflicts, "Should have merge conflicts");

    // Try to checkpoint while there are conflicts
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints_before = working_log.read_all_checkpoints().unwrap();
    let count_before = checkpoints_before.len();

    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Checkpoints record conflicted files so conflict-resolution attribution can be
    // merged into the eventual rebase/merge commit.
    let checkpoints_after = working_log.read_all_checkpoints().unwrap();
    assert!(
        checkpoints_after.len() > count_before,
        "Should create a checkpoint for conflicted files"
    );
    let latest = checkpoints_after.last().unwrap();
    assert!(
        latest.entries.iter().any(|entry| entry.file == lines_file),
        "Should record an entry for the conflicted file"
    );
}

#[test]
fn test_checkpoint_with_paths_outside_repo() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Make changes to the file
    let file_path = repo.path().join(&lines_file);
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("New line added\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = gitai_repo.head().unwrap().target().unwrap();

    // Build a resolved checkpoint with only the valid file (outside paths filtered at resolution)
    let resolved = ResolvedCheckpointExecution {
        base_commit: base_commit.clone(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        files: vec![lines_file.clone()],
        dirty_files: HashMap::from([(lines_file.clone(), Arc::from(content.clone()))]),
    };

    let checkpoint_request = CheckpointRequest {
        trace_id: "test-outside-paths".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: Some(AgentId {
            tool: "test_tool".to_string(),
            id: "test_session".to_string(),
            model: "test_model".to_string(),
        }),
        files: vec![CheckpointFile {
            path: file_path,
            content: Some(content.clone()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Sha(base_commit),
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };

    let result = execute_resolved_checkpoint_from_daemon(
        &gitai_repo,
        "test_user",
        CheckpointKind::AiAgent,
        checkpoint_request,
        resolved,
    );

    assert!(
        result.is_ok(),
        "Checkpoint should succeed: {:?}",
        result.err()
    );
}

#[test]
fn test_checkpoint_filters_external_paths_from_stored_checkpoints() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Get access to the working log storage
    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());

    // Manually inject a checkpoint with an external file path (simulating the bug)
    // This is what happens when a file outside the repo was tracked before the fix
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();

    let external_entry = WorkingLogEntry::new(
        "/external/path/outside/repo.txt".to_string(),
        "fake_sha_for_external".to_string(),
        vec![],
        vec![],
    );

    let fake_checkpoint = Checkpoint::new(
        CheckpointKind::Human,
        "fake_diff".to_string(),
        "test_author".to_string(),
        vec![external_entry],
    );

    // Store the checkpoint with external path
    working_log
        .append_checkpoint(&fake_checkpoint)
        .expect("Should be able to append checkpoint");

    // Now make actual changes to a file in the repo
    let file_path = repo.path().join(&lines_file);
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("New line for testing\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    // Run checkpoint - this should NOT crash even though there's an external path stored
    // Previously this would fail with: "fatal: /external/path/outside/repo.txt is outside repository"
    let result = repo.git_ai(&["checkpoint", "mock_known_human", &lines_file]);

    assert!(
        result.is_ok(),
        "Checkpoint should succeed even with external paths stored in previous checkpoints: {:?}",
        result.err()
    );

    // Verify the new checkpoint only processed the valid file
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    // Should only process the valid file in the repo
    assert_eq!(
        latest.entries.len(),
        1,
        "Should process 1 valid file (external path should be filtered)"
    );
}

#[test]
fn test_checkpoint_works_after_conflict_resolution_maintains_authorship() {
    // Create a repo with an initial commit
    let (repo, lines_file, _) = setup_repo_with_base_commit();

    // Get the current branch name (whatever the default is)
    let base_branch = repo.current_branch();

    // Checkpoint initial state to track the base authorship
    let file_path = repo.path().join(&lines_file);
    let initial_content = std::fs::read_to_string(&file_path).unwrap();
    println!("Initial content:\n{}", initial_content);

    // Create a branch and make changes
    repo.git(&["checkout", "-b", "feature-branch"]).unwrap();
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Feature line 1\n");
    content.push_str("Feature line 2\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", &lines_file])
        .unwrap();
    repo.stage_all_and_commit("Feature commit").unwrap();

    // Switch back to base branch and make conflicting changes
    repo.git(&["checkout", &base_branch]).unwrap();
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Main line 1\n");
    content.push_str("Main line 2\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();
    repo.stage_all_and_commit("Main commit").unwrap();

    // Attempt to merge feature-branch into base branch - this should create a conflict
    let output = repo.git_og(&["merge", "feature-branch"]);
    let has_conflicts = output.is_err();
    assert!(has_conflicts, "Should have merge conflicts");

    // While there are conflicts, checkpoint should still record the file so the
    // eventual resolution can carry explicit attribution.
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints_before_conflict_checkpoint = working_log.read_all_checkpoints().unwrap();
    let count_before = checkpoints_before_conflict_checkpoint.len();

    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    // Checkpoint should record conflicted files during the conflict.
    let checkpoints_after_conflict_checkpoint = working_log.read_all_checkpoints().unwrap();
    assert!(
        checkpoints_after_conflict_checkpoint.len() > count_before,
        "Should create a checkpoint for conflicted files"
    );
    let checkpoint_during_conflict = checkpoints_after_conflict_checkpoint.last().unwrap();
    assert!(
        checkpoint_during_conflict
            .entries
            .iter()
            .any(|entry| entry.file == lines_file),
        "Should record conflicted files during conflict"
    );

    // Resolve the conflict by choosing "ours" (base branch)
    repo.git_og(&["checkout", "--ours", &lines_file]).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    // Verify content to ensure the resolution was applied correctly
    let resolved_content = std::fs::read_to_string(&file_path).unwrap();
    println!("Resolved content after resolution:\n{}", resolved_content);
    assert!(
        resolved_content.contains("Main line 1"),
        "Should contain base branch content (we chose 'ours')"
    );
    assert!(
        resolved_content.contains("Main line 2"),
        "Should contain base branch content (we chose 'ours')"
    );
    assert!(
        !resolved_content.contains("Feature line 1"),
        "Should not contain feature branch content (we chose 'ours')"
    );

    // After resolution, make additional changes to test that checkpointing works again
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("Post-resolution line 1\n");
    content.push_str("Post-resolution line 2\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    // Now checkpoint should work and track the new changes
    repo.git_ai(&["checkpoint", "mock_known_human", &lines_file])
        .unwrap();

    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    println!(
        "After resolution and new changes: entries_len={}",
        latest.entries.len()
    );

    // The file should be tracked with the new changes
    assert_eq!(
        latest.entries.len(),
        1,
        "Should create 1 entry for new changes after conflict resolution"
    );
}

#[test]
fn test_known_human_checkpoint_without_ai_history_records_h_hash_attributions() {
    let repo = TestRepo::new();

    std::fs::write(repo.path().join("simple.txt"), "one\n").unwrap();
    repo.git(&["add", "simple.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "simple.txt"])
        .unwrap();
    repo.stage_all_and_commit("seed commit").unwrap();

    let file_path = repo.path().join("simple.txt");
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("two\n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", "simple.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "simple.txt"])
        .unwrap();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();
    let entry = latest
        .entries
        .iter()
        .find(|entry| entry.file == "simple.txt")
        .unwrap();

    // KnownHuman checkpoints always record h_<hash> line attributions, even with no AI history.
    // This allows downstream stats to count these lines as human_additions.
    assert!(
        !entry.line_attributions.is_empty(),
        "KnownHuman checkpoint should record line-level h_<hash> attributions"
    );
    assert!(
        entry
            .line_attributions
            .iter()
            .all(|la| la.author_id.starts_with("h_")),
        "All line attributions should be h_<hash> IDs"
    );
    assert!(
        latest.line_stats.additions > 0,
        "KnownHuman checkpoint should record line stats"
    );
}

#[test]
fn test_human_checkpoint_keeps_attributions_for_ai_touched_file() {
    let (repo, lines_file, alphabet_file) = setup_repo_with_base_commit();

    let lines_path = repo.path().join(&lines_file);
    let alphabet_path = repo.path().join(&alphabet_file);

    let mut content = std::fs::read_to_string(&lines_path).unwrap();
    content.push_str("ai change\n");
    std::fs::write(&lines_path, &content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", &lines_file])
        .unwrap();

    let mut lines_content = std::fs::read_to_string(&lines_path).unwrap();
    lines_content.push_str("human after ai\n");
    std::fs::write(&lines_path, &lines_content).unwrap();
    repo.git(&["add", &lines_file]).unwrap();

    let mut alphabet_content = std::fs::read_to_string(&alphabet_path).unwrap();
    alphabet_content.push_str("human only\n");
    std::fs::write(&alphabet_path, &alphabet_content).unwrap();
    repo.git(&["add", &alphabet_file]).unwrap();

    repo.git_ai(&[
        "checkpoint",
        "mock_known_human",
        &lines_file,
        &alphabet_file,
    ])
    .unwrap();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints.last().unwrap();

    let ai_touched_entry = latest
        .entries
        .iter()
        .find(|entry| entry.file == "lines.md")
        .unwrap();
    assert!(
        !ai_touched_entry.attributions.is_empty() || !ai_touched_entry.line_attributions.is_empty(),
        "AI-touched file should keep attribution tracking"
    );

    let human_only_entry = latest
        .entries
        .iter()
        .find(|entry| entry.file == "alphabet.md")
        .unwrap();
    // KnownHuman checkpoints record h_<hash> attributions for all files, including
    // files with no AI history. This ensures human lines are counted correctly in stats.
    assert!(
        !human_only_entry.line_attributions.is_empty(),
        "KnownHuman checkpoint should record line attributions for human-only files"
    );
    assert!(
        human_only_entry
            .line_attributions
            .iter()
            .all(|la| la.author_id.starts_with("h_")),
        "Human-only file attributions should all be h_<hash> IDs"
    );
}

#[test]
fn test_checkpoint_skips_default_ignored_files() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("README.md"), "# repo\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    std::fs::write(repo.path().join("README.md"), "# repo\n\nupdated\n").unwrap();
    std::fs::write(repo.path().join("Cargo.lock"), "# lock\n# lock2\n").unwrap();

    // Checkpoint both files explicitly (CLI doesn't support "." the same way)
    repo.git(&["add", "README.md"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "README.md"])
        .unwrap();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();

    // Should have at least one checkpoint
    assert!(
        !checkpoints.is_empty(),
        "Should have at least one checkpoint"
    );
    let latest = checkpoints.last().unwrap();

    assert!(
        latest.entries.iter().any(|entry| entry.file == "README.md"),
        "Expected non-ignored source file to be checkpointed"
    );
    assert!(
        latest
            .entries
            .iter()
            .all(|entry| entry.file != "Cargo.lock"),
        "Expected Cargo.lock to be filtered by default ignore patterns"
    );
}

#[test]
fn test_checkpoint_skips_linguist_generated_files_from_root_gitattributes() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("README.md"), "# repo\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    std::fs::write(
        repo.path().join(".gitattributes"),
        "generated/** linguist-generated\n",
    )
    .unwrap();
    repo.git(&["add", ".gitattributes"]).unwrap();
    repo.stage_all_and_commit("attrs").unwrap();

    std::fs::create_dir_all(repo.path().join("generated")).unwrap();
    std::fs::write(
        repo.path().join("generated").join("api.generated.ts"),
        "// generated\n// generated 2\n",
    )
    .unwrap();
    std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").unwrap();
    repo.git(&["add", "main.rs"]).unwrap();

    // Checkpoint the non-generated file
    repo.git_ai(&["checkpoint", "mock_known_human", "main.rs"])
        .unwrap();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();

    // Should have at least one checkpoint
    assert!(
        !checkpoints.is_empty(),
        "Should have at least one checkpoint"
    );
    let latest = checkpoints.last().unwrap();

    assert!(
        latest.entries.iter().any(|entry| entry.file == "main.rs"),
        "Expected non-generated file to be checkpointed"
    );
    assert!(
        latest
            .entries
            .iter()
            .all(|entry| entry.file != "generated/api.generated.ts"),
        "Expected linguist-generated file to be filtered via .gitattributes"
    );
}

#[test]
fn test_compute_line_stats_ignores_whitespace_only_lines() {
    let (repo, _lines_file, _alphabet_file) = setup_repo_with_base_commit();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");

    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();

    std::fs::write(repo.path().join("whitespace.txt"), "Seed line\n").unwrap();
    repo.git(&["add", "whitespace.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "whitespace.txt"])
        .expect("Setup checkpoint should succeed");

    let file_path = repo.path().join("whitespace.txt");
    let mut content = std::fs::read_to_string(&file_path).unwrap();
    content.push_str("\n\n   \nVisible line one\n\n\t\nVisible line two\n  \n");
    std::fs::write(&file_path, &content).unwrap();
    repo.git(&["add", "whitespace.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "whitespace.txt"])
        .expect("First checkpoint should succeed");

    let after_add_stats = working_log
        .read_all_checkpoints()
        .expect("Should read checkpoints after addition");
    let after_add_last = after_add_stats
        .last()
        .expect("At least one checkpoint expected")
        .line_stats
        .clone();

    assert_eq!(
        after_add_last.additions, 8,
        "Additions includes empty lines"
    );
    assert_eq!(after_add_last.deletions, 0, "No deletions expected yet");
    assert_eq!(
        after_add_last.additions_sloc, 2,
        "Only visible lines counted"
    );
    assert_eq!(
        after_add_last.deletions_sloc, 0,
        "No deletions expected yet"
    );

    let cleaned_content = std::fs::read_to_string(&file_path).unwrap();
    let cleaned_lines: Vec<&str> = cleaned_content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let cleaned_body = format!("{}\n", cleaned_lines.join("\n"));
    std::fs::write(&file_path, &cleaned_body).unwrap();
    repo.git(&["add", "whitespace.txt"]).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "whitespace.txt"])
        .expect("Second checkpoint should succeed");

    let after_delete_stats = working_log
        .read_all_checkpoints()
        .expect("Should read checkpoints after deletion");
    let latest_stats = after_delete_stats
        .last()
        .expect("At least one checkpoint expected")
        .line_stats
        .clone();

    assert_eq!(
        latest_stats.additions, 0,
        "No additions in cleanup checkpoint"
    );
    assert_eq!(latest_stats.deletions, 6, "Deletions includes empty lines");
    assert_eq!(
        latest_stats.additions_sloc, 0,
        "No additions in cleanup checkpoint"
    );
    assert_eq!(
        latest_stats.deletions_sloc, 0,
        "Whitespace deletions ignored"
    );
}

// ====================================================================
// CRLF / LF normalization tests for compute_file_line_stats
// ====================================================================

#[test]
fn test_compute_file_line_stats_crlf_to_lf_no_changes() {
    // Same content, only line endings differ (CRLF → LF).
    // Stats should show 0 additions and 0 deletions.
    let old = "line1\r\nline2\r\nline3\r\n";
    let new = "line1\nline2\nline3\n";

    let stats = compute_file_line_stats(old, new);

    assert_eq!(
        stats.additions, 0,
        "CRLF→LF with identical content should show 0 additions"
    );
    assert_eq!(
        stats.deletions, 0,
        "CRLF→LF with identical content should show 0 deletions"
    );
}

#[test]
fn test_compute_file_line_stats_lf_to_crlf_no_changes() {
    let old = "line1\nline2\nline3\n";
    let new = "line1\r\nline2\r\nline3\r\n";

    let stats = compute_file_line_stats(old, new);

    assert_eq!(
        stats.additions, 0,
        "LF→CRLF with identical content should show 0 additions"
    );
    assert_eq!(
        stats.deletions, 0,
        "LF→CRLF with identical content should show 0 deletions"
    );
}

#[test]
fn test_compute_file_line_stats_crlf_to_lf_with_additions() {
    // Reproduces the user-reported bug: file with CRLF, AI adds lines with LF.
    // Old: 3 CRLF lines. New: same 3 lines (LF) + 2 new lines.
    // Should show exactly 2 additions and 0 deletions.
    let old = "line1\r\nline2\r\nline3\r\n";
    let new = "line1\nline2\nline3\nnew_a\nnew_b\n";

    let stats = compute_file_line_stats(old, new);

    assert_eq!(
        stats.additions, 2,
        "Should have exactly 2 additions (the new lines)"
    );
    assert_eq!(
        stats.deletions, 0,
        "Should have 0 deletions (no lines removed)"
    );
}

#[test]
fn test_compute_file_line_stats_crlf_large_file_user_reported_bug() {
    // Exact scenario from user report:
    // 100-line CRLF file, AI adds 5 lines (with LF).
    // Should show +5 -0, NOT +105 -100.
    let mut old = String::new();
    for i in 1..=100 {
        old.push_str(&format!("line number {}\r\n", i));
    }

    let mut new = String::new();
    for i in 1..=100 {
        new.push_str(&format!("line number {}\n", i));
    }
    for i in 1..=5 {
        new.push_str(&format!("new ai line {}\n", i));
    }

    let stats = compute_file_line_stats(&old, &new);

    assert_eq!(
        stats.additions, 5,
        "Should have exactly 5 additions (AI-added lines), not {}",
        stats.additions
    );
    assert_eq!(
        stats.deletions, 0,
        "Should have 0 deletions, not {}",
        stats.deletions
    );
}

// ====================================================================
// End-to-end CRLF test: blob has CRLF, working tree has LF
// Simulates the real-world scenario where git stores CRLF (or autocrlf
// converts on checkout) and an AI tool writes LF.
// ====================================================================

#[test]
fn test_checkpoint_crlf_blob_vs_lf_working_tree_stats_not_inflated() {
    // Step 1: Create a repo and commit a file with CRLF line endings.
    // On Linux without autocrlf, the blob stores CRLF verbatim.
    let repo = TestRepo::new();
    let crlf_content = "line1\r\nline2\r\nline3\r\nline4\r\nline5\r\n";
    std::fs::write(repo.path().join("test.txt"), crlf_content).unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("initial commit with CRLF")
        .unwrap();

    // Step 2: Overwrite the file with LF endings + one new line,
    // simulating an AI tool that writes LF on a Windows repo.
    let lf_content_with_addition = "line1\nline2\nline3\nline4\nline5\nnew_ai_line\n";
    std::fs::write(repo.path().join("test.txt"), lf_content_with_addition).unwrap();

    // Step 3: Run a checkpoint
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    // Step 4: Read back checkpoint stats
    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    let latest = checkpoints
        .last()
        .expect("Should have at least one checkpoint");

    // The key assertion: stats should reflect only the actual addition,
    // NOT inflate every line because of CRLF→LF conversion.
    assert_eq!(
        latest.line_stats.additions, 1,
        "Should have 1 addition (the new AI line), not {} (which would mean CRLF→LF inflated the count)",
        latest.line_stats.additions
    );
    assert_eq!(
        latest.line_stats.deletions, 0,
        "Should have 0 deletions, not {} (which would mean CRLF→LF caused all old lines to appear deleted)",
        latest.line_stats.deletions
    );
}

#[test]
fn test_checkpoint_crlf_blob_vs_lf_working_tree_no_changes_skipped() {
    // When the only difference is CRLF→LF (no actual content change),
    // the checkpoint should skip the file entirely — normalized comparison
    // detects they're equal and returns None.
    let repo = TestRepo::new();
    let crlf_content = "line1\r\nline2\r\nline3\r\n";
    std::fs::write(repo.path().join("test.txt"), crlf_content).unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("initial commit with CRLF")
        .unwrap();

    // Overwrite with LF-only — same text content, different line endings
    let lf_content = "line1\nline2\nline3\n";
    std::fs::write(repo.path().join("test.txt"), lf_content).unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();

    // The checkpoint may be empty (no entries) or absent entirely,
    // because normalized comparison correctly detected no real change.
    if let Some(latest) = checkpoints.last() {
        let test_entry = latest.entries.iter().find(|e| e.file == "test.txt");
        assert!(
            test_entry.is_none(),
            "test.txt should be skipped when only line endings differ"
        );
    }
    // If no checkpoints at all, that's also correct — nothing changed.
}

#[test]
fn test_checkpoint_stale_crlf_blob_causes_ai_reattribution() {
    // Regression test for Devin review finding: when a CRLF-only change is
    // skipped (preserving a stale CRLF blob), the NEXT AI checkpoint compares
    // the stale CRLF blob against the LF working tree. Because
    // capture_diff_slices sees "line\r\n" ≠ "line\n", ALL lines appear changed.
    // With force_split=true in AI checkpoints, every "changed" line gets
    // re-attributed to AI — even human-written lines.
    //
    // The fix: when content differs only in line endings, update the blob
    // to LF (preserving attributions) so future diffs are LF-vs-LF.
    let repo = TestRepo::new();
    let crlf_initial = "human_line1\r\nhuman_line2\r\nhuman_line3\r\n";
    std::fs::write(repo.path().join("test.txt"), crlf_initial).unwrap();
    repo.git(&["add", "test.txt"]).unwrap();
    repo.stage_all_and_commit("initial commit with CRLF")
        .unwrap();

    // Step 1: Human checkpoint on CRLF file → creates entry with CRLF blob
    // (need to add a line so the checkpoint creates an entry)
    let crlf_with_edit = "human_line1\r\nhuman_line2\r\nhuman_line3\r\nhuman_line4\r\n";
    std::fs::write(repo.path().join("test.txt"), crlf_with_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    // Step 2: Convert file to LF (same content, only line endings change)
    let lf_with_edit = "human_line1\nhuman_line2\nhuman_line3\nhuman_line4\n";
    std::fs::write(repo.path().join("test.txt"), lf_with_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    // Step 3: AI adds one line (LF) → AI checkpoint
    let lf_with_ai = "human_line1\nhuman_line2\nhuman_line3\nhuman_line4\nai_new_line\n";
    std::fs::write(repo.path().join("test.txt"), lf_with_ai).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();

    // Read the AI checkpoint
    let gitai_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("Repository should exist");
    let base_commit = gitai_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();

    // Find the AI checkpoint entry for test.txt
    let ai_checkpoint = checkpoints
        .iter()
        .rev()
        .find(|cp| cp.kind.is_ai() && cp.entries.iter().any(|e| e.file == "test.txt"))
        .expect("Should have an AI checkpoint with test.txt");
    let test_entry = ai_checkpoint
        .entries
        .iter()
        .find(|e| e.file == "test.txt")
        .unwrap();

    // The key assertion: the AI checkpoint should NOT attribute all lines to AI.
    // Only the actually-added line should be AI-attributed.
    let ai_line_attrs: Vec<_> = test_entry
        .line_attributions
        .iter()
        .filter(|la| is_ai_author_id(&la.author_id))
        .collect();

    // Count total lines covered by AI attributions
    let ai_line_count: u32 = ai_line_attrs
        .iter()
        .map(|la| la.end_line - la.start_line + 1)
        .sum();

    // AI should only attribute 1 line (the new ai_new_line), not all 5 lines.
    // If the stale CRLF blob caused full re-attribution, ai_line_count would be 5.
    assert!(
        ai_line_count <= 2,
        "AI should attribute at most 1-2 lines (the actual addition), \
         but attributed {} lines — stale CRLF blob caused full re-attribution. \
         AI attributions: {:?}, all attributions: {:?}",
        ai_line_count,
        ai_line_attrs,
        test_entry.line_attributions
    );
}

/// Regression test: INITIAL attributions without stored file_blobs are invalid.
/// Line attributions are only meaningful relative to the exact file snapshot
/// they describe, so checkpointing must fail loudly instead of guessing from
/// dirty-file content.
#[test]
fn test_checkpoint_fails_with_initial_missing_blobs() {
    let repo = TestRepo::new();
    let file_a = repo.path().join("file_a.txt");
    let file_b = repo.path().join("file_b.txt");

    // Create both files and commit
    std::fs::write(&file_a, "line1\nline2\n").unwrap();
    std::fs::write(&file_b, "hello\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_a.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "file_b.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial commit").unwrap();

    // Edit BOTH files and commit (so both end up in INITIAL after reset)
    std::fs::write(&file_a, "line1\nline2\nai on a\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_a.txt"])
        .unwrap();
    std::fs::write(&file_b, "hello\nai added\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_b.txt"])
        .unwrap();
    repo.stage_all_and_commit("second commit with AI on both files")
        .unwrap();

    repo.git(&["reset", "--soft", "HEAD~1"]).unwrap();
    repo.sync_daemon_force();

    // Strip file_blobs from INITIAL to simulate legacy data (pre-March-2026)
    let git_ai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let head_sha = git_ai_repo.head().unwrap().target().unwrap();
    let working_log = git_ai_repo
        .storage
        .working_log_for_base_commit(&head_sha)
        .unwrap();
    let initial = working_log.read_initial_attributions();
    assert!(
        initial.files.contains_key("file_a.txt"),
        "INITIAL must contain file_a.txt for this test"
    );
    assert!(
        initial.files.contains_key("file_b.txt"),
        "INITIAL must contain file_b.txt for this test"
    );

    let mut legacy_initial = initial.clone();
    legacy_initial.file_blobs.clear();
    let json = serde_json::to_string(&legacy_initial).unwrap();
    std::fs::write(&working_log.initial_file, &json).unwrap();

    // Directly invoke checkpoint daemon logic on file_b only. The no-blob INITIAL
    // state is invalid even though dirty_files contains file_b, because INITIAL
    // also references file_a and neither attribution can be resolved against the
    // exact snapshot it describes.
    let mut dirty_files = HashMap::new();
    dirty_files.insert(
        "file_b.txt".to_string(),
        Arc::from("hello\nai added\nnew line\n"),
    );

    let resolved = ResolvedCheckpointExecution {
        base_commit: head_sha.clone(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        files: vec!["file_b.txt".to_string()],
        dirty_files,
    };

    let checkpoint_request = CheckpointRequest {
        trace_id: "test-trace".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: Some(AgentId {
            tool: "test".to_string(),
            id: "test-id".to_string(),
            model: "test-model".to_string(),
        }),
        files: vec![CheckpointFile {
            path: file_b.clone(),
            content: Some("hello\nai added\nnew line\n".to_string()),
            repo_work_dir: repo.path().to_path_buf(),
            base_commit: BaseCommit::Sha(head_sha),
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };

    let result = execute_resolved_checkpoint_from_daemon(
        &git_ai_repo,
        "test",
        CheckpointKind::AiAgent,
        checkpoint_request,
        resolved,
    );
    let error = result.expect_err("checkpoint should reject INITIAL without persisted blobs");
    assert!(
        error
            .to_string()
            .contains("INITIAL missing persisted file snapshot"),
        "unexpected error: {error}"
    );
}

/// When an AI agent deletes a file, the checkpoint should still be recorded (not silently
/// dropped). The scoped post-edit checkpoint fires with the deleted file's path — the file
/// no longer exists on disk, so the orchestrator must set content = Some("") and pass it
/// through to the daemon so the deletion is tracked in the working log.
#[test]
fn test_scoped_checkpoint_records_file_deletion() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("to_delete.txt");

    // Create file and commit with known human attribution
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "to_delete.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial commit").unwrap();

    // AI agent pre-edit snapshot (captures before state)
    repo.git_ai(&["checkpoint", "human", "to_delete.txt"])
        .unwrap();

    // AI deletes the file
    std::fs::remove_file(&file_path).unwrap();

    // AI agent post-edit checkpoint on the now-deleted file
    let checkpoint_result = repo.git_ai(&["checkpoint", "mock_ai", "to_delete.txt"]);
    assert!(
        checkpoint_result.is_ok(),
        "Checkpoint on deleted file should succeed, got: {:?}",
        checkpoint_result.err()
    );

    // Verify the checkpoint was recorded in the working log with deletion stats
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let working_log = gitai_repo
        .storage
        .working_log_for_base_commit(&head_sha)
        .unwrap();
    let checkpoints = working_log.read_all_checkpoints().unwrap();

    // The AI post-edit checkpoint should be recorded (human pre-edit is a no-op since
    // the file hadn't changed relative to HEAD at that point)
    assert!(
        !checkpoints.is_empty(),
        "At least one checkpoint should be recorded for the deletion"
    );

    // The AI checkpoint should reference to_delete.txt and record 3 deleted lines
    let ai_checkpoint = checkpoints
        .iter()
        .find(|cp| cp.kind.is_ai())
        .expect("Should have an AI checkpoint");
    assert!(
        ai_checkpoint.kind.is_ai(),
        "Last checkpoint should be AI, got {:?}",
        ai_checkpoint.kind
    );
    let has_file = ai_checkpoint
        .entries
        .iter()
        .any(|e| e.file == "to_delete.txt");
    assert!(
        has_file,
        "AI checkpoint should reference to_delete.txt, entries: {:?}",
        ai_checkpoint
            .entries
            .iter()
            .map(|e| &e.file)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ai_checkpoint.line_stats.deletions, 3,
        "AI checkpoint should record 3 deleted lines, got {}",
        ai_checkpoint.line_stats.deletions
    );
}
