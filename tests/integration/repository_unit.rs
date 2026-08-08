use git_ai::operations::git::repository::{
    find_repository_in_path, parse_diff_added_lines_with_insertions, resolve_command_base_dir,
    worktree_storage_ai_dir,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repos::test_repo::TestRepo;
use crate::test_utils::run_git_stdout;
use std::sync::OnceLock;

fn init_test_git_config() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let path = std::env::temp_dir().join("git-ai-test-global-gitconfig");
        let _ = fs::write(
            &path,
            "[user]\n\tname = Test User\n\temail = test@example.com\n",
        );
        #[cfg(not(windows))]
        let canonical = path.canonicalize().unwrap_or(path);
        #[cfg(windows)]
        let canonical = path;
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &canonical);
            #[cfg(not(windows))]
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
    });
}

/// Unlike the shared `raw_git`, this suite resolves the real git binary via
/// `Config::get().git_cmd()` (fork-bomb-safe when a git-ai shim shadows `git`
/// in PATH) and points at an isolated global config first.
fn run_git(cwd: &Path, args: &[&str]) {
    init_test_git_config();
    let output = Command::new(git_ai::config::Config::get().git_cmd())
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

#[test]
fn test_list_commit_files_with_utf8_filename() {
    // Create a test repo with a UTF-8 filename
    let repo = TestRepo::new();

    // Write a file with Chinese characters in its name
    let chinese_filename = "中文文件.txt";
    std::fs::write(repo.path().join(chinese_filename), "Hello, 世界!\n")
        .expect("write Chinese file");

    // Stage and commit
    repo.stage_all_and_commit("Add Chinese file")
        .expect("commit Chinese file");

    // Get the head SHA
    let sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Get a gitai Repository
    let repo_handle =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("find repository");

    // Test list_commit_files
    let files = repo_handle
        .list_commit_files(&sha, None)
        .expect("list commit files");

    // Debug: print what we got
    println!("Files in commit: {:?}", files);

    // The file should be in the list with its UTF-8 name
    assert!(
        files.contains(chinese_filename),
        "Should contain the Chinese filename '{}', but got: {:?}",
        chinese_filename,
        files
    );
}

macro_rules! pure_insertion_path_tests {
    ($( $name:ident: ($old:literal, $new:literal, $index:literal, $count:literal, [$first_added:literal $(, $added:literal)*]) => ($path:literal, [$($line:literal),+]); )+) => {$(
        #[test]
        fn $name() {
            let diff = concat!(
                "diff --git ", $old, " ", $new, "\nindex ", $index, " 100644\n--- ",
                $old, "\n+++ ", $new, "\n@@ -0,0 +1,", $count, " @@\n+", $first_added
                $(, "\n+", $added)*
            );
            let (added_lines, insertion_lines) =
                parse_diff_added_lines_with_insertions(diff).unwrap();
            assert_eq!(added_lines.get($path), Some(&vec![$($line),+]));
            assert_eq!(insertion_lines.get($path), Some(&vec![$($line),+]));
        }
    )+};
}

pure_insertion_path_tests! {
    test_parse_diff_added_lines_with_insertions_standard_prefix: ("a/test.txt", "b/test.txt", "0000000..abc1234", "2", ["line 1", "line 2"]) => ("test.txt", [1, 2]);
    test_parse_diff_added_lines_with_insertions_workdir_prefix: ("c/test.txt", "w/test.txt", "a751413..8adaa6c", "2", ["// AI added line 1", "// AI added line 2"]) => ("test.txt", [1, 2]);
    test_parse_diff_added_lines_with_insertions_quoted_paths: ("\"a/my file.txt\"", "\"b/my file.txt\"", "0000000..abc1234", "3", ["line 1", "line 2", "line 3"]) => ("my file.txt", [1, 2, 3]);
    test_parse_diff_added_lines_with_insertions_quoted_workdir_paths: ("\"c/my file.txt\"", "\"w/my file.txt\"", "0000000..abc1234", "2", ["line 1", "line 2"]) => ("my file.txt", [1, 2]);
    test_parse_diff_added_lines_with_insertions_no_prefix_paths: ("my-file.txt", "my-file.txt", "0000000..abc1234", "2", ["line 1", "line 2"]) => ("my-file.txt", [1, 2]);
    test_parse_diff_added_lines_with_insertions_custom_prefix_paths: ("SRC/my-file.txt", "DST/my-file.txt", "0000000..abc1234", "2", ["line 1", "line 2"]) => ("DST/my-file.txt", [1, 2]);
}

#[test]
fn test_parse_diff_added_lines_with_insertions_replacement_uses_plus_lines_only() {
    let diff = r#"diff --git a/test.txt b/test.txt
index 0000000..abc1234 100644
--- a/test.txt
+++ b/test.txt
@@ -10,3 +10,4 @@
 unchanged
-old line
+new line
 context
+added tail"#;

    let (added_lines, insertion_lines) = parse_diff_added_lines_with_insertions(diff).unwrap();
    assert_eq!(added_lines.get("test.txt"), Some(&vec![11, 13]));
    assert_eq!(insertion_lines.get("test.txt"), None);
}

#[test]
fn worktree_storage_ai_dir_keeps_full_relative_worktree_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let common_dir = temp.path().join("repo.git");
    let linked_git_dir = common_dir.join("worktrees").join("feature").join("nested");

    fs::create_dir_all(&linked_git_dir).expect("create linked git dir");

    let ai_dir = worktree_storage_ai_dir(&linked_git_dir, &common_dir);
    assert_eq!(
        ai_dir,
        common_dir
            .join("ai")
            .join("worktrees")
            .join("feature")
            .join("nested")
    );
}

