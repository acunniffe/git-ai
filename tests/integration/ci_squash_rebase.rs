use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::git::notes_api::write_note;
use git_ai::git::repository as GitAiRepository;

fn direct_test_repo() -> TestRepo {
    TestRepo::new()
}

fn run_ci_local_merge(repo: &TestRepo, merge_sha: &str, head_sha: &str, base_sha: &str) -> String {
    repo.git_ai(&[
        "ci",
        "local",
        "merge",
        "--merge-commit-sha",
        merge_sha,
        "--base-ref",
        "main",
        "--head-ref",
        "feature",
        "--head-sha",
        head_sha,
        "--base-sha",
        base_sha,
        "--skip-fetch",
        "--skip-push",
    ])
    .expect("ci local merge should succeed")
}

fn assert_ci_rewrite_succeeded(output: &str) {
    assert!(
        output.contains("authorship rewritten successfully"),
        "expected ci local merge to rewrite authorship, got: {output}"
    );
}

fn authorship_files(repo: &TestRepo, commit_sha: &str) -> Vec<String> {
    let note = repo
        .read_authorship_note(commit_sha)
        .unwrap_or_else(|| panic!("expected authorship note for {commit_sha}"));
    AuthorshipLog::deserialize_from_string(&note)
        .expect("authorship note should deserialize")
        .attestations
        .iter()
        .map(|attestation| attestation.file_path.clone())
        .collect()
}

fn setup_main(repo: &TestRepo) -> String {
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base"]);
    let base_sha = repo.stage_all_and_commit("base").unwrap().commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();
    base_sha
}

fn squash_feature_with_raw_git(repo: &TestRepo, message: &str) -> String {
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&["commit", "-m", message]).unwrap();
    repo.git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn test_ci_squash_merge_basic() {
    let repo = TestRepo::new();
    let base_sha = setup_main(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut feature = repo.filename("feature.js");
    feature.set_contents(crate::lines![
        "export function aiFeature() {".ai(),
        "  return 'ai code';".ai(),
        "}".ai()
    ]);
    let head_sha = repo
        .stage_all_and_commit("add ai feature")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "squash feature");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    feature.assert_lines_and_blame(crate::lines![
        "export function aiFeature() {".ai(),
        "  return 'ai code';".ai(),
        "}".ai()
    ]);
}

#[test]
fn test_ci_squash_merge_multiple_files() {
    let repo = TestRepo::new();
    let base_sha = setup_main(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut api = repo.filename("api.js");
    let mut view = repo.filename("view.js");
    api.set_contents(crate::lines![
        "export const handler = () => {".ai(),
        "  return 'ok';".ai(),
        "};".ai()
    ]);
    view.set_contents(crate::lines![
        "export function View() {".ai(),
        "  return handler();".ai(),
        "}".ai()
    ]);
    let head_sha = repo
        .stage_all_and_commit("add ai feature files")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "squash feature files");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    api.assert_lines_and_blame(crate::lines![
        "export const handler = () => {".ai(),
        "  return 'ok';".ai(),
        "};".ai()
    ]);
    view.assert_lines_and_blame(crate::lines![
        "export function View() {".ai(),
        "  return handler();".ai(),
        "}".ai()
    ]);
}

#[test]
fn test_ci_squash_merge_mixed_ai_and_human_content() {
    let repo = TestRepo::new();
    let base_sha = setup_main(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut mixed = repo.filename("mixed.js");
    mixed.set_contents(crate::lines![
        "// Human-written setup",
        "const flag = true;",
        "// AI generated helper".ai(),
        "function helper() {".ai(),
        "  return flag;".ai(),
        "}".ai(),
        "// Human-written footer"
    ]);
    let head_sha = repo
        .stage_all_and_commit("add mixed feature")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "squash mixed feature");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    mixed.assert_lines_and_blame(crate::lines![
        "// Human-written setup".human(),
        "const flag = true;".human(),
        "// AI generated helper".ai(),
        "function helper() {".ai(),
        "  return flag;".ai(),
        "}".ai(),
        "// Human-written footer".human()
    ]);
}

#[test]
fn test_ci_squash_merge_no_notes_no_authorship_created() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("feature.txt");
    std::fs::write(&file_path, "base\n").unwrap();
    repo.git_og(&["add", "feature.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "base"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&["branch", "-M", "main"]).unwrap();

    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    std::fs::write(&file_path, "base\nhuman change\n").unwrap();
    repo.git_og(&["commit", "-am", "human feature"]).unwrap();
    let head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let merge_sha = squash_feature_with_raw_git(&repo, "squash human feature");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);

    assert!(
        output.contains("no AI authorship to track"),
        "expected ci local merge to report no authorship, got: {output}"
    );
    assert!(
        repo.read_authorship_note(&merge_sha).is_none(),
        "expected no authorship note when source commits have no notes"
    );
}

