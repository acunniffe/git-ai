use crate::repos::test_repo::TestRepo;
use crate::test_utils::extract_json_object;
use git_ai::authorship::stats::CommitStats;
use serde::Deserialize;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Deserialize)]
struct StatusOutput {
    stats: CommitStats,
    checkpoints: Vec<serde_json::Value>,
}

fn status_from_args(repo: &TestRepo, args: &[&str]) -> StatusOutput {
    let raw = repo.git_ai(args).expect("git-ai status should succeed");
    let json = extract_json_object(&raw);
    serde_json::from_str(&json).expect("valid status json")
}

fn write_file(repo: &TestRepo, path: &str, contents: &str) {
    let abs_path = repo.path().join(path);
    if let Some(parent) = abs_path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory should be creatable");
    }
    std::fs::write(abs_path, contents).expect("file write should succeed");
}

fn configure_repo_external_diff_helper(repo: &TestRepo) -> String {
    let marker = "STATUS_EXTERNAL_DIFF_MARKER";
    let helper_path = repo.path().join("status-ext-diff-helper.sh");
    let helper_path_posix = helper_path
        .to_str()
        .expect("helper path must be valid UTF-8")
        .replace('\\', "/");

    fs::write(&helper_path, format!("#!/bin/sh\necho {marker}\nexit 0\n"))
        .expect("should write external diff helper");
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&helper_path)
            .expect("helper metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper_path, perms).expect("helper should be executable");
    }

    repo.git_og(&["config", "diff.external", &helper_path_posix])
        .expect("configuring diff.external should succeed");

    marker.to_string()
}

fn configure_hostile_diff_settings(repo: &TestRepo) {
    let settings = [
        ("diff.noprefix", "true"),
        ("diff.mnemonicprefix", "true"),
        ("diff.srcPrefix", "SRC/"),
        ("diff.dstPrefix", "DST/"),
        ("diff.renames", "copies"),
        ("diff.relative", "true"),
        ("diff.algorithm", "histogram"),
        ("diff.indentHeuristic", "false"),
        ("diff.interHunkContext", "8"),
        ("color.diff", "always"),
        ("color.ui", "always"),
    ];
    for (key, value) in settings {
        repo.git_og(&["config", key, value])
            .unwrap_or_else(|err| panic!("setting {key}={value} should succeed: {err}"));
    }
}