#[test]
fn worktree_storage_ai_dir_fallback_uses_git_dir_leaf_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let common_dir = temp.path().join("repo.git");
    let detached_git_dir = temp.path().join("somewhere").join("linked-worktree");

    fs::create_dir_all(&common_dir).expect("create common dir");
    fs::create_dir_all(&detached_git_dir).expect("create detached git dir");

    let ai_dir = worktree_storage_ai_dir(&detached_git_dir, &common_dir);
    assert_eq!(
        ai_dir,
        common_dir
            .join("ai")
            .join("worktrees")
            .join("linked-worktree")
    );
}

#[test]
fn resolve_command_base_dir_applies_chained_c_arguments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = temp.path().join("root");
    let args = vec![
        "-C".to_string(),
        base.to_string_lossy().to_string(),
        "-C".to_string(),
        "nested".to_string(),
        "-C".to_string(),
        "..".to_string(),
        "-C".to_string(),
        "repo".to_string(),
        "status".to_string(),
    ];

    let resolved = resolve_command_base_dir(&args).expect("resolve base dir");
    assert_eq!(resolved, base.join("nested").join("..").join("repo"));
}

#[test]
fn find_repository_in_path_supports_bare_repositories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("repo.git");
    fs::create_dir_all(&source).expect("create source");

    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.name", "Test User"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);
    fs::write(source.join("README.md"), "# repo\n").expect("write readme");
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

    let repo = find_repository_in_path(bare.to_str().unwrap()).expect("find bare repo");
    assert!(repo.is_bare_repository().expect("bare check"));
    assert_eq!(
        repo.path().canonicalize().expect("canonical bare"),
        bare.canonicalize().expect("canonical path")
    );

    let discovered =
        git_ai::operations::git::repository::discover_repository_in_path_no_git_exec(&bare)
            .expect("discover bare repo");
    assert_eq!(
        discovered.path().canonicalize().expect("canonical bare"),
        bare.canonicalize().expect("canonical path")
    );
}

