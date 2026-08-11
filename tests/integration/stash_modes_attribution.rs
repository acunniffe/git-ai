use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::collections::BTreeSet;
use std::fs;

fn tracked_pair(repo: &TestRepo) {
    let mut staged = repo.filename("staged.txt");
    staged.set_contents(crate::lines![
        "staged base".human(),
        "staged footer".human(),
    ]);
    let mut unstaged = repo.filename("unstaged.txt");
    unstaged.set_contents(crate::lines![
        "unstaged base".human(),
        "unstaged footer".human(),
    ]);
    repo.stage_all_and_commit("base").unwrap();
}

fn prepare_staged_and_unstaged_ai(repo: &TestRepo) {
    let mut staged = repo.filename("staged.txt");
    staged.set_contents_no_stage(crate::lines![
        "staged base".human(),
        "staged ai".ai(),
        "staged footer".human(),
    ]);
    repo.git(&["add", "staged.txt"]).unwrap();

    let mut unstaged = repo.filename("unstaged.txt");
    unstaged.set_contents_no_stage(crate::lines![
        "unstaged base".human(),
        "unstaged ai".ai(),
        "unstaged footer".human(),
    ]);
}

#[test]
fn stash_keep_index_partitions_live_and_stashed_attribution() {
    let repo = TestRepo::new();
    tracked_pair(&repo);
    prepare_staged_and_unstaged_ai(&repo);

    repo.git(&["stash", "push", "--keep-index", "-m", "partition"])
        .unwrap();
    assert_eq!(
        repo.read_file("staged.txt").unwrap(),
        "staged base\nstaged ai\nstaged footer"
    );
    assert_eq!(
        repo.read_file("unstaged.txt").unwrap(),
        "unstaged base\nunstaged footer"
    );

    repo.git(&["commit", "-m", "commit retained index"])
        .unwrap();
    repo.sync_daemon_force();
    let mut staged = repo.filename("staged.txt");
    staged.assert_committed_lines(crate::lines![
        "staged base".human(),
        "staged ai".ai(),
        "staged footer".human(),
    ]);

    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("commit stashed worktree")
        .unwrap();
    let mut unstaged = repo.filename("unstaged.txt");
    unstaged.assert_committed_lines(crate::lines![
        "unstaged base".human(),
        "unstaged ai".ai(),
        "unstaged footer".human(),
    ]);
}

#[test]
fn stash_staged_partitions_stashed_and_live_attribution() {
    let repo = TestRepo::new();
    tracked_pair(&repo);
    prepare_staged_and_unstaged_ai(&repo);

    repo.git(&["stash", "push", "--staged", "-m", "index only"])
        .unwrap();
    assert_eq!(
        repo.read_file("staged.txt").unwrap(),
        "staged base\nstaged footer"
    );
    assert_eq!(
        repo.read_file("unstaged.txt").unwrap(),
        "unstaged base\nunstaged ai\nunstaged footer"
    );

    repo.stage_all_and_commit("commit retained worktree")
        .unwrap();
    let mut unstaged = repo.filename("unstaged.txt");
    unstaged.assert_committed_lines(crate::lines![
        "unstaged base".human(),
        "unstaged ai".ai(),
        "unstaged footer".human(),
    ]);

    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("commit stashed index").unwrap();
    let mut staged = repo.filename("staged.txt");
    staged.assert_committed_lines(crate::lines![
        "staged base".human(),
        "staged ai".ai(),
        "staged footer".human(),
    ]);
}

