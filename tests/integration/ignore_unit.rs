use crate::repos::test_repo::TestRepo;
use git_ai::authorship::ignore::{
    build_ignore_matcher, effective_ignore_patterns, load_git_ai_ignore_patterns,
    load_linguist_generated_patterns_from_root_gitattributes,
    load_linguist_vendored_patterns_from_root_gitattributes, should_ignore_file_with_matcher,
};
use git_ai::git::repository::from_bare_repository;
use std::fs;
use std::path::Path;
use std::process::Command;

// Helper for bare repo tests
fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn make_bare_repo(
    root_gitattributes: Option<&str>,
    parent_gitattributes: Option<&str>,
) -> (tempfile::TempDir, git_ai::git::repository::Repository) {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("bare.git");
    fs::create_dir_all(&source).expect("create source");

    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.name", "Test User"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);

    fs::write(source.join("README.md"), "# repo\n").expect("write readme");
    if let Some(attrs) = root_gitattributes {
        fs::write(source.join(".gitattributes"), attrs).expect("write attrs");
    }

    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "initial"]);
    run_git(
        temp.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );

    if let Some(parent_attrs) = parent_gitattributes {
        fs::write(temp.path().join(".gitattributes"), parent_attrs).expect("write parent attrs");
    }

    (
        temp,
        from_bare_repository(&bare).expect("bare repository should load"),
    )
}

fn make_bare_repo_with_ignore(
    root_gitattributes: Option<&str>,
    git_ai_ignore: Option<&str>,
) -> (tempfile::TempDir, git_ai::git::repository::Repository) {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("bare.git");
    fs::create_dir_all(&source).expect("create source");

    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.name", "Test User"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);

    fs::write(source.join("README.md"), "# repo\n").expect("write readme");
    if let Some(attrs) = root_gitattributes {
        fs::write(source.join(".gitattributes"), attrs).expect("write attrs");
    }
    if let Some(ignore) = git_ai_ignore {
        fs::write(source.join(".git-ai-ignore"), ignore).expect("write .git-ai-ignore");
    }

    run_git(&source, &["add", "."]);
    run_git(&source, &["commit", "-m", "initial"]);
    run_git(
        temp.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );

    (
        temp,
        from_bare_repository(&bare).expect("bare repository should load"),
    )
}

// TestRepo tests (converted from TmpRepo)