#[test]
fn find_repository_in_path_bare_repo_can_read_head_gitattributes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("repo.git");
    fs::create_dir_all(&source).expect("create source");

    run_git(&source, &["init"]);
    run_git(&source, &["config", "user.name", "Test User"]);
    run_git(&source, &["config", "user.email", "test@example.com"]);
    fs::write(
        source.join(".gitattributes"),
        "generated/** linguist-generated=true\n",
    )
    .expect("write attrs");
    fs::write(source.join("README.md"), "# repo\n").expect("write readme");
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

    let repo = find_repository_in_path(bare.to_str().unwrap()).expect("find bare repo");
    let content = repo
        .get_file_content(".gitattributes", "HEAD")
        .expect("read attrs from HEAD");
    let content = String::from_utf8(content).expect("utf8 attrs");
    assert!(content.contains("generated/** linguist-generated=true"));
}

#[test]
fn find_repository_in_path_worktree_uses_common_dir_for_isolated_storage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main_repo = temp.path().join("main");
    let worktree = temp.path().join("linked");

    fs::create_dir_all(&main_repo).expect("create main repo dir");
    run_git(&main_repo, &["init"]);
    run_git(&main_repo, &["config", "user.name", "Test User"]);
    run_git(&main_repo, &["config", "user.email", "test@example.com"]);
    run_git(&main_repo, &["worktree", "add", worktree.to_str().unwrap()]);

    let repo = find_repository_in_path(worktree.to_str().unwrap()).expect("find worktree repo");
    let common_dir = PathBuf::from(run_git_stdout(
        &worktree,
        &["rev-parse", "--git-common-dir"],
    ));

    assert_eq!(
        repo.common_dir()
            .canonicalize()
            .expect("canonical common dir"),
        common_dir
            .canonicalize()
            .expect("canonical expected common dir")
    );
    assert!(
        repo.storage
            .working_logs
            .starts_with(common_dir.join("ai").join("worktrees")),
        "worktree storage should be isolated under common-dir/ai/worktrees: {}",
        repo.storage.working_logs.display()
    );

    let discovered =
        git_ai::operations::git::repository::discover_repository_in_path_no_git_exec(&worktree)
            .expect("discover worktree repo");
    assert_eq!(
        discovered
            .common_dir()
            .canonicalize()
            .expect("canonical discovered common dir"),
        common_dir
            .canonicalize()
            .expect("canonical expected common dir")
    );
    assert!(
        discovered
            .storage
            .working_logs
            .starts_with(common_dir.join("ai").join("worktrees")),
        "discovered worktree storage should be isolated under common-dir/ai/worktrees: {}",
        discovered.storage.working_logs.display()
    );
}

#[test]
fn path_is_in_workdir_returns_false_for_linked_worktree_file() {
    // Sibling worktree: the worktree lives OUTSIDE the main repo's working tree.
    // path_is_in_workdir returns false purely because the path doesn't
    // start_with(workdir) — no .git file inspection is needed.  This test
    // passes even without the is_linked_worktree_git_file fix.
    let temp = tempfile::tempdir().expect("tempdir");
    let main_repo = temp.path().join("main");
    let worktree = temp.path().join("linked");

    fs::create_dir_all(&main_repo).expect("create main repo dir");
    run_git(&main_repo, &["init"]);
    run_git(&main_repo, &["config", "user.name", "Test User"]);
    run_git(&main_repo, &["config", "user.email", "test@example.com"]);
    // Write a file so the sanity-check path exists on disk — path_is_in_workdir
    // calls path.canonicalize() which only resolves symlinks for existing paths
    // (on macOS /var/... is a symlink to /private/var/...; on Windows temp paths
    // may use short names that differ from the canonical workdir stored by git).
    fs::write(main_repo.join("README.md"), "# main\n").expect("write README");
    run_git(&main_repo, &["worktree", "add", worktree.to_str().unwrap()]);

    let dot_git = worktree.join(".git");
    assert!(
        dot_git.is_file(),
        ".git should be a file in a linked worktree"
    );

    let main = find_repository_in_path(main_repo.to_str().unwrap()).expect("find main repo");

    let wt_file = worktree.join("somefile.rs");
    assert!(
        !main.path_is_in_workdir(&wt_file),
        "sibling linked worktree file should not be in main repo workdir"
    );

    // Use an existing file so path.canonicalize() resolves symlinks correctly.
    let main_file = main_repo.join("README.md");
    assert!(
        main.path_is_in_workdir(&main_file),
        "main repo file should be in main repo workdir"
    );
}