#[test]
fn stash_apply_index_restores_index_partition_and_attribution() {
    let repo = TestRepo::new();
    tracked_pair(&repo);
    prepare_staged_and_unstaged_ai(&repo);

    repo.git(&["stash", "push", "-m", "both partitions"])
        .unwrap();
    repo.git(&["stash", "apply", "--index", "stash@{0}"])
        .unwrap();

    let staged_names = repo.git_og(&["diff", "--cached", "--name-only"]).unwrap();
    let unstaged_names = repo.git_og(&["diff", "--name-only"]).unwrap();
    assert_eq!(staged_names.trim(), "staged.txt");
    assert_eq!(unstaged_names.trim(), "unstaged.txt");

    repo.git(&["commit", "-m", "commit restored index"])
        .unwrap();
    repo.sync_daemon_force();
    let mut staged = repo.filename("staged.txt");
    staged.assert_committed_lines(crate::lines![
        "staged base".human(),
        "staged ai".ai(),
        "staged footer".human(),
    ]);

    repo.stage_all_and_commit("commit restored worktree")
        .unwrap();
    let mut unstaged = repo.filename("unstaged.txt");
    unstaged.assert_committed_lines(crate::lines![
        "unstaged base".human(),
        "unstaged ai".ai(),
        "unstaged footer".human(),
    ]);
}

#[test]
fn stash_all_roundtrips_ignored_and_untracked_ai_files() {
    let repo = TestRepo::new();
    let mut ignore = repo.filename(".gitignore");
    ignore.set_contents(crate::lines!["ignored.txt".human()]);
    repo.stage_all_and_commit("ignore fixture").unwrap();

    let mut ignored = repo.filename("ignored.txt");
    ignored.set_contents_no_stage(crate::lines!["ignored ai".ai()]);
    let mut untracked = repo.filename("untracked.txt");
    untracked.set_contents_no_stage(crate::lines!["untracked ai".ai()]);

    repo.git(&["stash", "push", "--all", "-m", "all files"])
        .unwrap();
    assert!(repo.read_file("ignored.txt").is_none());
    assert!(repo.read_file("untracked.txt").is_none());

    repo.git(&["stash", "pop"]).unwrap();
    repo.git(&["add", "-f", "ignored.txt", "untracked.txt"])
        .unwrap();
    repo.commit("commit restored ignored files").unwrap();
    ignored.assert_committed_lines(crate::lines!["ignored ai".ai()]);
    untracked.assert_committed_lines(crate::lines!["untracked ai".ai()]);
}

#[test]
fn stash_patch_partitions_two_hunks_in_one_file() {
    let repo = TestRepo::new();
    let mut file = repo.filename("patch.txt");
    let base = (1..=20)
        .map(|line| format!("base {line}").human())
        .collect::<Vec<_>>();
    file.set_contents(base.clone());
    repo.stage_all_and_commit("patch base").unwrap();

    let mut modified = base;
    modified.insert(2, "first ai hunk".ai());
    modified.insert(18, "second ai hunk".ai());
    file.set_contents_no_stage(modified);

    repo.git_with_stdin(&["stash", "push", "--patch", "-m", "one hunk"], b"y\nn\n")
        .unwrap();
    let after_stash = repo.read_file("patch.txt").unwrap();
    assert!(!after_stash.contains("first ai hunk"));
    assert!(after_stash.contains("second ai hunk"));

    repo.stage_all_and_commit("commit retained hunk").unwrap();
    let mut file = repo.filename("patch.txt");
    let retained = after_stash
        .lines()
        .map(|line| {
            if line == "second ai hunk" {
                line.ai()
            } else {
                line.human()
            }
        })
        .collect::<Vec<_>>();
    file.assert_committed_lines(retained);

    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("commit selected hunk").unwrap();
    let final_contents = repo.read_file("patch.txt").unwrap();
    let final_lines = final_contents
        .lines()
        .map(|line| {
            if matches!(line, "first ai hunk" | "second ai hunk") {
                line.ai()
            } else {
                line.human()
            }
        })
        .collect::<Vec<_>>();
    file.assert_committed_lines(final_lines);
}

fn stash_state_oids(repo: &TestRepo) -> BTreeSet<String> {
    let dir = repo.path().join(".git").join("ai").join("stashes_v2");
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect()
}

