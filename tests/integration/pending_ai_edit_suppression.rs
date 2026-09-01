use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;
use std::fs;

/// Uses the agent-v1 preset to fire a generic PreFileEdit (with agent_id),
/// which registers pending AI edit state. This is agent-agnostic: it tests
/// the daemon's suppression logic independent of any specific agent preset.
fn fire_pre_edit_checkpoint(repo: &TestRepo, file_paths: &[&str]) {
    let abs_paths: Vec<String> = file_paths
        .iter()
        .map(|p| repo.path().join(p).to_string_lossy().to_string())
        .collect();
    fire_pre_edit_checkpoint_abs(repo, &abs_paths);
}

fn fire_pre_edit_checkpoint_abs(repo: &TestRepo, abs_paths: &[String]) {
    let payload = json!({
        "type": "human",
        "repo_working_dir": repo.path().to_string_lossy().to_string(),
        "will_edit_filepaths": abs_paths,
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &payload])
        .unwrap();
}

fn fire_post_edit_checkpoint(repo: &TestRepo, file_paths: &[&str]) {
    let mut args: Vec<&str> = vec!["checkpoint", "mock_ai"];
    for p in file_paths {
        args.push(p);
    }
    repo.git_ai(&args).unwrap();
}

/// Core race condition test: a KnownHuman checkpoint arriving between
/// pre-edit and post-edit AI checkpoints should be suppressed.
#[test]
fn test_known_human_suppressed_between_ai_pre_and_post_edit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("target.txt");

    fs::write(&file_path, "original\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI agent fires pre-edit checkpoint (registers pending state)
    fire_pre_edit_checkpoint(&repo, &["target.txt"]);

    // AI edits the file
    fs::write(&file_path, "original\nai added\n").unwrap();

    // IDE fires KnownHuman (spurious save event) — should be suppressed
    repo.git_ai(&["checkpoint", "mock_known_human", "target.txt"])
        .unwrap();

    // AI agent fires post-edit checkpoint (clears pending state)
    fire_post_edit_checkpoint(&repo, &["target.txt"]);

    repo.stage_all_and_commit("AI edit with race").unwrap();
    let mut file = repo.filename("target.txt");
    file.assert_committed_lines(lines!["original".unattributed_human(), "ai added".ai(),]);
}