#[test]
fn test_checkpoint_ignores_default_lockfiles_integration() {
    let repo = TestRepo::new();

    write_file(&repo, "README.md", "# repo\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "README.md", "# repo\nupdated\n");
    write_file(&repo, "Cargo.lock", "# lock\n# lock2\n# lock3\n");

    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let latest = checkpoints.last().expect("checkpoint should be present");

    assert!(
        latest.entries.iter().any(|entry| entry.file == "README.md"),
        "Expected non-ignored file to be checkpointed"
    );
    assert!(
        latest
            .entries
            .iter()
            .all(|entry| entry.file != "Cargo.lock"),
        "Expected Cargo.lock to be filtered out by default ignores"
    );
}

#[test]
fn test_checkpoint_honors_uncommitted_root_gitattributes_linguist_generated_integration() {
    let repo = TestRepo::new();

    write_file(&repo, "src/main.rs", "fn main() {}\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(
        &repo,
        ".gitattributes",
        "generated/** linguist-generated=true\n",
    );
    write_file(&repo, "src/main.rs", "fn main() {}\nfn added() {}\n");
    write_file(
        &repo,
        "generated/api.generated.ts",
        "export const one = 1;\nexport const two = 2;\n",
    );

    repo.git_ai(&[
        "checkpoint",
        "mock_ai",
        "src/main.rs",
        "generated/api.generated.ts",
    ])
    .unwrap();

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let latest = checkpoints.last().expect("checkpoint should be present");

    assert!(
        latest
            .entries
            .iter()
            .any(|entry| entry.file == "src/main.rs"),
        "Expected regular source file to be checkpointed"
    );
    assert!(
        latest
            .entries
            .iter()
            .all(|entry| entry.file != "generated/api.generated.ts"),
        "Expected linguist-generated file to be filtered out"
    );
}

#[test]
fn test_status_default_ignores_affect_git_diff_and_ai_accepted() {
    let repo = TestRepo::new();

    write_file(&repo, "README.md", "# repo\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "README.md", "# repo\nnew ai line\n");
    write_file(&repo, "Cargo.lock", "# lock\n# lock2\n# lock3\n");

    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let status = status_from_args(&repo, &["status", "--json"]);

    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
    assert!(
        !status.checkpoints.is_empty(),
        "status should report at least one checkpoint"
    );
}

#[test]
fn test_status_honors_uncommitted_root_gitattributes_linguist_generated() {
    let repo = TestRepo::new();

    write_file(&repo, "src/app.ts", "export const app = 1;\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(
        &repo,
        ".gitattributes",
        "generated/** linguist-generated=true\n",
    );
    write_file(
        &repo,
        "src/app.ts",
        "export const app = 1;\nexport const next = 2;\n",
    );
    write_file(
        &repo,
        "generated/out.generated.ts",
        "export const generatedA = 1;\nexport const generatedB = 2;\n",
    );

    repo.git_ai(&[
        "checkpoint",
        "mock_ai",
        "src/app.ts",
        "generated/out.generated.ts",
    ])
    .unwrap();

    let status = status_from_args(&repo, &["status", "--json"]);

    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
}

#[test]
fn test_status_with_only_ignored_changes_reports_zero_diff() {
    let repo = TestRepo::new();

    write_file(&repo, "README.md", "# repo\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "Cargo.lock", "# lock\n# lock2\n# lock3\n");

    let status = status_from_args(&repo, &["status", "--json"]);

    assert_eq!(status.stats.git_diff_added_lines, 0);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 0);
}

#[test]
fn test_checkpoint_honors_git_ai_ignore_file() {
    let repo = TestRepo::new();

    write_file(&repo, "src/main.rs", "fn main() {}\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, ".git-ai-ignore", "docs/**\n");
    write_file(&repo, "src/main.rs", "fn main() {}\nfn added() {}\n");
    write_file(&repo, "docs/guide.md", "# Guide\nLine 1\nLine 2\n");

    repo.git_ai(&["checkpoint", "mock_ai", "src/main.rs", "docs/guide.md"])
        .unwrap();

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let latest = checkpoints.last().expect("checkpoint should be present");

    assert!(
        latest
            .entries
            .iter()
            .any(|entry| entry.file == "src/main.rs"),
        "Expected regular source file to be checkpointed"
    );
    assert!(
        latest
            .entries
            .iter()
            .all(|entry| entry.file != "docs/guide.md"),
        "Expected .git-ai-ignore pattern to filter out docs/guide.md"
    );
}

#[test]
fn test_status_honors_git_ai_ignore_file() {
    let repo = TestRepo::new();

    write_file(&repo, "src/app.ts", "export const app = 1;\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, ".git-ai-ignore", "docs/**\n");
    write_file(
        &repo,
        "src/app.ts",
        "export const app = 1;\nexport const next = 2;\n",
    );
    write_file(&repo, "docs/api.md", "# API\nendpoint 1\nendpoint 2\n");

    repo.git_ai(&["checkpoint", "mock_ai", "src/app.ts", "docs/api.md"])
        .unwrap();

    let status = status_from_args(&repo, &["status", "--json"]);

    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
}

#[test]
fn test_status_git_ai_ignore_union_with_gitattributes() {
    let repo = TestRepo::new();

    write_file(&repo, "src/app.ts", "export const app = 1;\n");
    repo.stage_all_and_commit("initial").unwrap();

    // Set up both .gitattributes and .git-ai-ignore
    write_file(
        &repo,
        ".gitattributes",
        "generated/** linguist-generated=true\n",
    );
    write_file(&repo, ".git-ai-ignore", "docs/**\n");
    write_file(
        &repo,
        "src/app.ts",
        "export const app = 1;\nexport const next = 2;\n",
    );
    write_file(
        &repo,
        "generated/out.ts",
        "export const gen = 1;\nexport const gen2 = 2;\n",
    );
    write_file(&repo, "docs/api.md", "# API\nendpoint 1\nendpoint 2\n");

    repo.git_ai(&[
        "checkpoint",
        "mock_ai",
        "src/app.ts",
        "generated/out.ts",
        "docs/api.md",
    ])
    .unwrap();

    let status = status_from_args(&repo, &["status", "--json"]);

    // Only src/app.ts addition should be counted (1 line)
    // generated/out.ts ignored by .gitattributes linguist-generated
    // docs/api.md ignored by .git-ai-ignore
    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
}

#[test]
fn test_status_ignores_repo_external_diff_helper_for_internal_numstat() {
    let repo = TestRepo::new();

    write_file(&repo, "app.txt", "line1\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "app.txt", "line1\nline2\n");
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    let marker = configure_repo_external_diff_helper(&repo);
    let proxied_diff = repo
        .git(&["diff", "HEAD"])
        .expect("proxied git diff should succeed");
    assert!(
        proxied_diff.contains(&marker),
        "sanity check: proxied git diff should use configured external helper"
    );

    let status = status_from_args(&repo, &["status", "--json"]);
    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
}

#[test]
fn test_status_numstat_is_stable_under_hostile_diff_config() {
    let repo = TestRepo::new();

    write_file(&repo, "app.txt", "line1\n");
    repo.stage_all_and_commit("initial").unwrap();

    write_file(&repo, "app.txt", "line1\nline2\n");
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
    configure_hostile_diff_settings(&repo);

    let status = status_from_args(&repo, &["status", "--json"]);
    assert_eq!(status.stats.git_diff_added_lines, 1);
    assert_eq!(status.stats.git_diff_deleted_lines, 0);
    assert_eq!(status.stats.ai_accepted, 1);
}
crate::reuse_tests_in_worktree!(
    test_checkpoint_ignores_default_lockfiles_integration,
    test_checkpoint_honors_uncommitted_root_gitattributes_linguist_generated_integration,
    test_status_default_ignores_affect_git_diff_and_ai_accepted,
    test_status_honors_uncommitted_root_gitattributes_linguist_generated,
    test_status_with_only_ignored_changes_reports_zero_diff,
    test_checkpoint_honors_git_ai_ignore_file,
    test_status_honors_git_ai_ignore_file,
    test_status_git_ai_ignore_union_with_gitattributes,
    test_status_ignores_repo_external_diff_helper_for_internal_numstat,
    test_status_numstat_is_stable_under_hostile_diff_config,
);