#[test]
fn test_ci_rebase_merge_commit_order_pairing() {
    let repo = TestRepo::new();
    let base_sha = setup_main(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["ai content in file_a".ai()]);
    let feature_sha1 = repo.stage_all_and_commit("add file_a").unwrap().commit_sha;

    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["ai content in file_b".ai()]);
    let feature_sha2 = repo.stage_all_and_commit("add file_b").unwrap().commit_sha;

    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_only = repo.filename("main_only.txt");
    main_only.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "advance main"]).unwrap();

    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();
    let new_sha2 = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha1 = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(new_sha1, feature_sha1);
    assert_ne!(new_sha2, feature_sha2);

    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();

    let output = run_ci_local_merge(&repo, &new_sha2, &feature_sha2, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    let files1 = authorship_files(&repo, &new_sha1);
    let files2 = authorship_files(&repo, &new_sha2);

    assert!(
        files1.iter().any(|file| file.contains("file_a")),
        "rebased commit 1 should reference file_a.txt, got: {files1:?}"
    );
    assert!(
        !files1.iter().any(|file| file.contains("file_b")),
        "rebased commit 1 should not reference file_b.txt, got: {files1:?}"
    );
    assert!(
        files2.iter().any(|file| file.contains("file_b")),
        "rebased commit 2 should reference file_b.txt, got: {files2:?}"
    );
    assert!(
        !files2.iter().any(|file| file.contains("file_a")),
        "rebased commit 2 should not reference file_a.txt, got: {files2:?}"
    );
}

#[test]
fn test_ci_local_sync_skips_when_current_rebased_commit_already_has_note() {
    let repo = direct_test_repo();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["ai content".ai()]);
    let previous_head_sha = repo.stage_all_and_commit("Add feature").unwrap().commit_sha;

    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();
    let current_head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let gitai_repo =
        GitAiRepository::find_repository_in_path(repo.path().to_str().expect("repo path"))
            .expect("git-ai repo");
    let existing_note = "client-side-note-that-ci-must-not-overwrite";
    write_note(&gitai_repo, &current_head_sha, existing_note).expect("add existing current note");

    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "sync",
            "--previous-head-sha",
            previous_head_sha.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--head-sha",
            current_head_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-push",
        ])
        .expect("ci local sync should succeed");

    assert!(
        output.contains("Local CI (sync): skipped PR sync with existing authorship"),
        "Expected existing-note skip, got: {}",
        output
    );
    let current_note = repo
        .read_authorship_note(&current_head_sha)
        .map(|note| note.trim().to_string());
    assert_eq!(
        current_note.as_deref(),
        Some(existing_note),
        "CI sync must not overwrite a current commit note that already exists"
    );
}

#[test]
fn test_ci_local_sync_skips_non_rebase_force_push() {
    let repo = direct_test_repo();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["old ai content".ai()]);
    let previous_head_sha = repo
        .stage_all_and_commit("Add old AI content")
        .unwrap()
        .commit_sha;
    assert!(
        repo.read_authorship_note(&previous_head_sha).is_some(),
        "old PR head should have an authorship note"
    );

    repo.git_og(&["reset", "--hard", "main"]).unwrap();
    feature_file.set_contents(crate::lines!["different force-pushed content"]);
    repo.git_og(&["add", "feature.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Force-pushed replacement"])
        .unwrap();
    let current_head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "sync",
            "--previous-head-sha",
            previous_head_sha.as_str(),
            "--base-ref",
            "main",
            "--head-sha",
            current_head_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-fetch-sync-refs",
            "--skip-push",
        ])
        .expect("ci local sync should succeed for non-rebase force push");

    assert!(
        output.contains("Local CI (sync): skipped non-rebase PR sync"),
        "Expected non-rebase sync skip, got: {}",
        output
    );
    assert!(
        repo.read_authorship_note(&current_head_sha).is_none(),
        "non-rebase sync must not transfer old authorship to unrelated replacement commit"
    );
}

#[test]
fn test_ci_local_open_pr_rebase_single_commit() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

    let repo = direct_test_repo();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(crate::lines!["ai content".ai()]);
    let previous_head_sha = repo.stage_all_and_commit("Add feature").unwrap().commit_sha;

    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();
    let current_head_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(current_head_sha, previous_head_sha);
    assert!(
        repo.read_authorship_note(&current_head_sha).is_none(),
        "bypassed rebase should not pre-create note for the rebased commit"
    );

    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "sync",
            "--previous-head-sha",
            previous_head_sha.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--head-sha",
            current_head_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-push",
        ])
        .expect("ci local sync should succeed");

    assert!(
        output.contains("Local CI (sync): authorship rewritten successfully"),
        "Expected authorship rewritten, got: {}",
        output
    );

    let note = repo
        .read_authorship_note(&current_head_sha)
        .expect("rebased single PR commit should have an authorship note");
    let files: Vec<String> = AuthorshipLog::deserialize_from_string(&note)
        .unwrap()
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect();
    assert!(
        files.iter().any(|f| f.contains("feature.txt")),
        "rebased single PR commit should reference feature.txt, got: {:?}",
        files
    );
}

