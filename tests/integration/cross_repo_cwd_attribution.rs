//! Tests for cross-repo CWD attribution.
//!
//! These tests verify that when the CWD of the agent (hook call) differs from the
//! repo root where files are being edited, attribution is still correctly found.
//!
//! Scenarios covered:
//! 1. CWD != repo root, single repo edit
//! 2. CWD != repo root, edits in several different repos
//! 3. CWD != repo root, edits in several repos + CWD repo itself
//! 4. CWD is a parent directory above all repos (e.g. ~/projects)
//! 5. CWD is a parent directory above all repos, edits in several repo subpaths
//! 6. Agent preset (e.g. Claude) with CWD in repo A editing files in repo B (issue #871)

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;

use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Creates a unique temporary directory for tests
fn create_unique_workspace(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir_name = format!("{}-{}-{}-{}", prefix, now, pid, seq);
    let path = base.join(dir_name);
    fs::create_dir_all(&path).expect("failed to create workspace dir");
    path
}

#[test]
fn read_from_non_repo_cwd_returns_an_error_without_sync_panicking() {
    let non_repo_cwd = create_unique_workspace("git-ai-read-non-repo-cwd");
    let repo = TestRepo::new();

    let result = repo.git_ai_from_working_dir(&non_repo_cwd, &["status"]);

    assert!(result.is_err(), "status outside a repository should fail");
    let _ = fs::remove_dir_all(&non_repo_cwd);
}

// ---------------------------------------------------------------------------
// Scenario 1: CWD != repo root, single repo edit, attribution correct
// ---------------------------------------------------------------------------

/// When the agent's CWD is an unrelated directory (not inside any repo being edited),
/// the checkpoint should still correctly attribute AI-written lines in the target repo.
#[test]
fn test_cwd_different_from_repo_root_single_repo() {
    // repo_target is where the file lives
    let repo_target = TestRepo::new();
    // repo_cwd is an unrelated repo used as the agent's CWD
    let repo_cwd = TestRepo::new();

    // Set up initial commit in the target repo
    let mut readme = repo_target.filename("README.md");
    readme.set_contents(crate::lines!["# Target Repo"]);
    repo_target.stage_all_and_commit("initial commit").unwrap();

    // Write AI content to a file in the target repo
    fs::write(
        repo_target.path().join("feature.txt"),
        "Human line 1\nAI line 1\nAI line 2\n",
    )
    .unwrap();

    // Checkpoint from the CWD of the unrelated repo, passing absolute paths
    let target_file_abs = repo_target.canonical_path().join("feature.txt");
    repo_target
        .git_ai_from_working_dir(
            &repo_cwd.canonical_path(),
            &["checkpoint", "mock_ai", target_file_abs.to_str().unwrap()],
        )
        .expect("checkpoint from different CWD should succeed");

    // Verify the working log was written in the target repo (before committing,
    // since the working log is keyed by the base commit SHA at checkpoint time)
    let working_log = repo_target.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Scenario 1: Working log entries should exist in the target repo \
         when checkpoint is run from a different CWD."
    );

    // Commit in the target repo
    let commit = repo_target.stage_all_and_commit("add AI feature").unwrap();

    // Verify AI attribution is present
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Scenario 1: AI edits in the target repo should be attributed correctly \
         even when checkpoint is run from a different CWD (an unrelated repo). \
         Found no attestations."
    );
}

// ---------------------------------------------------------------------------
// Scenario 2: CWD != repo root, edits in several different repos
// ---------------------------------------------------------------------------