#[test]
fn stash_create_store_preserves_live_and_restorable_attribution() {
    let repo = TestRepo::new();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base".human()]);
    let mut created = repo.filename("created.txt");
    created.set_contents(crate::lines![
        "created base".human(),
        "created footer".human(),
    ]);
    repo.stage_all_and_commit("base").unwrap();

    created.set_contents_no_stage(crate::lines![
        "created base".human(),
        "created by ai".ai(),
        "created footer".human(),
    ]);
    let stash_oid = repo
        .git(&["stash", "create", "detached stash"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(stash_oid.len(), 40);
    assert_eq!(
        repo.read_file("created.txt").unwrap(),
        "created base\ncreated by ai\ncreated footer"
    );

    repo.git(&["stash", "store", "-m", "stored", &stash_oid])
        .unwrap();
    repo.sync_daemon_force();
    assert!(
        repo.current_working_logs()
            .read_initial_attributions()
            .files
            .contains_key("created.txt")
            || repo
                .current_working_logs()
                .read_all_checkpoints()
                .unwrap()
                .iter()
                .any(|checkpoint| checkpoint
                    .entries
                    .iter()
                    .any(|entry| entry.file == "created.txt")),
        "stash store must not consume live provenance"
    );
    assert!(stash_state_oids(&repo).contains(&stash_oid));

    repo.git(&["reset", "--hard", "HEAD"]).unwrap();
    repo.git(&["stash", "apply", "stash@{0}"]).unwrap();
    repo.stage_all_and_commit("apply stored stash").unwrap();
    created.assert_committed_lines(crate::lines![
        "created base".human(),
        "created by ai".ai(),
        "created footer".human(),
    ]);
}

#[test]
fn stash_drop_removes_only_selected_attribution_state() {
    let repo = TestRepo::new();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base".human()]);
    repo.stage_all_and_commit("base").unwrap();

    let mut first = repo.filename("first.txt");
    first.set_contents_no_stage(crate::lines!["first ai".ai()]);
    repo.git(&["stash", "push", "-u", "-m", "first"]).unwrap();
    let first_oid = repo.git_og(&["rev-parse", "stash@{0}"]).unwrap();

    let mut second = repo.filename("second.txt");
    second.set_contents_no_stage(crate::lines!["second ai".ai()]);
    repo.git(&["stash", "push", "-u", "-m", "second"]).unwrap();
    let second_oid = repo.git_og(&["rev-parse", "stash@{0}"]).unwrap();

    repo.git(&["stash", "drop", "stash@{1}"]).unwrap();
    repo.sync_daemon_force();
    let state = stash_state_oids(&repo);
    assert!(!state.contains(first_oid.trim()));
    assert!(state.contains(second_oid.trim()));

    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("apply survivor").unwrap();
    second.assert_committed_lines(crate::lines!["second ai".ai()]);
    assert!(repo.read_file("first.txt").is_none());
}

#[test]
fn stash_clear_cleans_all_state_and_legacy_save_still_roundtrips() {
    let repo = TestRepo::new();
    let mut base = repo.filename("base.txt");
    base.set_contents(crate::lines!["base".human()]);
    repo.stage_all_and_commit("base").unwrap();

    for name in ["one.txt", "two.txt"] {
        let mut file = repo.filename(name);
        file.set_contents_no_stage(crate::lines![format!("{name} ai").ai()]);
        repo.git(&["stash", "push", "-u", "-m", name]).unwrap();
    }
    repo.sync_daemon_force();
    assert_eq!(stash_state_oids(&repo).len(), 2);
    repo.git(&["stash", "clear"]).unwrap();
    repo.sync_daemon_force();
    assert!(stash_state_oids(&repo).is_empty());
    assert!(repo.git_og(&["stash", "list"]).unwrap().trim().is_empty());

    let mut legacy = repo.filename("legacy.txt");
    legacy.set_contents_no_stage(crate::lines!["legacy ai".ai()]);
    repo.git(&["stash", "save", "-u", "legacy message"])
        .unwrap();
    repo.git(&["stash", "pop"]).unwrap();
    repo.stage_all_and_commit("legacy restore").unwrap();
    legacy.assert_committed_lines(crate::lines!["legacy ai".ai()]);
}

crate::reuse_tests_in_worktree!(
    stash_keep_index_partitions_live_and_stashed_attribution,
    stash_staged_partitions_stashed_and_live_attribution,
    stash_apply_index_restores_index_partition_and_attribution,
    stash_all_roundtrips_ignored_and_untracked_ai_files,
    stash_patch_partitions_two_hunks_in_one_file,
);