#[test]
fn path_is_in_workdir_returns_false_for_nested_linked_worktree_file() {
    // Nested worktree: the worktree lives INSIDE the main repo's working tree
    // (e.g. main_repo/.worktrees/feature).  This is the exact Bug-A / Bug-B
    // scenario: path starts_with(workdir) so the starts_with check passes,
    // and only is_linked_worktree_git_file makes path_is_in_workdir return
    // false.  This test FAILS without the fix.
    let temp = tempfile::tempdir().expect("tempdir");
    let main_repo = temp.path().join("main");
    let worktree = main_repo.join(".worktrees").join("feature");

    fs::create_dir_all(&main_repo).expect("create main repo dir");
    run_git(&main_repo, &["init"]);
    run_git(&main_repo, &["config", "user.name", "Test User"]);
    run_git(&main_repo, &["config", "user.email", "test@example.com"]);
    // git worktree add requires at least one commit
    fs::write(main_repo.join("README.md"), "# test\n").expect("write README");
    run_git(&main_repo, &["add", "."]);
    run_git(&main_repo, &["commit", "-m", "initial"]);
    run_git(
        &main_repo,
        &["worktree", "add", "--detach", worktree.to_str().unwrap()],
    );

    let dot_git = worktree.join(".git");
    assert!(
        dot_git.is_file(),
        ".git should be a file in a nested worktree"
    );
    let gitfile_content = fs::read_to_string(&dot_git).expect("read .git file");
    assert!(
        gitfile_content.contains("/worktrees/"),
        ".git file should reference /worktrees/: {}",
        gitfile_content.trim()
    );

    let main = find_repository_in_path(main_repo.to_str().unwrap()).expect("find main repo");

    // The nested worktree file is physically under main_repo/ but must NOT
    // be reported as part of the main repo's working tree.
    let wt_file = worktree.join("somefile.rs");
    assert!(
        !main.path_is_in_workdir(&wt_file),
        "nested linked worktree file should not be in main repo workdir \
         (path starts_with workdir, but .git file marks a repo boundary)"
    );

    // Sanity: file is in the worktree's own workdir.
    let wt_repo =
        find_repository_in_path(worktree.to_str().unwrap()).expect("find nested worktree");
    assert!(
        wt_repo.path_is_in_workdir(&wt_file),
        "nested worktree file should be in the worktree's own workdir"
    );

    // Sanity: a normal file in the main repo is still in the main workdir.
    // Use README.md which already exists so path.canonicalize() resolves
    // symlinks correctly (macOS /var/... → /private/var/...; Windows short names).
    let main_file = main_repo.join("README.md");
    assert!(
        main.path_is_in_workdir(&main_file),
        "main repo file should be in main repo workdir"
    );
}

#[test]
fn get_all_staged_file_blob_oids_reads_stage_zero_entries_without_git2() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo_dir = temp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("create repo dir");

    run_git(&repo_dir, &["init"]);
    run_git(&repo_dir, &["config", "user.name", "Test User"]);
    run_git(&repo_dir, &["config", "user.email", "test@example.com"]);

    fs::write(repo_dir.join("a.txt"), "alpha\n").expect("write a.txt");
    fs::create_dir_all(repo_dir.join("dir")).expect("create dir");
    fs::write(repo_dir.join("dir").join("b.txt"), "beta\n").expect("write b.txt");

    run_git(&repo_dir, &["add", "."]);

    let repo = find_repository_in_path(repo_dir.to_str().expect("repo path")).expect("repo");
    let staged = repo
        .get_all_staged_file_blob_oids()
        .expect("read staged blobs");

    assert_eq!(
        staged.get("a.txt"),
        Some(&run_git_stdout(&repo_dir, &["rev-parse", ":0:a.txt"]))
    );
    assert_eq!(
        staged.get("dir/b.txt"),
        Some(&run_git_stdout(&repo_dir, &["rev-parse", ":0:dir/b.txt"]))
    );
}