/// Verifies that KnownHuman checkpoints are NOT suppressed when there is
/// no pending AI edit — genuine human edits must be attributed correctly.
#[test]
fn test_known_human_not_suppressed_without_pending_ai_edit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("genuine.txt");

    fs::write(&file_path, "line one\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Human edits without any AI pre-edit checkpoint
    fs::write(&file_path, "line one\nhuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "genuine.txt"])
        .unwrap();

    repo.stage_all_and_commit("Human edit").unwrap();
    let mut file = repo.filename("genuine.txt");
    file.assert_committed_lines(lines![
        "line one".unattributed_human(),
        "human line".human(),
    ]);
}

/// After a full AI edit cycle completes (pre + post), subsequent KnownHuman
/// checkpoints on the same file must NOT be suppressed.
#[test]
fn test_known_human_works_after_ai_edit_cycle_completes() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("cycle.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Full AI edit cycle
    fire_pre_edit_checkpoint(&repo, &["cycle.txt"]);
    fs::write(&file_path, "base\nai line\n").unwrap();
    fire_post_edit_checkpoint(&repo, &["cycle.txt"]);

    // Now a genuine human edit — pending state should be cleared
    fs::write(&file_path, "base\nai line\nhuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "cycle.txt"])
        .unwrap();

    repo.stage_all_and_commit("Mixed edit").unwrap();
    let mut file = repo.filename("cycle.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "ai line".ai(),
        "human line".human(),
    ]);
}

/// Multiple files: only the file with a pending AI edit should have its
/// KnownHuman suppressed; other files should still get KnownHuman attribution.
#[test]
fn test_suppression_is_per_file_not_global() {
    let repo = TestRepo::new();
    let ai_file = repo.path().join("ai_target.txt");
    let human_file = repo.path().join("human_target.txt");

    fs::write(&ai_file, "ai base\n").unwrap();
    fs::write(&human_file, "human base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Only ai_target.txt gets a pre-edit checkpoint
    fire_pre_edit_checkpoint(&repo, &["ai_target.txt"]);

    // Both files are edited
    fs::write(&ai_file, "ai base\nai new line\n").unwrap();
    fs::write(&human_file, "human base\nhuman new line\n").unwrap();

    // KnownHuman fired for both files
    repo.git_ai(&["checkpoint", "mock_known_human", "ai_target.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "human_target.txt"])
        .unwrap();

    // AI post-edit only for ai_target.txt
    fire_post_edit_checkpoint(&repo, &["ai_target.txt"]);

    repo.stage_all_and_commit("Mixed per-file edit").unwrap();

    let mut ai_f = repo.filename("ai_target.txt");
    ai_f.assert_committed_lines(lines!["ai base".unattributed_human(), "ai new line".ai(),]);

    let mut human_f = repo.filename("human_target.txt");
    human_f.assert_committed_lines(lines![
        "human base".unattributed_human(),
        "human new line".human(),
    ]);
}

/// The untracked `human` preset (legacy) should NOT register pending state.
/// A KnownHuman after an untracked checkpoint should still work.
#[test]
fn test_untracked_human_checkpoint_does_not_suppress_known_human() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("untracked.txt");

    fs::write(&file_path, "first\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Untracked checkpoint (no agent_id, should NOT register pending state)
    fs::write(&file_path, "first\nsecond\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "untracked.txt"])
        .unwrap();

    // KnownHuman should NOT be suppressed
    fs::write(&file_path, "first\nsecond\nthird\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "untracked.txt"])
        .unwrap();

    repo.stage_all_and_commit("Untracked then human").unwrap();
    let mut file = repo.filename("untracked.txt");
    file.assert_committed_lines(lines![
        "first".unattributed_human(),
        "second".unattributed_human(),
        "third".human(),
    ]);
}

/// Multiple sequential AI edit cycles on the same file should each work
/// correctly with suppression during each cycle and normal behavior between.
#[test]
fn test_multiple_ai_edit_cycles_on_same_file() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("multi.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // First AI edit cycle
    fire_pre_edit_checkpoint(&repo, &["multi.txt"]);
    fs::write(&file_path, "base\nai first\n").unwrap();
    // KnownHuman during first cycle — should be suppressed
    repo.git_ai(&["checkpoint", "mock_known_human", "multi.txt"])
        .unwrap();
    fire_post_edit_checkpoint(&repo, &["multi.txt"]);

    // Human edit between cycles — should NOT be suppressed
    fs::write(&file_path, "base\nai first\nhuman middle\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "multi.txt"])
        .unwrap();

    // Second AI edit cycle
    fire_pre_edit_checkpoint(&repo, &["multi.txt"]);
    fs::write(&file_path, "base\nai first\nhuman middle\nai second\n").unwrap();
    // KnownHuman during second cycle — should be suppressed
    repo.git_ai(&["checkpoint", "mock_known_human", "multi.txt"])
        .unwrap();
    fire_post_edit_checkpoint(&repo, &["multi.txt"]);

    repo.stage_all_and_commit("Multi-cycle edit").unwrap();
    let mut file = repo.filename("multi.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "ai first".ai(),
        "human middle".human(),
        "ai second".ai(),
    ]);
}

/// When a pre-edit is fired for multiple files at once, KnownHuman should
/// be suppressed for all of them.
#[test]
fn test_multi_file_pre_edit_suppresses_all_files() {
    let repo = TestRepo::new();
    let file_a = repo.path().join("a.txt");
    let file_b = repo.path().join("b.txt");

    fs::write(&file_a, "a base\n").unwrap();
    fs::write(&file_b, "b base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Pre-edit for both files at once
    fire_pre_edit_checkpoint(&repo, &["a.txt", "b.txt"]);

    // Both files edited
    fs::write(&file_a, "a base\na ai line\n").unwrap();
    fs::write(&file_b, "b base\nb ai line\n").unwrap();

    // KnownHuman for both — should be suppressed
    repo.git_ai(&["checkpoint", "mock_known_human", "a.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "b.txt"])
        .unwrap();

    // Post-edit for both
    fire_post_edit_checkpoint(&repo, &["a.txt", "b.txt"]);

    repo.stage_all_and_commit("Multi-file AI edit").unwrap();

    let mut fa = repo.filename("a.txt");
    fa.assert_committed_lines(lines!["a base".unattributed_human(), "a ai line".ai(),]);

    let mut fb = repo.filename("b.txt");
    fb.assert_committed_lines(lines!["b base".unattributed_human(), "b ai line".ai(),]);
}

/// A single multi-file KnownHuman checkpoint that bundles files where only
/// SOME have pending AI edits. The pending-AI files should be filtered out
/// (suppressed) while the non-pending files still get KnownHuman attribution.
/// Uses stats --json to verify human_additions (KnownHuman) vs unknown_additions.
#[test]
fn test_multi_file_known_human_partial_suppression() {
    let repo = TestRepo::new();
    let ai_file = repo.path().join("ai_file.txt");
    let human_file = repo.path().join("human_file.txt");

    fs::write(&ai_file, "ai base\n").unwrap();
    fs::write(&human_file, "human base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Only ai_file.txt gets a pre-edit checkpoint (pending AI edit)
    fire_pre_edit_checkpoint(&repo, &["ai_file.txt"]);

    // Both files are edited
    fs::write(&ai_file, "ai base\nai new line\n").unwrap();
    fs::write(&human_file, "human base\nhuman new line\n").unwrap();

    // Single KnownHuman checkpoint for BOTH files at once
    repo.git_ai(&[
        "checkpoint",
        "mock_known_human",
        "ai_file.txt",
        "human_file.txt",
    ])
    .unwrap();

    // AI post-edit only for ai_file.txt
    fire_post_edit_checkpoint(&repo, &["ai_file.txt"]);

    repo.stage_all_and_commit("Mixed multi-file checkpoint")
        .unwrap();

    // ai_file.txt: KnownHuman was suppressed for this file, AI post-edit attributed it
    let mut ai_f = repo.filename("ai_file.txt");
    ai_f.assert_committed_lines(lines!["ai base".unattributed_human(), "ai new line".ai(),]);

    // Verify via stats that human_file.txt's line is KnownHuman (not unattributed).
    // human_additions counts lines with KnownHuman attestation;
    // unknown_additions counts unattributed lines.
    let raw = repo
        .git_ai(&["stats", "--json"])
        .expect("stats should succeed");
    let json_start = raw.find('{').unwrap_or(0);
    let json_end = raw.rfind('}').unwrap_or(raw.len().saturating_sub(1));
    let stats: serde_json::Value =
        serde_json::from_str(&raw[json_start..=json_end]).expect("valid stats json");

    assert_eq!(
        stats["human_additions"],
        1,
        "human_file.txt's new line should be KnownHuman-attributed (not suppressed). Stats: {}",
        serde_json::to_string_pretty(&stats).unwrap()
    );
}

/// Regression for #2221: agent creates a *new* file under a symlinked path.
/// Pre-edit registers pending state while the file does not exist yet
/// (`canonicalize` fails); after the write, KnownHuman's path resolves through
/// the symlink to a different string. Suppression must still match so the AI
/// post-edit owns the lines instead of KnownHuman.
#[cfg(unix)]
#[test]
fn test_known_human_suppressed_for_new_file_under_symlinked_path() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("README.md"), "# init\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let real_dir = repo.path().join("realdir");
    fs::create_dir(&real_dir).unwrap();
    let link_dir = repo.path().join("linkdir");
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    let via_link = link_dir.join("created.txt");
    assert!(
        !via_link.exists(),
        "file must not exist yet when Pre registers pending AI edit"
    );

    // Pre through the symlink path — canonicalize fails, historically stored the raw path.
    fire_pre_edit_checkpoint_abs(&repo, &[via_link.to_string_lossy().to_string()]);

    // Agent writes the new file (content visible via both link and real path).
    fs::write(real_dir.join("created.txt"), "ai created\n").unwrap();

    // Spurious IDE save after the write: path now canonicalizes through the symlink.
    repo.git_ai(&[
        "checkpoint",
        "mock_known_human",
        via_link.to_str().expect("utf-8 path"),
    ])
    .unwrap();

    fire_post_edit_checkpoint(&repo, &[via_link.to_str().expect("utf-8 path")]);

    repo.git(&["add", "realdir/created.txt"]).unwrap();
    repo.commit("AI created new file").unwrap();

    let mut file = repo.filename("realdir/created.txt");
    file.assert_committed_lines(lines!["ai created".ai()]);
}