#[test]
fn test_ci_local_open_pr_rebase_two_commits() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

    let repo = direct_test_repo();

    // --- Initial commit on main ---
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();

    // --- Feature branch: two AI commits touching distinct files ---
    repo.git_og(&["checkout", "-b", "feature"]).unwrap();

    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["ai content in file_a".ai()]);
    let feature_sha1 = repo.stage_all_and_commit("Add file_a").unwrap().commit_sha;

    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["ai content in file_b".ai()]);
    let feature_sha2 = repo.stage_all_and_commit("Add file_b").unwrap().commit_sha;

    let previous_head_sha = feature_sha2.clone();

    // --- Advance main so the open-PR rebase produces new SHAs ---
    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // --- Rebase the open feature branch onto main, bypassing local hooks ---
    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();

    let new_sha2 = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha1 = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(
        new_sha1, feature_sha1,
        "open-PR rebase must produce a new SHA for commit 1"
    );
    assert_ne!(
        new_sha2, feature_sha2,
        "open-PR rebase must produce a new SHA for commit 2"
    );
    assert!(
        repo.read_authorship_note(&new_sha1).is_none(),
        "bypassed rebase should not pre-create note for commit 1"
    );
    assert!(
        repo.read_authorship_note(&new_sha2).is_none(),
        "bypassed rebase should not pre-create note for commit 2"
    );

    // --- Run the new open-PR sync command ---
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "sync",
            "--previous-head-sha",
            previous_head_sha.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--head-sha",
            new_sha2.as_str(),
            "--skip-fetch-notes",
            "--skip-push",
        ])
        .expect("ci local sync should succeed");

    assert!(
        output.contains("Local CI (sync): authorship rewritten successfully"),
        "Expected authorship rewritten, got: {}",
        output
    );

    // --- Verify each rebased open-PR commit carries notes for its own file ---
    let note1 = repo
        .read_authorship_note(&new_sha1)
        .expect("rebased PR commit 1 should have an authorship note");
    let note2 = repo
        .read_authorship_note(&new_sha2)
        .expect("rebased PR commit 2 should have an authorship note");

    let files1: Vec<String> = AuthorshipLog::deserialize_from_string(&note1)
        .unwrap()
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect();
    let files2: Vec<String> = AuthorshipLog::deserialize_from_string(&note2)
        .unwrap()
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect();

    assert!(
        files1.iter().any(|f| f.contains("file_a")),
        "rebased PR commit 1 should reference file_a.txt, got: {:?}",
        files1
    );
    assert!(
        !files1.iter().any(|f| f.contains("file_b")),
        "rebased PR commit 1 should not reference file_b.txt, got: {:?}",
        files1
    );
    assert!(
        files2.iter().any(|f| f.contains("file_b")),
        "rebased PR commit 2 should reference file_b.txt, got: {:?}",
        files2
    );
    assert!(
        !files2.iter().any(|f| f.contains("file_a")),
        "rebased PR commit 2 should not reference file_a.txt, got: {:?}",
        files2
    );
}

#[test]
fn test_ci_local_merge_squash_on_linear_main_does_not_note_base_commits() {
    let repo = direct_test_repo();
    repo.git_og(&["config", "user.name", "Test User"]).unwrap();
    repo.git_og(&["config", "user.email", "test@example.com"])
        .unwrap();

    // B0: initial commit on main (raw git -> no authorship note)
    std::fs::write(repo.path().join("base.txt"), "base content\n").unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "B0 initial"]).unwrap();
    repo.git_og(&["branch", "-M", "main"]).unwrap();
    let b0_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // B1, B2, B3: teammate commits on main, NOT using the wrapper (no notes)
    for i in 1..=3 {
        std::fs::write(
            repo.path().join(format!("teammate{i}.txt")),
            format!("teammate change {i}\n"),
        )
        .unwrap();
        repo.git_og(&["add", "-A"]).unwrap();
        repo.git_og(&["commit", "-m", &format!("B{i} teammate change")])
            .unwrap();
    }
    let b2_sha = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    let b3_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // feature branch off B0 with 3 AI commits (each gets a note via the wrapper)
    repo.git_og(&["checkout", "-b", "feature", &b0_sha])
        .unwrap();
    let mut feat = repo.filename("feature.txt");
    feat.set_contents(crate::lines!["// P1 ai line".ai()]);
    repo.stage_all_and_commit("P1").unwrap();
    feat.insert_at(1, crate::lines!["// P2 ai line".ai()]);
    repo.stage_all_and_commit("P2").unwrap();
    feat.insert_at(2, crate::lines!["// P3 ai line".ai()]);
    let head_sha = repo.stage_all_and_commit("P3").unwrap().commit_sha;

    // Squash merge: GitHub creates one new commit S on top of B3 (raw git)
    repo.git_og(&["checkout", "main"]).unwrap();
    std::fs::write(
        repo.path().join("feature.txt"),
        "// P1 ai line\n// P2 ai line\n// P3 ai line\n",
    )
    .unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "Squash merge feature (#PR)"])
        .unwrap();
    let squash_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Bare origin so `ci local merge` can push authorship
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_path = origin_dir.path().join("origin.git");
    repo.git_og(&[
        "clone",
        "--bare",
        repo.path().to_str().unwrap(),
        origin_path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "origin", origin_path.to_str().unwrap()])
        .unwrap();

    // Run the real CLI exactly as CI would after a squash merge
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            squash_sha.as_str(),
            "--head-ref",
            "feature",
            "--head-sha",
            head_sha.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            b3_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-fetch-base",
        ])
        .expect("ci local merge should succeed");

    assert!(
        output.contains("authorship rewritten successfully"),
        "expected authorship rewritten, got: {output}"
    );

    // Only the squash commit S carries a note; the base commits are untouched.
    assert!(
        repo.read_authorship_note(&squash_sha).is_some(),
        "squash commit S ({squash_sha}) should receive the rewritten authorship note"
    );
    assert!(
        repo.read_authorship_note(&b2_sha).is_none(),
        "#1473 regression: unrelated base commit B2 ({b2_sha}) must not receive a note"
    );
    assert!(
        repo.read_authorship_note(&b3_sha).is_none(),
        "#1473 regression: unrelated base commit B3 ({b3_sha}) must not receive a note"
    );
}