/// When the agent's CWD is an unrelated directory, and it checkpoints files
/// across multiple different repos, each repo should get correct attribution.
#[test]
fn test_cwd_different_from_repo_root_multiple_repos() {
    let repo_cwd = TestRepo::new(); // unrelated CWD
    let repo_a = TestRepo::new();
    let repo_b = TestRepo::new();
    let repo_c = TestRepo::new();

    // Set up initial commits
    for (repo, name) in [(&repo_a, "A"), (&repo_b, "B"), (&repo_c, "C")] {
        let mut readme = repo.filename("README.md");
        readme.set_contents(crate::lines![format!("# Repo {}", name)]);
        repo.stage_all_and_commit("initial commit").unwrap();
    }

    // Write AI content in each repo
    fs::write(repo_a.path().join("a.txt"), "AI line A1\nAI line A2\n").unwrap();
    fs::write(repo_b.path().join("b.txt"), "AI line B1\nAI line B2\n").unwrap();
    fs::write(repo_c.path().join("c.txt"), "AI line C1\n").unwrap();

    // Checkpoint from unrelated CWD, passing all file paths from different repos
    let file_a = repo_a.canonical_path().join("a.txt");
    let file_b = repo_b.canonical_path().join("b.txt");
    let file_c = repo_c.canonical_path().join("c.txt");

    // Use repo_a to run the checkpoint (the method needs a repo object, but CWD is repo_cwd)
    repo_a
        .git_ai_from_working_dir(
            &repo_cwd.canonical_path(),
            &[
                "checkpoint",
                "mock_ai",
                file_a.to_str().unwrap(),
                file_b.to_str().unwrap(),
                file_c.to_str().unwrap(),
            ],
        )
        .expect("checkpoint across multiple repos from different CWD should succeed");

    // Commit in each repo and verify attribution
    let commit_a = repo_a.stage_all_and_commit("AI edits in A").unwrap();
    assert!(
        !commit_a.authorship_log.attestations.is_empty(),
        "Scenario 2: repo_a should have AI attestations when checkpoint was run \
         from an unrelated CWD with files across multiple repos."
    );

    let commit_b = repo_b.stage_all_and_commit("AI edits in B").unwrap();
    assert!(
        !commit_b.authorship_log.attestations.is_empty(),
        "Scenario 2: repo_b should have AI attestations when checkpoint was run \
         from an unrelated CWD with files across multiple repos."
    );

    let commit_c = repo_c.stage_all_and_commit("AI edits in C").unwrap();
    assert!(
        !commit_c.authorship_log.attestations.is_empty(),
        "Scenario 2: repo_c should have AI attestations when checkpoint was run \
         from an unrelated CWD with files across multiple repos."
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: CWD != repo root, edits in several repos + the CWD repo itself
// ---------------------------------------------------------------------------

/// When the agent's CWD is inside one of the repos being edited, and edits also
/// span other repos, all repos should get correct attribution including the CWD repo.
#[test]
fn test_cwd_is_one_of_edited_repos_plus_others() {
    let repo_cwd = TestRepo::new(); // CWD is this repo, AND it has edits
    let repo_other1 = TestRepo::new();
    let repo_other2 = TestRepo::new();

    // Set up initial commits
    let mut readme_cwd = repo_cwd.filename("README.md");
    readme_cwd.set_contents(crate::lines!["# CWD Repo"]);
    repo_cwd.stage_all_and_commit("initial commit").unwrap();

    let mut readme_o1 = repo_other1.filename("README.md");
    readme_o1.set_contents(crate::lines!["# Other1 Repo"]);
    repo_other1.stage_all_and_commit("initial commit").unwrap();

    let mut readme_o2 = repo_other2.filename("README.md");
    readme_o2.set_contents(crate::lines!["# Other2 Repo"]);
    repo_other2.stage_all_and_commit("initial commit").unwrap();

    // Write AI content in all three repos (including the CWD repo)
    fs::write(repo_cwd.path().join("cwd_file.txt"), "AI in CWD repo\n").unwrap();
    fs::write(repo_other1.path().join("other1.txt"), "AI in other1\n").unwrap();
    fs::write(repo_other2.path().join("other2.txt"), "AI in other2\n").unwrap();

    let file_cwd = repo_cwd.canonical_path().join("cwd_file.txt");
    let file_o1 = repo_other1.canonical_path().join("other1.txt");
    let file_o2 = repo_other2.canonical_path().join("other2.txt");

    // Checkpoint from the CWD repo itself, with files spanning all three repos
    repo_cwd
        .git_ai_from_working_dir(
            &repo_cwd.canonical_path(),
            &[
                "checkpoint",
                "mock_ai",
                file_cwd.to_str().unwrap(),
                file_o1.to_str().unwrap(),
                file_o2.to_str().unwrap(),
            ],
        )
        .expect("checkpoint from CWD repo with cross-repo files should succeed");

    // Verify the CWD repo has attribution (it is also a target)
    let commit_cwd = repo_cwd
        .stage_all_and_commit("AI edits in CWD repo")
        .unwrap();
    assert!(
        !commit_cwd.authorship_log.attestations.is_empty(),
        "Scenario 3: CWD repo should have AI attestations when it is also one of the \
         repos with edits."
    );

    // Verify other repos have attribution too
    let commit_o1 = repo_other1
        .stage_all_and_commit("AI edits in other1")
        .unwrap();
    assert!(
        !commit_o1.authorship_log.attestations.is_empty(),
        "Scenario 3: other1 repo should have AI attestations when checkpoint was run \
         from the CWD repo (which is different from other1)."
    );

    let commit_o2 = repo_other2
        .stage_all_and_commit("AI edits in other2")
        .unwrap();
    assert!(
        !commit_o2.authorship_log.attestations.is_empty(),
        "Scenario 3: other2 repo should have AI attestations when checkpoint was run \
         from the CWD repo (which is different from other2)."
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: CWD is a parent directory above all repos (e.g. ~/projects)
// ---------------------------------------------------------------------------

/// When the agent's CWD is a parent directory that contains the repos as
/// subdirectories (simulating ~/projects), attribution should still work.
#[test]
fn test_cwd_is_parent_dir_above_repos_single_repo() {
    // Create a workspace directory (simulating ~/projects - NOT a git repo)
    let workspace = create_unique_workspace("git-ai-cwd-parent-test");

    // Create repos inside the workspace
    let repo_path = workspace.join("project-alpha");
    let repo = TestRepo::new_at_path(&repo_path);

    // Set up initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project Alpha"]);
    repo.stage_all_and_commit("initial commit").unwrap();

    // Write AI content
    fs::write(
        repo_path.join("alpha.txt"),
        "AI alpha line 1\nAI alpha line 2\n",
    )
    .unwrap();

    let file_abs = repo.canonical_path().join("alpha.txt");

    // Checkpoint from the workspace parent directory (above the repo)
    repo.git_ai_from_working_dir(
        &workspace,
        &["checkpoint", "mock_ai", file_abs.to_str().unwrap()],
    )
    .expect("checkpoint from parent directory above repo should succeed");

    // Verify the working log was written (check before committing)
    let working_log = repo.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Scenario 4: Working log should have entries when CWD is parent directory."
    );

    // Commit and verify
    let commit = repo
        .stage_all_and_commit("AI edits from parent CWD")
        .unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Scenario 4: AI edits should be attributed correctly when the agent's CWD is \
         a parent directory above the repo (e.g. ~/projects)."
    );

    // Cleanup workspace (repos are cleaned up by TestRepo Drop)
    let _ = fs::remove_dir_all(&workspace);
}

/// Scenario 4 variant: CWD is parent, edits across multiple repos under it.
#[test]
fn test_cwd_is_parent_dir_above_multiple_repos() {
    let workspace = create_unique_workspace("git-ai-cwd-parent-multi-test");

    let repo_a_path = workspace.join("repo-alpha");
    let repo_b_path = workspace.join("repo-beta");

    let repo_a = TestRepo::new_at_path(&repo_a_path);
    let repo_b = TestRepo::new_at_path(&repo_b_path);

    // Set up initial commits
    let mut readme_a = repo_a.filename("README.md");
    readme_a.set_contents(crate::lines!["# Alpha"]);
    repo_a.stage_all_and_commit("initial commit").unwrap();

    let mut readme_b = repo_b.filename("README.md");
    readme_b.set_contents(crate::lines!["# Beta"]);
    repo_b.stage_all_and_commit("initial commit").unwrap();

    // Write AI content
    fs::write(repo_a_path.join("alpha.txt"), "AI alpha\n").unwrap();
    fs::write(repo_b_path.join("beta.txt"), "AI beta\n").unwrap();

    let file_a = repo_a.canonical_path().join("alpha.txt");
    let file_b = repo_b.canonical_path().join("beta.txt");

    // Checkpoint from the workspace parent directory
    repo_a
        .git_ai_from_working_dir(
            &workspace,
            &[
                "checkpoint",
                "mock_ai",
                file_a.to_str().unwrap(),
                file_b.to_str().unwrap(),
            ],
        )
        .expect("checkpoint from parent directory with multiple repos should succeed");

    let commit_a = repo_a.stage_all_and_commit("AI edits in alpha").unwrap();
    assert!(
        !commit_a.authorship_log.attestations.is_empty(),
        "Scenario 4 (multi): repo_a should have AI attestations when CWD is parent."
    );

    let commit_b = repo_b.stage_all_and_commit("AI edits in beta").unwrap();
    assert!(
        !commit_b.authorship_log.attestations.is_empty(),
        "Scenario 4 (multi): repo_b should have AI attestations when CWD is parent."
    );

    let _ = fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// Scenario 5: CWD above all repos, edits in several different repo subpaths
// ---------------------------------------------------------------------------

/// When CWD is a parent directory (like ~/projects), and edits span files in
/// subdirectories of multiple repos, attribution should work for all of them.
#[test]
fn test_cwd_parent_dir_edits_in_repo_subpaths() {
    let workspace = create_unique_workspace("git-ai-cwd-parent-subpaths-test");

    let repo_x_path = workspace.join("project-x");
    let repo_y_path = workspace.join("project-y");
    let repo_z_path = workspace.join("project-z");

    let repo_x = TestRepo::new_at_path(&repo_x_path);
    let repo_y = TestRepo::new_at_path(&repo_y_path);
    let repo_z = TestRepo::new_at_path(&repo_z_path);

    // Set up initial commits
    for (repo, name) in [(&repo_x, "X"), (&repo_y, "Y"), (&repo_z, "Z")] {
        let mut readme = repo.filename("README.md");
        readme.set_contents(crate::lines![format!("# Project {}", name)]);
        repo.stage_all_and_commit("initial commit").unwrap();
    }

    // Write AI content in subdirectories of each repo
    fs::create_dir_all(repo_x_path.join("src").join("components")).unwrap();
    fs::write(
        repo_x_path.join("src").join("components").join("widget.rs"),
        "AI widget code\n",
    )
    .unwrap();

    fs::create_dir_all(repo_y_path.join("lib")).unwrap();
    fs::write(repo_y_path.join("lib").join("utils.py"), "AI utils code\n").unwrap();

    fs::create_dir_all(repo_z_path.join("pkg").join("api")).unwrap();
    fs::write(
        repo_z_path.join("pkg").join("api").join("handler.go"),
        "AI handler code\n",
    )
    .unwrap();

    let file_x = repo_x
        .canonical_path()
        .join("src")
        .join("components")
        .join("widget.rs");
    let file_y = repo_y.canonical_path().join("lib").join("utils.py");
    let file_z = repo_z
        .canonical_path()
        .join("pkg")
        .join("api")
        .join("handler.go");

    // Checkpoint from the workspace parent directory
    repo_x
        .git_ai_from_working_dir(
            &workspace,
            &[
                "checkpoint",
                "mock_ai",
                file_x.to_str().unwrap(),
                file_y.to_str().unwrap(),
                file_z.to_str().unwrap(),
            ],
        )
        .expect("checkpoint from parent directory with files in repo subpaths should succeed");

    // Verify attribution in each repo
    let commit_x = repo_x.stage_all_and_commit("AI edits in X").unwrap();
    assert!(
        !commit_x.authorship_log.attestations.is_empty(),
        "Scenario 5: repo_x should have AI attestations for deeply nested file \
         when CWD is the parent directory above all repos."
    );

    let commit_y = repo_y.stage_all_and_commit("AI edits in Y").unwrap();
    assert!(
        !commit_y.authorship_log.attestations.is_empty(),
        "Scenario 5: repo_y should have AI attestations for file in lib/ subpath \
         when CWD is the parent directory above all repos."
    );

    let commit_z = repo_z.stage_all_and_commit("AI edits in Z").unwrap();
    assert!(
        !commit_z.authorship_log.attestations.is_empty(),
        "Scenario 5: repo_z should have AI attestations for file in pkg/api/ subpath \
         when CWD is the parent directory above all repos."
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// Scenario 5 variant: CWD above repos, edits in multiple files per repo across subpaths.
#[test]
fn test_cwd_parent_dir_multiple_files_per_repo_subpaths() {
    let workspace = create_unique_workspace("git-ai-cwd-parent-multi-files-test");

    let repo_fe_path = workspace.join("frontend");
    let repo_be_path = workspace.join("backend");

    let repo_fe = TestRepo::new_at_path(&repo_fe_path);
    let repo_be = TestRepo::new_at_path(&repo_be_path);

    // Set up initial commits
    let mut readme_fe = repo_fe.filename("README.md");
    readme_fe.set_contents(crate::lines!["# Frontend"]);
    repo_fe.stage_all_and_commit("initial commit").unwrap();

    let mut readme_be = repo_be.filename("README.md");
    readme_be.set_contents(crate::lines!["# Backend"]);
    repo_be.stage_all_and_commit("initial commit").unwrap();

    // Write multiple AI files in subdirectories of each repo
    fs::create_dir_all(repo_fe_path.join("src").join("pages")).unwrap();
    fs::create_dir_all(repo_fe_path.join("src").join("hooks")).unwrap();
    fs::write(
        repo_fe_path.join("src").join("pages").join("home.tsx"),
        "AI home page\n",
    )
    .unwrap();
    fs::write(
        repo_fe_path.join("src").join("hooks").join("useAuth.ts"),
        "AI auth hook\n",
    )
    .unwrap();

    fs::create_dir_all(repo_be_path.join("api").join("routes")).unwrap();
    fs::create_dir_all(repo_be_path.join("api").join("middleware")).unwrap();
    fs::write(
        repo_be_path.join("api").join("routes").join("users.py"),
        "AI users route\n",
    )
    .unwrap();
    fs::write(
        repo_be_path.join("api").join("middleware").join("auth.py"),
        "AI auth middleware\n",
    )
    .unwrap();

    let fe_file1 = repo_fe
        .canonical_path()
        .join("src")
        .join("pages")
        .join("home.tsx");
    let fe_file2 = repo_fe
        .canonical_path()
        .join("src")
        .join("hooks")
        .join("useAuth.ts");
    let be_file1 = repo_be
        .canonical_path()
        .join("api")
        .join("routes")
        .join("users.py");
    let be_file2 = repo_be
        .canonical_path()
        .join("api")
        .join("middleware")
        .join("auth.py");

    // Checkpoint from workspace parent with all four files
    repo_fe
        .git_ai_from_working_dir(
            &workspace,
            &[
                "checkpoint",
                "mock_ai",
                fe_file1.to_str().unwrap(),
                fe_file2.to_str().unwrap(),
                be_file1.to_str().unwrap(),
                be_file2.to_str().unwrap(),
            ],
        )
        .expect(
            "checkpoint from parent with multiple files in multiple repo subpaths should succeed",
        );

    // Verify frontend repo attribution
    let commit_fe = repo_fe
        .stage_all_and_commit("AI edits in frontend")
        .unwrap();
    assert!(
        !commit_fe.authorship_log.attestations.is_empty(),
        "Scenario 5 (multi-file): frontend repo should have AI attestations."
    );
    assert!(
        commit_fe.authorship_log.attestations.len() >= 2,
        "Scenario 5 (multi-file): frontend repo should have attestations for at least 2 files, \
         got {}",
        commit_fe.authorship_log.attestations.len()
    );

    // Verify backend repo attribution
    let commit_be = repo_be.stage_all_and_commit("AI edits in backend").unwrap();
    assert!(
        !commit_be.authorship_log.attestations.is_empty(),
        "Scenario 5 (multi-file): backend repo should have AI attestations."
    );
    assert!(
        commit_be.authorship_log.attestations.len() >= 2,
        "Scenario 5 (multi-file): backend repo should have attestations for at least 2 files, \
         got {}",
        commit_be.authorship_log.attestations.len()
    );

    let _ = fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// Additional edge case: blame verification for cross-repo CWD attribution
// ---------------------------------------------------------------------------

/// Verify that blame output correctly shows AI authorship for lines written
/// via cross-repo checkpoint where CWD is an unrelated directory.
#[test]
fn test_cross_repo_cwd_blame_shows_correct_attribution() {
    let repo_cwd = TestRepo::new();
    let repo_target = TestRepo::new();

    // Set up target repo with an existing file
    let mut existing = repo_target.filename("existing.txt");
    existing.set_contents(crate::lines!["Human line 1", "Human line 2"]);
    repo_target
        .stage_all_and_commit("initial commit with human lines")
        .unwrap();

    // Append AI lines to the existing file
    fs::write(
        repo_target.path().join("existing.txt"),
        "Human line 1\nHuman line 2\nAI appended line 1\nAI appended line 2\n",
    )
    .unwrap();

    let target_file_abs = repo_target.canonical_path().join("existing.txt");

    // Checkpoint from the unrelated CWD
    repo_target
        .git_ai_from_working_dir(
            &repo_cwd.canonical_path(),
            &["checkpoint", "mock_ai", target_file_abs.to_str().unwrap()],
        )
        .expect("cross-repo CWD checkpoint should succeed");

    // Commit and verify blame
    repo_target
        .stage_all_and_commit("add AI lines from cross-repo CWD")
        .unwrap();

    let mut file = repo_target.filename("existing.txt");
    file.assert_lines_and_blame(vec![
        "Human line 1".human(),
        "Human line 2".ai(),
        "AI appended line 1".ai(),
        "AI appended line 2".ai(),
    ]);
}

/// Verify blame across multiple repos when CWD is a parent directory.
#[test]
fn test_parent_cwd_blame_correct_across_repos() {
    let workspace = create_unique_workspace("git-ai-parent-blame-test");

    let repo_a_path = workspace.join("svc-a");
    let repo_b_path = workspace.join("svc-b");

    let repo_a = TestRepo::new_at_path(&repo_a_path);
    let repo_b = TestRepo::new_at_path(&repo_b_path);

    // Initial commits with human content
    let mut file_a = repo_a.filename("code.txt");
    file_a.set_contents(crate::lines!["Human A1", "Human A2"]);
    repo_a.stage_all_and_commit("initial A").unwrap();

    let mut file_b = repo_b.filename("code.txt");
    file_b.set_contents(crate::lines!["Human B1"]);
    repo_b.stage_all_and_commit("initial B").unwrap();

    // Write mixed content (human + AI appended)
    fs::write(repo_a_path.join("code.txt"), "Human A1\nHuman A2\nAI A3\n").unwrap();
    fs::write(repo_b_path.join("code.txt"), "Human B1\nAI B2\nAI B3\n").unwrap();

    let abs_a = repo_a.canonical_path().join("code.txt");
    let abs_b = repo_b.canonical_path().join("code.txt");

    // Checkpoint from parent workspace
    repo_a
        .git_ai_from_working_dir(
            &workspace,
            &[
                "checkpoint",
                "mock_ai",
                abs_a.to_str().unwrap(),
                abs_b.to_str().unwrap(),
            ],
        )
        .expect("parent-CWD checkpoint for blame test should succeed");

    // Commit both repos
    repo_a.stage_all_and_commit("AI additions A").unwrap();
    repo_b.stage_all_and_commit("AI additions B").unwrap();

    // Verify blame in repo_a
    let mut blamed_a = repo_a.filename("code.txt");
    blamed_a.assert_lines_and_blame(vec!["Human A1".human(), "Human A2".ai(), "AI A3".ai()]);

    // Verify blame in repo_b
    let mut blamed_b = repo_b.filename("code.txt");
    blamed_b.assert_lines_and_blame(vec!["Human B1".ai(), "AI B2".ai(), "AI B3".ai()]);

    let _ = fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// Scenario 6: Agent preset (Claude) with CWD in repo A, editing files in repo B
// Regression test for issue #871
// ---------------------------------------------------------------------------

/// When Claude Code is started in repo A but edits a file in repo B,
/// the checkpoint should record data in repo B so that committing in repo B
/// produces non-empty prompts in the git note.
#[test]
fn test_claude_preset_cross_repo_cwd_records_prompts_in_target_repo() {
    // repo_cwd is where the agent session runs (repo A)
    let repo_cwd = TestRepo::new();
    // repo_target is where the file is actually edited (repo B)
    let mut repo_target = TestRepo::new();

    // Enable prompt sharing for the target repo
    repo_target.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let cwd_root = repo_cwd.canonical_path();
    let target_root = repo_target.canonical_path();

    // Set up initial commits in both repos
    fs::write(cwd_root.join("README.md"), "# Repo A\n").unwrap();
    repo_cwd.stage_all_and_commit("initial commit A").unwrap();

    let src_dir = target_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let target_file = src_dir.join("main.ts");
    fs::write(&target_file, "console.log('hello');\n").unwrap();
    repo_target
        .stage_all_and_commit("initial commit B")
        .unwrap();

    // Create a transcript file that the Claude preset can parse
    let transcript_path = target_root.join("claude-session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();

    // Build hook input JSON simulating Claude Code running from repo A
    // but editing a file in repo B (absolute path)
    let hook_input = json!({
        "cwd": cwd_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Simulate AI edit in repo B
    fs::write(
        &target_file,
        "console.log('hello');\nconsole.log('AI was here');\n",
    )
    .unwrap();

    // Run checkpoint from repo A's CWD, with Claude preset hook input
    // pointing to a file in repo B
    repo_target
        .git_ai_from_working_dir(
            &cwd_root,
            &["checkpoint", "claude", "--hook-input", &hook_input],
        )
        .expect("checkpoint from cross-repo CWD should succeed");

    // Verify the working log was written in the target repo
    let working_log = repo_target.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Issue #871 regression: Working log entries should exist in repo B \
         when Claude checkpoint is run from repo A's CWD."
    );

    // Commit in repo B
    let commit = repo_target.stage_all_and_commit("add AI changes").unwrap();

    // The core assertion: sessions must NOT be empty
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Issue #871 regression: Sessions should not be empty in repo B's git note \
         when Claude Code is started in repo A but edits files in repo B."
    );

    // Verify attestations are present too
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Issue #871 regression: AI attestations should be present in repo B \
         when checkpoint is run from repo A's CWD with Claude preset."
    );
}

/// Same as above but tests the PreToolUse (human checkpoint) path.
/// The human checkpoint should also correctly record in repo B when CWD is repo A.
#[test]
fn test_claude_preset_cross_repo_cwd_pre_tool_use_records_in_target_repo() {
    let repo_cwd = TestRepo::new();
    let repo_target = TestRepo::new();

    let cwd_root = repo_cwd.canonical_path();
    let target_root = repo_target.canonical_path();

    // Set up initial commits
    fs::write(cwd_root.join("README.md"), "# Repo A\n").unwrap();
    repo_cwd.stage_all_and_commit("initial commit A").unwrap();

    let target_file = target_root.join("feature.txt");
    fs::write(&target_file, "line 1\nline 2\n").unwrap();
    repo_target
        .stage_all_and_commit("initial commit B")
        .unwrap();

    // Create a transcript file
    let transcript_path = target_root.join("claude-session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();

    // PreToolUse hook input (human checkpoint before AI edit)
    let pre_hook_input = json!({
        "cwd": cwd_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Run PreToolUse checkpoint from repo A's CWD
    repo_target
        .git_ai_from_working_dir(
            &cwd_root,
            &["checkpoint", "claude", "--hook-input", &pre_hook_input],
        )
        .expect("PreToolUse checkpoint from cross-repo CWD should succeed");

    // Simulate AI edit in repo B
    fs::write(&target_file, "line 1\nline 2\nAI line 3\n").unwrap();

    // PostToolUse hook input
    let post_hook_input = json!({
        "cwd": cwd_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Run PostToolUse checkpoint from repo A's CWD
    repo_target
        .git_ai_from_working_dir(
            &cwd_root,
            &["checkpoint", "claude", "--hook-input", &post_hook_input],
        )
        .expect("PostToolUse checkpoint from cross-repo CWD should succeed");

    // Commit in repo B
    let commit = repo_target.stage_all_and_commit("add AI changes").unwrap();

    // Verify attestations are present
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Issue #871 regression: AI attestations should be present in repo B \
         when PreToolUse + PostToolUse checkpoints run from repo A's CWD."
    );
}

// ===========================================================================
// Scenario 7: Nested subrepo inside parent repo (mock_ai)
//
// Unlike scenarios 1-6 where repos are siblings or in unrelated directories,
// here the subrepo is a subdirectory of the parent repo. Both have their own
// .git/ directory. This tests that path_is_in_workdir correctly detects the
// nested .git boundary and routes checkpoints to the subrepo.
// ===========================================================================

/// When CWD is a parent git repo and the edited file is inside a nested
/// subrepo (subdirectory with its own .git/), the checkpoint must be written
/// to the subrepo, not the parent.
#[test]
fn test_nested_subrepo_single_file_mock_ai() {
    let workspace = create_unique_workspace("git-ai-nested-subrepo-test");

    // Create parent repo
    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    let mut parent_readme = parent.filename("README.md");
    parent_readme.set_contents(crate::lines!["# Parent Repo"]);
    parent.stage_all_and_commit("initial parent").unwrap();

    // Create nested subrepo inside parent repo
    let subrepo_path = parent_path.join("subrepo");
    let subrepo = TestRepo::new_at_path(&subrepo_path);
    let mut subrepo_readme = subrepo.filename("README.md");
    subrepo_readme.set_contents(crate::lines!["# Subrepo"]);
    subrepo.stage_all_and_commit("initial subrepo").unwrap();

    // Write AI content in the subrepo
    fs::write(subrepo_path.join("feature.txt"), "AI line 1\nAI line 2\n").unwrap();

    let subrepo_file = subrepo.canonical_path().join("feature.txt");

    // Checkpoint from parent repo's CWD, targeting file in nested subrepo
    parent
        .git_ai_from_working_dir(
            &parent.canonical_path(),
            &["checkpoint", "mock_ai", subrepo_file.to_str().unwrap()],
        )
        .expect("checkpoint from parent repo CWD for nested subrepo file should succeed");

    // Verify working log was written in the subrepo, not the parent
    let working_log = subrepo.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Scenario 7: Working log entries should exist in the nested subrepo \
         when checkpoint is run from the parent repo's CWD."
    );

    // Commit in the subrepo and verify AI attribution
    let commit = subrepo.stage_all_and_commit("add AI feature").unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Scenario 7: AI attestations should be present in the nested subrepo \
         when checkpoint was run from the parent repo's CWD."
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// Mixed edits: some files in parent repo, some in nested subrepo.
/// Both repos should get their respective checkpoints.
#[test]
fn test_nested_subrepo_mixed_edits_mock_ai() {
    let workspace = create_unique_workspace("git-ai-nested-mixed-test");

    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    let mut parent_readme = parent.filename("README.md");
    parent_readme.set_contents(crate::lines!["# Parent"]);
    parent.stage_all_and_commit("initial parent").unwrap();

    let subrepo_path = parent_path.join("subrepo");
    let subrepo = TestRepo::new_at_path(&subrepo_path);
    let mut subrepo_readme = subrepo.filename("README.md");
    subrepo_readme.set_contents(crate::lines!["# Subrepo"]);
    subrepo.stage_all_and_commit("initial subrepo").unwrap();

    // Write AI content in both repos
    fs::write(parent_path.join("parent_feature.txt"), "Parent AI\n").unwrap();
    fs::write(subrepo_path.join("subrepo_feature.txt"), "Subrepo AI\n").unwrap();

    let parent_file = parent.canonical_path().join("parent_feature.txt");
    let subrepo_file = subrepo.canonical_path().join("subrepo_feature.txt");

    // Checkpoint from parent CWD with files in both repos
    parent
        .git_ai_from_working_dir(
            &parent.canonical_path(),
            &[
                "checkpoint",
                "mock_ai",
                parent_file.to_str().unwrap(),
                subrepo_file.to_str().unwrap(),
            ],
        )
        .expect("mixed checkpoint (parent + nested subrepo) should succeed");

    // The mixed checkpoint fans out to both repo families; drain both before
    // committing so this test does not race delegated checkpoint processing.
    parent.sync_daemon();
    subrepo.sync_daemon();

    // Verify parent repo has attestation
    let parent_commit = parent.stage_all_and_commit("AI edits in parent").unwrap();
    assert!(
        !parent_commit.authorship_log.attestations.is_empty(),
        "Scenario 7 (mixed): Parent repo should have AI attestations for its own files."
    );

    // Verify subrepo has attestation
    let subrepo_commit = subrepo.stage_all_and_commit("AI edits in subrepo").unwrap();
    assert!(
        !subrepo_commit.authorship_log.attestations.is_empty(),
        "Scenario 7 (mixed): Nested subrepo should have AI attestations for its own files."
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// Multiple nested subrepos inside the same parent repo.
#[test]
fn test_nested_multiple_subrepos_mock_ai() {
    let workspace = create_unique_workspace("git-ai-nested-multi-sub-test");

    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    let mut parent_readme = parent.filename("README.md");
    parent_readme.set_contents(crate::lines!["# Parent"]);
    parent.stage_all_and_commit("initial parent").unwrap();

    let sub1_path = parent_path.join("sub1");
    let sub1 = TestRepo::new_at_path(&sub1_path);
    let mut sub1_readme = sub1.filename("README.md");
    sub1_readme.set_contents(crate::lines!["# Sub1"]);
    sub1.stage_all_and_commit("initial sub1").unwrap();

    let sub2_path = parent_path.join("sub2");
    let sub2 = TestRepo::new_at_path(&sub2_path);
    let mut sub2_readme = sub2.filename("README.md");
    sub2_readme.set_contents(crate::lines!["# Sub2"]);
    sub2.stage_all_and_commit("initial sub2").unwrap();

    fs::write(sub1_path.join("s1.txt"), "AI sub1\n").unwrap();
    fs::write(sub2_path.join("s2.txt"), "AI sub2\n").unwrap();

    let file_s1 = sub1.canonical_path().join("s1.txt");
    let file_s2 = sub2.canonical_path().join("s2.txt");

    parent
        .git_ai_from_working_dir(
            &parent.canonical_path(),
            &[
                "checkpoint",
                "mock_ai",
                file_s1.to_str().unwrap(),
                file_s2.to_str().unwrap(),
            ],
        )
        .expect("checkpoint across multiple nested subrepos should succeed");

    // The checkpoint spans two nested repo families; drain both before either
    // commit so each repo sees its delegated checkpoint.
    sub1.sync_daemon();
    sub2.sync_daemon();

    let commit_s1 = sub1.stage_all_and_commit("AI in sub1").unwrap();
    assert!(
        !commit_s1.authorship_log.attestations.is_empty(),
        "Scenario 7 (multi-sub): sub1 should have AI attestations."
    );

    let commit_s2 = sub2.stage_all_and_commit("AI in sub2").unwrap();
    assert!(
        !commit_s2.authorship_log.attestations.is_empty(),
        "Scenario 7 (multi-sub): sub2 should have AI attestations."
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// Blame verification for nested subrepo: human lines stay human,
/// AI-appended lines show as AI.
#[test]
fn test_nested_subrepo_blame_attribution_mock_ai() {
    let workspace = create_unique_workspace("git-ai-nested-blame-test");

    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    let mut parent_readme = parent.filename("README.md");
    parent_readme.set_contents(crate::lines!["# Parent"]);
    parent.stage_all_and_commit("initial parent").unwrap();

    let subrepo_path = parent_path.join("subrepo");
    let subrepo = TestRepo::new_at_path(&subrepo_path);

    // Initial human content in subrepo
    let mut existing = subrepo.filename("code.txt");
    existing.set_contents(crate::lines!["Human line 1", "Human line 2"]);
    subrepo.stage_all_and_commit("initial subrepo").unwrap();

    // Append AI lines
    fs::write(
        subrepo_path.join("code.txt"),
        "Human line 1\nHuman line 2\nAI appended 1\nAI appended 2\n",
    )
    .unwrap();

    let target_file = subrepo.canonical_path().join("code.txt");

    parent
        .git_ai_from_working_dir(
            &parent.canonical_path(),
            &["checkpoint", "mock_ai", target_file.to_str().unwrap()],
        )
        .expect("nested subrepo blame checkpoint should succeed");

    subrepo
        .stage_all_and_commit("add AI lines from parent CWD")
        .unwrap();

    let mut file = subrepo.filename("code.txt");
    file.assert_lines_and_blame(vec![
        "Human line 1".human(),
        "Human line 2".ai(),
        "AI appended 1".ai(),
        "AI appended 2".ai(),
    ]);

    let _ = fs::remove_dir_all(&workspace);
}

// ===========================================================================
// Scenario 8: Nested subrepo with Claude preset
//
// Mirrors scenario 6 (issue #871) but with the subrepo nested inside the
// parent repo instead of being a sibling repo.
// ===========================================================================

/// Claude Code running in parent repo, editing a file in nested subrepo.
/// Checkpoint should record data in subrepo so commit produces non-empty prompts.
#[test]
fn test_claude_preset_nested_subrepo_records_prompts() {
    let workspace = create_unique_workspace("git-ai-claude-nested-test");

    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    fs::write(parent_path.join("README.md"), "# Parent Repo\n").unwrap();
    parent.stage_all_and_commit("initial parent").unwrap();

    let subrepo_path = parent_path.join("subrepo");
    let mut subrepo = TestRepo::new_at_path(&subrepo_path);

    // Enable prompt sharing for the subrepo
    subrepo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let parent_root = parent.canonical_path();
    let subrepo_root = subrepo.canonical_path();

    let src_dir = subrepo_root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let target_file = src_dir.join("main.ts");
    fs::write(&target_file, "console.log('hello');\n").unwrap();
    subrepo.stage_all_and_commit("initial subrepo").unwrap();

    // Create transcript file
    let transcript_path = subrepo_root.join("claude-session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();

    // Build Claude PostToolUse hook input with CWD = parent repo
    let hook_input = json!({
        "cwd": parent_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Simulate AI edit in subrepo
    fs::write(
        &target_file,
        "console.log('hello');\nconsole.log('AI was here');\n",
    )
    .unwrap();

    // Run checkpoint from parent repo's CWD with Claude preset
    subrepo
        .git_ai_from_working_dir(
            &parent_root,
            &["checkpoint", "claude", "--hook-input", &hook_input],
        )
        .expect("Claude checkpoint from parent CWD for nested subrepo should succeed");

    // Verify working log in subrepo
    let working_log = subrepo.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Scenario 8: Working log entries should exist in the nested subrepo \
         when Claude checkpoint is run from the parent repo's CWD."
    );

    // Commit in subrepo
    let commit = subrepo.stage_all_and_commit("add AI changes").unwrap();

    // Sessions must NOT be empty
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Scenario 8: Sessions should not be empty in nested subrepo's git note \
         when Claude Code is started in parent repo but edits files in subrepo."
    );

    // Attestations must be present
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Scenario 8: AI attestations should be present in the nested subrepo."
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// Claude PreToolUse + PostToolUse cycle for nested subrepo.
#[test]
fn test_claude_preset_nested_subrepo_pre_post_cycle() {
    let workspace = create_unique_workspace("git-ai-claude-nested-cycle-test");

    let parent_path = workspace.join("parent-repo");
    let parent = TestRepo::new_at_path(&parent_path);
    fs::write(parent_path.join("README.md"), "# Parent\n").unwrap();
    parent.stage_all_and_commit("initial parent").unwrap();

    let subrepo_path = parent_path.join("subrepo");
    let subrepo = TestRepo::new_at_path(&subrepo_path);

    let parent_root = parent.canonical_path();
    let subrepo_root = subrepo.canonical_path();

    let target_file = subrepo_root.join("feature.txt");
    fs::write(&target_file, "line 1\nline 2\n").unwrap();
    subrepo.stage_all_and_commit("initial subrepo").unwrap();

    let transcript_path = subrepo_root.join("claude-session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();

    // PreToolUse (human checkpoint before AI edit)
    let pre_hook_input = json!({
        "cwd": parent_root.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    subrepo
        .git_ai_from_working_dir(
            &parent_root,
            &["checkpoint", "claude", "--hook-input", &pre_hook_input],
        )
        .expect("PreToolUse checkpoint for nested subrepo should succeed");

    // Simulate AI edit
    fs::write(&target_file, "line 1\nline 2\nAI line 3\n").unwrap();

    // PostToolUse (AI checkpoint after edit)
    let post_hook_input = json!({
        "cwd": parent_root.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    subrepo
        .git_ai_from_working_dir(
            &parent_root,
            &["checkpoint", "claude", "--hook-input", &post_hook_input],
        )
        .expect("PostToolUse checkpoint for nested subrepo should succeed");

    // Commit and verify
    let commit = subrepo.stage_all_and_commit("add AI changes").unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Scenario 8 (cycle): AI attestations should be present in nested subrepo \
         after PreToolUse + PostToolUse cycle from parent repo's CWD."
    );

    let _ = fs::remove_dir_all(&workspace);
}

// ===========================================================================
// Issue #954 regression: CWD is a completely unrelated non-git directory
//
// When Claude (or any agent) is launched from a directory that is not inside
// any git repository (e.g. /tmp, ~/), all AI attribution was dropped because
// the workspace boundary check in find_repository_for_file rejected files
// that were not children of the CWD directory.
// ===========================================================================

/// Regression test for issue #954:
/// When CWD is a non-git directory completely unrelated to the target repo
/// (not a parent/ancestor of the target), checkpoint must still write to the
/// target repo's working logs so that the commit carries AI attribution.
#[test]
fn test_issue_954_non_git_cwd_unrelated_to_target_repo_mock_ai() {
    // Create a non-git workspace directory (simulates launching from /tmp or ~/)
    // This directory is a SIBLING of the target repo, not its parent.
    let cwd_workspace = create_unique_workspace("git-ai-954-non-git-cwd");

    // Target repo lives in a completely separate location (also under /tmp but as a sibling)
    let repo_target = TestRepo::new();

    // Set up initial commit in target repo
    let mut readme = repo_target.filename("README.md");
    readme.set_contents(crate::lines!["# Target Repo"]);
    repo_target.stage_all_and_commit("initial commit").unwrap();

    // Write AI content to the target repo
    fs::write(
        repo_target.path().join("auth.ts"),
        "export function verifyJwt(token: string): boolean {\n  return true;\n}\n",
    )
    .unwrap();

    let target_file = repo_target.canonical_path().join("auth.ts");

    // Checkpoint from the unrelated non-git CWD with an absolute path to the file.
    // Before the fix, this failed because the workspace boundary was set to cwd_workspace,
    // and find_repository_for_file refused to search outside that boundary.
    repo_target
        .git_ai_from_working_dir(
            &cwd_workspace,
            &["checkpoint", "mock_ai", target_file.to_str().unwrap()],
        )
        .expect("checkpoint from non-git CWD unrelated to target repo should succeed");

    // Verify the working log was written in the target repo before committing
    let working_log = repo_target.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Issue #954: Working log entries should exist in the target repo \
         when checkpoint is run from a non-git CWD that is unrelated to the target repo. \
         Found no AI-touched files in working log."
    );

    // Commit in the target repo and verify AI attribution
    let commit = repo_target.stage_all_and_commit("add AI jwt auth").unwrap();
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Issue #954: AI attribution should be present when Claude is launched from \
         a non-git directory (e.g. /tmp) and writes files to a separate git repo. \
         Expected non-empty attestations but got none."
    );

    let _ = fs::remove_dir_all(&cwd_workspace);
}

/// Issue #954 variant: non-git CWD, multiple files in multiple separate repos.
/// Verifies that all target repos receive correct attribution.
#[test]
fn test_issue_954_non_git_cwd_multiple_target_repos() {
    let cwd_workspace = create_unique_workspace("git-ai-954-multi-target-cwd");

    let repo_a = TestRepo::new();
    let repo_b = TestRepo::new();

    for (repo, name) in [(&repo_a, "A"), (&repo_b, "B")] {
        let mut readme = repo.filename("README.md");
        readme.set_contents(crate::lines![format!("# Repo {}", name)]);
        repo.stage_all_and_commit("initial commit").unwrap();
    }

    fs::write(repo_a.path().join("feature_a.ts"), "AI code A\n").unwrap();
    fs::write(repo_b.path().join("feature_b.ts"), "AI code B\n").unwrap();

    let file_a = repo_a.canonical_path().join("feature_a.ts");
    let file_b = repo_b.canonical_path().join("feature_b.ts");

    repo_a
        .git_ai_from_working_dir(
            &cwd_workspace,
            &[
                "checkpoint",
                "mock_ai",
                file_a.to_str().unwrap(),
                file_b.to_str().unwrap(),
            ],
        )
        .expect("multi-repo checkpoint from non-git CWD should succeed");

    let commit_a = repo_a.stage_all_and_commit("AI edits in A").unwrap();
    assert!(
        !commit_a.authorship_log.attestations.is_empty(),
        "Issue #954 (multi): repo_a should have AI attestations when checkpoint \
         runs from a non-git CWD."
    );

    let commit_b = repo_b.stage_all_and_commit("AI edits in B").unwrap();
    assert!(
        !commit_b.authorship_log.attestations.is_empty(),
        "Issue #954 (multi): repo_b should have AI attestations when checkpoint \
         runs from a non-git CWD."
    );

    let _ = fs::remove_dir_all(&cwd_workspace);
}

/// Issue #954 variant: Claude preset with non-git CWD and file in a separate repo.
/// Verifies that the Claude PostToolUse checkpoint records prompts correctly.
#[test]
fn test_issue_954_claude_preset_non_git_cwd() {
    let cwd_workspace = create_unique_workspace("git-ai-954-claude-cwd");
    let mut repo_target = TestRepo::new();

    // Enable prompt sharing so we can assert prompts are non-empty
    repo_target.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let target_root = repo_target.canonical_path();

    // Set up initial commit
    let target_file_path = target_root.join("auth.ts");
    fs::write(&target_file_path, "// initial\n").unwrap();
    repo_target.stage_all_and_commit("initial commit").unwrap();

    // Copy transcript fixture
    let transcript_path = target_root.join("claude-session.jsonl");
    let fixture = fixture_path("example-claude-code.jsonl");
    fs::copy(&fixture, &transcript_path).unwrap();

    // Build hook input: cwd is the non-git workspace, file is in repo_target
    let hook_input = json!({
        "cwd": cwd_workspace.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": target_file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Simulate AI edit
    fs::write(
        &target_file_path,
        "// initial\nexport function verifyJwt(): boolean { return true; }\n",
    )
    .unwrap();

    // Run checkpoint from non-git CWD with Claude preset
    repo_target
        .git_ai_from_working_dir(
            &cwd_workspace,
            &["checkpoint", "claude", "--hook-input", &hook_input],
        )
        .expect("Claude checkpoint from non-git CWD should succeed");

    // Verify the working log was written in the target repo
    let working_log = repo_target.current_working_logs();
    let ai_files = working_log.all_ai_touched_files().unwrap_or_default();
    assert!(
        !ai_files.is_empty(),
        "Issue #954 (claude): Working log entries should exist in target repo \
         when Claude PostToolUse fires from a non-git CWD."
    );

    let commit = repo_target.stage_all_and_commit("AI auth changes").unwrap();
    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Issue #954 (claude): Sessions should not be empty in target repo's git note \
         when Claude is launched from a non-git CWD."
    );
    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Issue #954 (claude): AI attestations should be present when Claude is launched \
         from a non-git CWD."
    );

    let _ = fs::remove_dir_all(&cwd_workspace);
}