#[test]
fn loads_positive_linguist_generated_only() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "\
*.generated.ts linguist-generated=true
dist/** linguist-generated
vendor/** -linguist-generated
manual/** linguist-generated=false
flags/** linguist-generated=1
other/** linguist-generated=0
generated\\ files/** linguist-generated=true
",
    )
    .unwrap();
    repo.git(&["add", ".gitattributes"]).unwrap();
    repo.stage_all_and_commit("add gitattributes").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_linguist_generated_patterns_from_root_gitattributes(&gitai_repo);
    assert!(patterns.contains(&"*.generated.ts".to_string()));
    assert!(patterns.contains(&"dist/**".to_string()));
    assert!(patterns.contains(&"flags/**".to_string()));
    assert!(patterns.contains(&"generated files/**".to_string()));
    assert!(!patterns.contains(&"vendor/**".to_string()));
    assert!(!patterns.contains(&"manual/**".to_string()));
    assert!(!patterns.contains(&"other/**".to_string()));
}

#[test]
fn ignores_gitattributes_macro_definitions() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "\
[attr]generated linguist-generated=true
generated/** linguist-generated=true
",
    )
    .unwrap();
    repo.git(&["add", ".gitattributes"]).unwrap();
    repo.stage_all_and_commit("add gitattributes").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_linguist_generated_patterns_from_root_gitattributes(&gitai_repo);

    assert!(patterns.contains(&"generated/**".to_string()));
    assert!(!patterns.contains(&"[attr]generated".to_string()));
}

#[test]
fn loads_git_ai_ignore_patterns_from_workdir() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".git-ai-ignore"),
        "\
# This is a comment
docs/**
*.pdf

assets/images/**
",
    )
    .unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_git_ai_ignore_patterns(&gitai_repo);
    assert_eq!(patterns.len(), 3);
    assert!(patterns.contains(&"docs/**".to_string()));
    assert!(patterns.contains(&"*.pdf".to_string()));
    assert!(patterns.contains(&"assets/images/**".to_string()));
}

#[test]
fn git_ai_ignore_skips_comments_and_blank_lines() {
    let repo = TestRepo::new();
    // Use explicit \n to preserve trailing whitespace on the "  *.log  " line
    std::fs::write(
        repo.path().join(".git-ai-ignore"),
        "# comment line\n   # indented comment\n\n  *.log  \nbuild/**\n",
    )
    .unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_git_ai_ignore_patterns(&gitai_repo);
    assert_eq!(patterns.len(), 2);
    assert!(patterns.contains(&"*.log".to_string()));
    assert!(patterns.contains(&"build/**".to_string()));
}

#[test]
fn git_ai_ignore_deduplicates_patterns() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".git-ai-ignore"),
        "\
*.pdf
docs/**
*.pdf
",
    )
    .unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_git_ai_ignore_patterns(&gitai_repo);
    assert_eq!(patterns.len(), 2);
}

#[test]
fn git_ai_ignore_returns_empty_when_file_missing() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join("README.md"), "# repo\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = load_git_ai_ignore_patterns(&gitai_repo);
    assert!(patterns.is_empty());
}

#[test]
fn effective_patterns_include_git_ai_ignore() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join(".git-ai-ignore"), "custom/**\n*.secret\n").unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = effective_ignore_patterns(&gitai_repo, &[], &[]);
    assert!(patterns.contains(&"custom/**".to_string()));
    assert!(patterns.contains(&"*.secret".to_string()));
    // Default patterns should still be present
    assert!(patterns.contains(&"*.lock".to_string()));
}

#[test]
fn effective_patterns_union_gitattributes_and_git_ai_ignore() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "generated/** linguist-generated=true\n",
    )
    .unwrap();
    std::fs::write(repo.path().join(".git-ai-ignore"), "docs/**\n").unwrap();
    repo.git(&["add", ".gitattributes", ".git-ai-ignore"])
        .unwrap();
    repo.stage_all_and_commit("add gitattributes and git-ai-ignore")
        .unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = effective_ignore_patterns(&gitai_repo, &[], &[]);
    // From .gitattributes linguist-generated
    assert!(patterns.contains(&"generated/**".to_string()));
    // From .git-ai-ignore
    assert!(patterns.contains(&"docs/**".to_string()));
    // Defaults
    assert!(patterns.contains(&"*.lock".to_string()));
}

#[test]
fn effective_patterns_union_git_ai_ignore_and_user_patterns() {
    let repo = TestRepo::new();
    std::fs::write(repo.path().join(".git-ai-ignore"), "docs/**\n").unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let user = vec!["tests/**".to_string()];
    let patterns = effective_ignore_patterns(&gitai_repo, &user, &[]);
    // From .git-ai-ignore
    assert!(patterns.contains(&"docs/**".to_string()));
    // From user --ignore flag
    assert!(patterns.contains(&"tests/**".to_string()));
    // Defaults
    assert!(patterns.contains(&"*.lock".to_string()));
}

// Regression tests for issue #1664: a first-party package named `vendor` was
// silently excluded by the built-in `**/vendor/**` default with no override.

#[test]
fn git_ai_ignore_negation_reincludes_first_party_vendor_package() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".git-ai-ignore"),
        "!src/main/java/com/acme/vendor/**\n",
    )
    .unwrap();
    repo.git(&["add", ".git-ai-ignore"]).unwrap();
    repo.stage_all_and_commit("add .git-ai-ignore").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let patterns = effective_ignore_patterns(&gitai_repo, &[], &[]);
    // The negation flows through the effective pattern set.
    assert!(patterns.contains(&"!src/main/java/com/acme/vendor/**".to_string()));

    let matcher = build_ignore_matcher(&patterns);
    // First-party `vendor` package is re-included (was 0% AI before the fix).
    assert!(!should_ignore_file_with_matcher(
        "src/main/java/com/acme/vendor/Probe.java",
        &matcher
    ));
    // A genuine third-party vendor directory elsewhere stays excluded.
    assert!(should_ignore_file_with_matcher(
        "third_party/vendor/lib.js",
        &matcher
    ));
}

#[test]
fn gitattributes_linguist_vendored_false_reincludes_first_party_package() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "src/main/java/com/acme/vendor/** -linguist-vendored\n",
    )
    .unwrap();
    repo.git(&["add", ".gitattributes"]).unwrap();
    repo.stage_all_and_commit("add gitattributes").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let vendored = load_linguist_vendored_patterns_from_root_gitattributes(&gitai_repo);
    assert!(vendored.contains(&"!src/main/java/com/acme/vendor/**".to_string()));

    let patterns = effective_ignore_patterns(&gitai_repo, &[], &[]);
    let matcher = build_ignore_matcher(&patterns);
    assert!(!should_ignore_file_with_matcher(
        "src/main/java/com/acme/vendor/Probe.java",
        &matcher
    ));
}

#[test]
fn gitattributes_linguist_vendored_true_excludes_path() {
    let repo = TestRepo::new();
    std::fs::write(
        repo.path().join(".gitattributes"),
        "tools/bundled/** linguist-vendored=true\n",
    )
    .unwrap();
    repo.git(&["add", ".gitattributes"]).unwrap();
    repo.stage_all_and_commit("add gitattributes").unwrap();

    let gitai_repo =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let vendored = load_linguist_vendored_patterns_from_root_gitattributes(&gitai_repo);
    assert!(vendored.contains(&"tools/bundled/**".to_string()));

    let patterns = effective_ignore_patterns(&gitai_repo, &[], &[]);
    let matcher = build_ignore_matcher(&patterns);
    assert!(should_ignore_file_with_matcher(
        "tools/bundled/helper.js",
        &matcher
    ));
}

// Bare repo tests (using make_bare_repo helpers)

#[test]
fn loads_linguist_generated_from_bare_repo_head() {
    let (_tmp, bare_repo) = make_bare_repo(
        Some("generated/** linguist-generated=true\nmanual/** linguist-generated=false\n"),
        None,
    );

    let patterns = load_linguist_generated_patterns_from_root_gitattributes(&bare_repo);
    assert!(patterns.contains(&"generated/**".to_string()));
    assert!(!patterns.contains(&"manual/**".to_string()));
}

#[test]
fn bare_repo_does_not_read_parent_directory_gitattributes() {
    let (_tmp, bare_repo) = make_bare_repo(None, Some("leak/** linguist-generated=true\n"));

    let patterns = load_linguist_generated_patterns_from_root_gitattributes(&bare_repo);
    assert!(patterns.is_empty());
}

#[test]
fn loads_git_ai_ignore_from_bare_repo_head() {
    let (_tmp, bare_repo) = make_bare_repo_with_ignore(None, Some("docs/**\n*.pdf\n"));

    let patterns = load_git_ai_ignore_patterns(&bare_repo);
    assert!(patterns.contains(&"docs/**".to_string()));
    assert!(patterns.contains(&"*.pdf".to_string()));
}

#[test]
fn bare_repo_returns_empty_when_git_ai_ignore_missing() {
    let (_tmp, bare_repo) = make_bare_repo_with_ignore(None, None);

    let patterns = load_git_ai_ignore_patterns(&bare_repo);
    assert!(patterns.is_empty());
}

#[test]
fn bare_repo_effective_patterns_union_gitattributes_and_git_ai_ignore() {
    let (_tmp, bare_repo) = make_bare_repo_with_ignore(
        Some("generated/** linguist-generated=true\n"),
        Some("docs/**\n"),
    );

    let patterns = effective_ignore_patterns(&bare_repo, &[], &[]);
    assert!(patterns.contains(&"generated/**".to_string()));
    assert!(patterns.contains(&"docs/**".to_string()));
    assert!(patterns.contains(&"*.lock".to_string()));
}

crate::reuse_tests_in_worktree!(
    // TestRepo tests
    loads_positive_linguist_generated_only,
    ignores_gitattributes_macro_definitions,
    loads_git_ai_ignore_patterns_from_workdir,
    git_ai_ignore_skips_comments_and_blank_lines,
    git_ai_ignore_deduplicates_patterns,
    git_ai_ignore_returns_empty_when_file_missing,
    effective_patterns_include_git_ai_ignore,
    effective_patterns_union_gitattributes_and_git_ai_ignore,
    effective_patterns_union_git_ai_ignore_and_user_patterns,
    // Issue #1664 regression tests
    git_ai_ignore_negation_reincludes_first_party_vendor_package,
    gitattributes_linguist_vendored_false_reincludes_first_party_package,
    gitattributes_linguist_vendored_true_excludes_path,
    // Bare repo tests
    loads_linguist_generated_from_bare_repo_head,
    bare_repo_does_not_read_parent_directory_gitattributes,
    loads_git_ai_ignore_from_bare_repo_head,
    bare_repo_returns_empty_when_git_ai_ignore_missing,
    bare_repo_effective_patterns_union_gitattributes_and_git_ai_ignore,
);