#[test]
fn test_ci_local_rebase_merge_with_abbreviated_merge_sha() {
    let repo = direct_test_repo();

    // --- Initial commit on main ---
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // --- Feature branch: two commits touching different files ---
    repo.git_og(&["checkout", "-b", "feature"]).unwrap();
    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["ai content in file_a".ai()]);
    let _feature_sha1 = repo.stage_all_and_commit("Add file_a").unwrap().commit_sha;
    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["ai content in file_b".ai()]);
    let feature_sha2 = repo.stage_all_and_commit("Add file_b").unwrap().commit_sha;

    // --- Advance main so the rebase produces new commit SHAs ---
    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();

    // --- Rebase feature onto main (bypassing the local hook), then ff main ---
    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();
    let new_sha2 = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha1 = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();

    // --- Bare origin so push_authorship inside CiContext can succeed ---
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_path = origin_dir.path().join("origin.git");
    repo.git_og(&[
        "clone",
        "--bare",
        repo.path().to_str().unwrap(),
        origin_path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "origin", origin_path.to_str().unwrap()])
        .unwrap();

    // --- Run `ci local merge` with an ABBREVIATED merge-commit-sha ---
    let abbreviated_merge_sha = &new_sha2[..12];
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            abbreviated_merge_sha,
            "--head-ref",
            "feature",
            "--head-sha",
            feature_sha2.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-fetch-base",
        ])
        .expect("ci local merge should succeed");

    assert!(
        output.contains("authorship rewritten successfully"),
        "expected authorship rewritten, got: {output}"
    );

    // --- Each rebased commit must still carry its own note (rebase path kept) ---
    let note1 = repo
        .read_authorship_note(&new_sha1)
        .expect("rebased commit 1 should have a note (rebase must not be misclassified as squash)");
    let note2 = repo
        .read_authorship_note(&new_sha2)
        .expect("rebased commit 2 should have a note");

    let files = |note: &str| -> Vec<String> {
        AuthorshipLog::deserialize_from_string(note)
            .unwrap()
            .attestations
            .iter()
            .map(|a| a.file_path.clone())
            .collect()
    };
    let files1 = files(&note1);
    let files2 = files(&note2);

    assert!(
        files1.iter().any(|f| f.contains("file_a")) && !files1.iter().any(|f| f.contains("file_b")),
        "rebased commit 1 should reference only file_a.txt, got: {files1:?}"
    );
    assert!(
        files2.iter().any(|f| f.contains("file_b")) && !files2.iter().any(|f| f.contains("file_a")),
        "rebased commit 2 should reference only file_b.txt, got: {files2:?}"
    );
}

/// Verify that `git-ai ci local merge` correctly pairs original commits with
/// their rebased counterparts (oldest-first) after a real `git rebase`.
///
/// Creates a two-commit feature branch (commit 1 → file_a.txt, commit 2 →
/// file_b.txt), advances main by one commit so the rebase produces genuinely
/// new SHAs, then rebases the feature branch onto main via plain `git rebase`
/// (bypassing the local hook).  After fast-forwarding main, the test invokes
/// `git-ai ci local merge` exactly as CI would and checks that:
///
/// - The first rebased commit's authorship note references only file_a.txt
/// - The second rebased commit's authorship note references only file_b.txt
///
/// Before the `.reverse()` fix in `ci_context.rs` the pairing was inverted:
/// original_commits came back newest-first from `CommitRange::all_commits()`
/// while new_commits were oldest-first, so each note landed on the wrong commit.
#[test]
fn test_ci_local_rebase_merge_two_commits() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

    let repo = direct_test_repo();

    // --- Initial commit on main ---
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // --- Feature branch: two commits touching different files ---
    repo.git_og(&["checkout", "-b", "feature"]).unwrap();

    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["ai content in file_a".ai()]);
    let feature_sha1 = repo.stage_all_and_commit("Add file_a").unwrap().commit_sha;

    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["ai content in file_b".ai()]);
    let feature_sha2 = repo.stage_all_and_commit("Add file_b").unwrap().commit_sha;

    // --- Advance main so the rebase produces new commit SHAs ---
    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();

    // --- Rebase feature onto main, bypassing the local rebase hook ---
    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();

    let new_sha2 = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha1 = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(
        new_sha1, feature_sha1,
        "rebase must produce a new SHA for commit 1"
    );
    assert_ne!(
        new_sha2, feature_sha2,
        "rebase must produce a new SHA for commit 2"
    );

    // --- Fast-forward main to the rebased feature HEAD ---
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();

    // --- Bare clone so push_authorship("origin") inside CiContext can succeed ---
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_path = origin_dir.path().join("origin.git");
    repo.git_og(&[
        "clone",
        "--bare",
        repo.path().to_str().unwrap(),
        origin_path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "origin", origin_path.to_str().unwrap()])
        .unwrap();

    // --- Run the local CI command as CI would after a rebase merge ---
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            new_sha2.as_str(),
            "--head-ref",
            "feature",
            "--head-sha",
            feature_sha2.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-fetch-base",
        ])
        .expect("ci local merge should succeed");

    assert!(
        output.contains("authorship rewritten successfully"),
        "Expected authorship rewritten, got: {}",
        output
    );

    // --- Verify each rebased commit carries notes for its own file only ---
    let note1 = repo
        .read_authorship_note(&new_sha1)
        .expect("rebased commit 1 should have an authorship note");
    let note2 = repo
        .read_authorship_note(&new_sha2)
        .expect("rebased commit 2 should have an authorship note");

    let files1: Vec<String> = AuthorshipLog::deserialize_from_string(&note1)
        .unwrap()
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect();
    let files2: Vec<String> = AuthorshipLog::deserialize_from_string(&note2)
        .unwrap()
        .attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect();

    assert!(
        files1.iter().any(|f| f.contains("file_a")),
        "rebased commit 1 should reference file_a.txt, got: {:?}",
        files1
    );
    assert!(
        !files1.iter().any(|f| f.contains("file_b")),
        "COMMIT ORDER BUG: rebased commit 1 references file_b (newest-first pairing). Got: {:?}",
        files1
    );
    assert!(
        files2.iter().any(|f| f.contains("file_b")),
        "rebased commit 2 should reference file_b.txt, got: {:?}",
        files2
    );
    assert!(
        !files2.iter().any(|f| f.contains("file_a")),
        "COMMIT ORDER BUG: rebased commit 2 references file_a (newest-first pairing). Got: {:?}",
        files2
    );
}

/// Three-commit variant of `test_ci_local_rebase_merge_two_commits`.
///
/// Each of the three original commits touches a distinct file (file_a / file_b /
/// file_c).  After rebasing onto an advanced main and running
/// `git-ai ci local merge`, every rebased commit must carry the note for its
/// own file and none of the others.  This catches both full inversions
/// (first↔last) and off-by-one shifts in the positional pairing.
#[test]
fn test_ci_local_rebase_merge_three_commits() {
    use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

    let repo = direct_test_repo();

    // --- Initial commit on main ---
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();
    let base_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // --- Feature branch: three commits touching distinct files ---
    repo.git_og(&["checkout", "-b", "feature"]).unwrap();

    let mut file_a = repo.filename("file_a.txt");
    file_a.set_contents(crate::lines!["ai content in file_a".ai()]);
    let feature_sha1 = repo.stage_all_and_commit("Add file_a").unwrap().commit_sha;

    let mut file_b = repo.filename("file_b.txt");
    file_b.set_contents(crate::lines!["ai content in file_b".ai()]);
    let feature_sha2 = repo.stage_all_and_commit("Add file_b").unwrap().commit_sha;

    let mut file_c = repo.filename("file_c.txt");
    file_c.set_contents(crate::lines!["ai content in file_c".ai()]);
    let feature_sha3 = repo.stage_all_and_commit("Add file_c").unwrap().commit_sha;

    // --- Advance main so the rebase produces new commit SHAs ---
    repo.git_og(&["checkout", "main"]).unwrap();
    let mut main_file = repo.filename("main_only.txt");
    main_file.set_contents(crate::lines!["main-only content"]);
    repo.git_og(&["add", "main_only.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "Advance main"]).unwrap();

    // --- Rebase feature onto main, bypassing the local rebase hook ---
    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git_og(&["rebase", "main"]).unwrap();

    let new_sha3 = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha2 = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    let new_sha1 = repo
        .git_og(&["rev-parse", "HEAD~2"])
        .unwrap()
        .trim()
        .to_string();

    assert_ne!(
        new_sha1, feature_sha1,
        "rebase must produce a new SHA for commit 1"
    );
    assert_ne!(
        new_sha2, feature_sha2,
        "rebase must produce a new SHA for commit 2"
    );
    assert_ne!(
        new_sha3, feature_sha3,
        "rebase must produce a new SHA for commit 3"
    );

    // --- Fast-forward main to the rebased feature HEAD ---
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--ff-only", "feature"]).unwrap();

    // --- Bare clone so push_authorship("origin") inside CiContext can succeed ---
    let origin_dir = tempfile::tempdir().unwrap();
    let origin_path = origin_dir.path().join("origin.git");
    repo.git_og(&[
        "clone",
        "--bare",
        repo.path().to_str().unwrap(),
        origin_path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git_og(&["remote", "add", "origin", origin_path.to_str().unwrap()])
        .unwrap();

    // --- Run the local CI command as CI would after a rebase merge ---
    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            new_sha3.as_str(),
            "--head-ref",
            "feature",
            "--head-sha",
            feature_sha3.as_str(),
            "--base-ref",
            "main",
            "--base-sha",
            base_sha.as_str(),
            "--skip-fetch-notes",
            "--skip-fetch-base",
        ])
        .expect("ci local merge should succeed");

    assert!(
        output.contains("authorship rewritten successfully"),
        "Expected authorship rewritten, got: {}",
        output
    );

    // --- Verify each rebased commit carries notes for its own file only ---
    let note1 = repo
        .read_authorship_note(&new_sha1)
        .expect("rebased commit 1 should have an authorship note");
    let note2 = repo
        .read_authorship_note(&new_sha2)
        .expect("rebased commit 2 should have an authorship note");
    let note3 = repo
        .read_authorship_note(&new_sha3)
        .expect("rebased commit 3 should have an authorship note");

    let files = |note: &str| -> Vec<String> {
        AuthorshipLog::deserialize_from_string(note)
            .unwrap()
            .attestations
            .iter()
            .map(|a| a.file_path.clone())
            .collect()
    };

    let files1 = files(&note1);
    let files2 = files(&note2);
    let files3 = files(&note3);

    // Commit 1 → file_a only
    assert!(
        files1.iter().any(|f| f.contains("file_a")),
        "rebased commit 1 should reference file_a.txt, got: {:?}",
        files1
    );
    assert!(
        !files1
            .iter()
            .any(|f| f.contains("file_b") || f.contains("file_c")),
        "COMMIT ORDER BUG: rebased commit 1 references wrong file. Got: {:?}",
        files1
    );

    // Commit 2 → file_b only
    assert!(
        files2.iter().any(|f| f.contains("file_b")),
        "rebased commit 2 should reference file_b.txt, got: {:?}",
        files2
    );
    assert!(
        !files2
            .iter()
            .any(|f| f.contains("file_a") || f.contains("file_c")),
        "COMMIT ORDER BUG: rebased commit 2 references wrong file. Got: {:?}",
        files2
    );

    // Commit 3 → file_c only
    assert!(
        files3.iter().any(|f| f.contains("file_c")),
        "rebased commit 3 should reference file_c.txt, got: {:?}",
        files3
    );
    assert!(
        !files3
            .iter()
            .any(|f| f.contains("file_a") || f.contains("file_b")),
        "COMMIT ORDER BUG: rebased commit 3 references wrong file. Got: {:?}",
        files3
    );
}

/// Squash merge where the feature branch mixes human and AI lines. The CI
/// rewrite must split attribution by author across the single squashed commit.
/// (Originally exercised the removed `rewrite_authorship_after_squash_or_rebase`
/// engine directly; now driven through the real `git-ai ci local merge` CLI.)
#[test]
fn test_ci_squash_merge_mixed_content() {
    let repo = direct_test_repo();
    let mut file = repo.filename("mixed.js");

    // Initial commit on main.
    file.set_contents(crate::lines!["// Base code", "const base = 1;"]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    // Feature branch: known-human comment, AI code, known-human comment.
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(crate::lines![
        "// Base code".human(),
        "const base = 1;".human(),
        "// Human comment".human(),
        "// AI generated function".ai(),
        "function aiHelper() {".ai(),
        "  return true;".ai(),
        "}".ai(),
        "// Another human comment".human()
    ]);
    let head_sha = repo
        .stage_all_and_commit("Add mixed content")
        .unwrap()
        .commit_sha;

    // CI squash merge: a single new commit on main with the squashed content.
    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature via squash");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    // The squashed commit splits attribution by author: known-human lines stay
    // human and the AI block stays AI.
    file.assert_lines_and_blame(crate::lines![
        "// Base code".human(),
        "const base = 1;".human(),
        "// Human comment".human(),
        "// AI generated function".ai(),
        "function aiHelper() {".ai(),
        "  return true;".ai(),
        "}".ai(),
        "// Another human comment".human()
    ]);
}

/// Squash merge where the feature's source commits carry notes but no AI
/// attestations (human-only change): the squashed commit must end up with no AI
/// prompts. Originally exercised the removed engine directly.
#[test]
fn test_ci_squash_merge_empty_notes_preserved() {
    let repo = direct_test_repo();
    let mut file = repo.filename("feature.txt");

    file.set_contents(crate::lines!["base"]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(crate::lines!["base", "human change"]);
    let head_sha = repo
        .stage_all_and_commit("Human change")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature via squash");
    let _ = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);

    // Human-only squash: if a note exists it must carry no AI prompts.
    if let Some(note) = repo.read_authorship_note(&merge_sha) {
        let log = AuthorshipLog::deserialize_from_string(&note).unwrap();
        assert!(
            log.metadata.prompts.is_empty(),
            "human-only squash merge must not produce AI prompts, got: {:?}",
            log.metadata.prompts
        );
    }
}

/// Squash merge where extra lines are added during the merge (conflict
/// resolution / manual tweaks): AI lines stay AI, manually-added lines are
/// untracked human. Originally exercised the removed engine directly.
#[test]
fn test_ci_squash_merge_with_manual_changes() {
    let repo = direct_test_repo();
    let mut file = repo.filename("config.js");

    file.set_contents(crate::lines!["const config = {", "  version: 1", "};"]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(crate::lines![
        "const config = {",
        "  version: 1,",
        "  // AI added feature flag".ai(),
        "  enableAI: true".ai(),
        "};"
    ]);
    let head_sha = repo
        .stage_all_and_commit("Add AI config")
        .unwrap()
        .commit_sha;

    // Squash onto main, then add manual lines before the CI rewrite runs.
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    file.set_contents(crate::lines![
        "const config = {",
        "  version: 1,",
        "  // AI added feature flag",
        "  enableAI: true,",
        "  // Manual addition during merge",
        "  production: false",
        "};"
    ]);
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "Merge feature via squash with tweaks"])
        .unwrap();
    let merge_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    // New-logic behavior (content-based reconciliation): only the AI line whose
    // committed content is byte-identical to the AI checkpoint keeps AI
    // attribution. `enableAI: true,` gained a trailing comma during the squash,
    // so its committed content differs from the AI-authored `enableAI: true` and
    // it is attributed to the committer (human) -- along with the manually-added
    // lines. (The removed engine attributed `enableAI: true,` to AI; this is the
    // intended tightening under the rewrite.)
    file.assert_lines_and_blame(crate::lines![
        "const config = {".human(),
        "  version: 1,".human(),
        "  // AI added feature flag".ai(),
        "  enableAI: true,".human(),
        "  // Manual addition during merge".human(),
        "  production: false".human(),
        "};".human()
    ]);
}

/// Multi-commit feature (AI + AI + human) squashed into one merge commit: the
/// squashed commit splits attribution across authors. Originally exercised the
/// removed engine directly.
#[test]
fn test_ci_rebase_merge_multiple_commits() {
    let repo = direct_test_repo();
    let mut file = repo.filename("app.js");

    file.set_contents(crate::lines!["// App v1", ""]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(
        1,
        crate::lines!["// AI function 1".ai(), "function ai1() { }".ai()],
    );
    repo.stage_all_and_commit("Add AI function 1").unwrap();
    file.insert_at(
        3,
        crate::lines!["// AI function 2".ai(), "function ai2() { }".ai()],
    );
    repo.stage_all_and_commit("Add AI function 2").unwrap();
    file.insert_at(
        5,
        crate::lines!["// Human function", "function human() { }"],
    );
    let head_sha = repo
        .stage_all_and_commit("Add human function")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature branch (squashed)");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    file.assert_lines_and_blame(crate::lines![
        "// App v1".human(),
        "// AI function 1".ai(),
        "function ai1() { }".ai(),
        "// AI function 2".ai(),
        "function ai2() { }".ai(),
        "// Human function".human(),
        "function human() { }".human()
    ]);
}

/// Standard-human variant of `test_ci_squash_merge_basic`: original lines are
/// untracked human (checkpoint `human`) rather than known-human.
#[test]
fn test_ci_squash_merge_basic_standard_human() {
    let repo = direct_test_repo();
    let mut file = repo.filename("feature.js");

    file.set_contents(crate::lines![
        "// Original code".unattributed_human(),
        "function original() {}".unattributed_human()
    ]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(
        2,
        crate::lines![
            "// AI added function".ai(),
            "function aiFeature() {".ai(),
            "  return 'ai code';".ai(),
            "}".ai()
        ],
    );
    let head_sha = repo
        .stage_all_and_commit("Add AI feature")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature via squash");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    file.assert_lines_and_blame(crate::lines![
        "// Original code".unattributed_human(),
        "function original() {}".ai(),
        "// AI added function".ai(),
        "function aiFeature() {".ai(),
        "  return 'ai code';".ai(),
        "}".ai()
    ]);
}

/// Legacy/untracked variant of `test_ci_squash_merge_mixed_content`.
#[test]
fn test_ci_squash_merge_mixed_content_standard_human() {
    let repo = direct_test_repo();
    let mut file = repo.filename("mixed.js");

    file.set_contents(crate::lines![
        "// Base code".unattributed_human(),
        "const base = 1;".unattributed_human()
    ]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(
        2,
        crate::lines![
            "// Untracked comment".unattributed_human(),
            "// AI generated function".ai(),
            "function aiHelper() {".ai(),
            "  return true;".ai(),
            "}".ai(),
            "// Another untracked comment".unattributed_human()
        ],
    );
    let head_sha = repo
        .stage_all_and_commit("Add mixed content")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature via squash");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    file.assert_lines_and_blame(crate::lines![
        "// Base code".unattributed_human(),
        "const base = 1;".ai(),
        "// Untracked comment".ai(),
        "// AI generated function".ai(),
        "function aiHelper() {".ai(),
        "  return true;".ai(),
        "}".ai(),
        "// Another untracked comment".ai()
    ]);
}

/// Standard-human variant of `test_ci_squash_merge_with_manual_changes`.
#[test]
fn test_ci_squash_merge_with_manual_changes_standard_human() {
    let repo = direct_test_repo();
    let mut file = repo.filename("config.js");

    file.set_contents(crate::lines![
        "const config = {".unattributed_human(),
        "  version: 1".unattributed_human(),
        "};".unattributed_human()
    ]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(crate::lines![
        "const config = {".unattributed_human(),
        "  version: 1,".ai(),
        "  // AI added feature flag".ai(),
        "  enableAI: true".ai(),
        "};".unattributed_human()
    ]);
    let head_sha = repo
        .stage_all_and_commit("Add AI config")
        .unwrap()
        .commit_sha;

    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    file.set_contents(crate::lines![
        "const config = {".unattributed_human(),
        "  version: 1,".ai(),
        "  // AI added feature flag".unattributed_human(),
        "  enableAI: true,".unattributed_human(),
        "  // Manual addition during merge".unattributed_human(),
        "  production: false".unattributed_human(),
        "};".unattributed_human()
    ]);
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "Merge feature via squash with tweaks"])
        .unwrap();
    let merge_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    // Same new-logic tightening as test_ci_squash_merge_with_manual_changes:
    // `enableAI: true,` gained a trailing comma during the squash, so its
    // committed content differs from the AI checkpoint and it falls back to
    // untracked human. The leading untracked `version` line is recovered as
    // part of the AI edge.
    file.assert_lines_and_blame(crate::lines![
        "const config = {".unattributed_human(),
        "  version: 1,".ai(),
        "  // AI added feature flag".ai(),
        "  enableAI: true,".unattributed_human(),
        "  // Manual addition during merge".unattributed_human(),
        "  production: false".unattributed_human(),
        "};".unattributed_human()
    ]);
}

/// Standard-human variant of `test_ci_rebase_merge_multiple_commits`.
#[test]
fn test_ci_rebase_merge_multiple_commits_standard_human() {
    let repo = direct_test_repo();
    let mut file = repo.filename("app.js");

    file.set_contents(crate::lines![
        "// App v1".unattributed_human(),
        "".unattributed_human()
    ]);
    let base_sha = repo
        .stage_all_and_commit("Initial commit")
        .unwrap()
        .commit_sha;
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(
        1,
        crate::lines!["// AI function 1".ai(), "function ai1() { }".ai()],
    );
    repo.stage_all_and_commit("Add AI function 1").unwrap();
    file.insert_at(
        3,
        crate::lines!["// AI function 2".ai(), "function ai2() { }".ai()],
    );
    repo.stage_all_and_commit("Add AI function 2").unwrap();
    file.insert_at(
        5,
        crate::lines![
            "// Human function".unattributed_human(),
            "function human() { }".unattributed_human()
        ],
    );
    let head_sha = repo
        .stage_all_and_commit("Add human function")
        .unwrap()
        .commit_sha;

    let merge_sha = squash_feature_with_raw_git(&repo, "Merge feature branch (squashed)");
    let output = run_ci_local_merge(&repo, &merge_sha, &head_sha, &base_sha);
    assert_ci_rewrite_succeeded(&output);

    file.assert_lines_and_blame(crate::lines![
        "// App v1".unattributed_human(),
        "// AI function 1".ai(),
        "function ai1() { }".ai(),
        "// AI function 2".ai(),
        "function ai2() { }".ai(),
        "// Human function".unattributed_human(),
        "function human() { }".unattributed_human()
    ]);
}

/// Regression test for #1473: a squash merge of a multi-commit PR onto a *linear*
/// main branch must not be misclassified as a rebase merge and pollute unrelated
/// base commits. Drives `CiContext::run_with_options` directly (still supported).
#[test]
fn test_ci_squash_merge_not_misclassified_as_rebase_on_linear_main() {
    use git_ai::ci::ci_context::{CiContext, CiEvent, CiRunOptions};

    let repo = direct_test_repo();
    repo.git_og(&["config", "user.name", "Test User"]).unwrap();
    repo.git_og(&["config", "user.email", "test@example.com"])
        .unwrap();

    // B0: initial commit on main (raw git -> no authorship note).
    std::fs::write(repo.path().join("base.txt"), "base content\n").unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "B0 initial"]).unwrap();
    repo.git_og(&["branch", "-M", "main"]).unwrap();
    let b0_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // B1, B2, B3: teammate commits on main, no notes.
    for i in 1..=3 {
        std::fs::write(
            repo.path().join(format!("teammate{i}.txt")),
            format!("teammate change {i}\n"),
        )
        .unwrap();
        repo.git_og(&["add", "-A"]).unwrap();
        repo.git_og(&["commit", "-m", &format!("B{i} teammate change")])
            .unwrap();
    }
    let b2_sha = repo
        .git_og(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    let b3_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // feature branch off B0 with 3 AI commits (each gets a note via the wrapper).
    repo.git_og(&["checkout", "-b", "feature", &b0_sha])
        .unwrap();
    let mut feat = repo.filename("feature.txt");
    feat.set_contents(crate::lines!["// P1 ai line".ai()]);
    repo.stage_all_and_commit("P1").unwrap();
    feat.insert_at(1, crate::lines!["// P2 ai line".ai()]);
    repo.stage_all_and_commit("P2").unwrap();
    feat.insert_at(2, crate::lines!["// P3 ai line".ai()]);
    let head_sha = repo.stage_all_and_commit("P3").unwrap().commit_sha;

    // Squash merge: one new commit S on top of B3 (raw git).
    repo.git_og(&["checkout", "main"]).unwrap();
    std::fs::write(
        repo.path().join("feature.txt"),
        "// P1 ai line\n// P2 ai line\n// P3 ai line\n",
    )
    .unwrap();
    repo.git_og(&["add", "-A"]).unwrap();
    repo.git_og(&["commit", "-m", "Squash merge feature (#PR)"])
        .unwrap();
    let squash_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let git_ai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("Failed to find repository");
    let event = CiEvent::Merge {
        merge_commit_sha: squash_sha.clone(),
        head_ref: "feature".to_string(),
        head_sha: head_sha.clone(),
        base_ref: "main".to_string(),
        base_sha: b3_sha.clone(),
        fork_clone_url: None,
    };
    let ctx = CiContext::with_repository(git_ai_repo, event);
    ctx.run_with_options(CiRunOptions {
        skip_fetch_notes: true,
        skip_fetch_base: true,
        skip_fetch_fork_notes: true,
        skip_fetch_sync_refs: false,
        skip_push: true,
    })
    .expect("CI merge rewrite should succeed");

    // S should be attributed; unrelated base commits B2/B3 must not be polluted.
    assert!(
        repo.read_authorship_note(&squash_sha).is_some(),
        "squash commit S ({squash_sha}) should receive the rewritten authorship note"
    );
    assert!(
        repo.read_authorship_note(&b2_sha).is_none(),
        "#1473 regression: unrelated base commit B2 ({b2_sha}) must not receive a note"
    );
    assert!(
        repo.read_authorship_note(&b3_sha).is_none(),
        "#1473 regression: unrelated base commit B3 ({b3_sha}) must not receive a note"
    );
}

crate::reuse_tests_in_worktree!(
    test_ci_squash_merge_basic,
    test_ci_squash_merge_multiple_files,
    test_ci_squash_merge_mixed_ai_and_human_content,
    test_ci_squash_merge_no_notes_no_authorship_created,
    test_ci_rebase_merge_commit_order_pairing,
    test_ci_local_sync_skips_when_current_rebased_commit_already_has_note,
    test_ci_local_sync_skips_non_rebase_force_push,
    test_ci_local_open_pr_rebase_single_commit,
    test_ci_local_open_pr_rebase_two_commits,
    test_ci_local_merge_squash_on_linear_main_does_not_note_base_commits,
    test_ci_local_rebase_merge_with_abbreviated_merge_sha,
    test_ci_local_rebase_merge_two_commits,
    test_ci_local_rebase_merge_three_commits,
    test_ci_squash_merge_mixed_content,
    test_ci_squash_merge_empty_notes_preserved,
    test_ci_squash_merge_with_manual_changes,
    test_ci_rebase_merge_multiple_commits,
    test_ci_squash_merge_basic_standard_human,
    test_ci_squash_merge_mixed_content_standard_human,
    test_ci_squash_merge_with_manual_changes_standard_human,
    test_ci_rebase_merge_multiple_commits_standard_human,
    test_ci_squash_merge_not_misclassified_as_rebase_on_linear_main,
);
