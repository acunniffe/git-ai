use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

fn conflicting_cherry_pick_repo() -> (TestRepo, String) {
    let repo = TestRepo::new();
    fs::write(repo.path().join("conflict.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("conflict.txt", lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut file = repo.filename("conflict.txt");
    file.set_contents_no_stage(lines!["feature AI".ai()]);
    let source = repo.stage_all_and_commit("Feature AI").unwrap().commit_sha;
    repo.assert_file_committed_lines("conflict.txt", lines!["feature AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("conflict.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Main human").unwrap();
    repo.assert_file_committed_lines("conflict.txt", lines!["main human".human()]);
    (repo, source)
}

#[test]
fn test_git_cherry_pick_abort_discards_ai_resolution_checkpoint() {
    let (repo, source) = conflicting_cherry_pick_repo();
    assert!(repo.git(&["cherry-pick", &source]).is_err());
    repo.write_ai_edit("conflict.txt", "discarded AI cherry resolution\n");
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["cherry-pick", "--abort"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI cherry resolution\n",
    )
    .unwrap();
    repo.stage_all_and_commit("Human recreates discarded cherry resolution")
        .unwrap();
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines![
        "discarded AI cherry resolution".unattributed_human()
    ]);
}

#[test]
fn test_git_cherry_pick_quit_keeps_ai_resolution_for_ordinary_commit() {
    let (repo, source) = conflicting_cherry_pick_repo();
    assert!(repo.git(&["cherry-pick", &source]).is_err());
    repo.write_ai_edit("conflict.txt", "AI cherry resolution after quit\n");
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["cherry-pick", "--quit"]).unwrap();
    repo.git(&["commit", "-m", "Commit resolution after cherry-pick quit"])
        .unwrap();

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["AI cherry resolution after quit".ai()]);
}

#[test]
fn test_git_cherry_pick_single_skip_discards_resolution_but_keeps_unrelated_ai() {
    let (repo, source) = conflicting_cherry_pick_repo();
    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.set_contents_no_stage(lines!["unrelated AI".ai()]);
    assert!(repo.git(&["cherry-pick", &source]).is_err());
    repo.write_ai_edit("conflict.txt", "discarded AI cherry skip\n");
    repo.git(&["add", "--", "conflict.txt"]).unwrap();
    repo.git(&["cherry-pick", "--skip"]).unwrap();

    fs::write(
        repo.path().join("conflict.txt"),
        "discarded AI cherry skip\n",
    )
    .unwrap();
    repo.git(&["add", "--", "conflict.txt", "unrelated.txt"])
        .unwrap();
    repo.git(&["commit", "-m", "Commit after skipped cherry-pick"])
        .unwrap();
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["discarded AI cherry skip".unattributed_human()]);
    let mut unrelated = repo.filename("unrelated.txt");
    unrelated.assert_committed_lines(lines!["unrelated AI".ai()]);
}

#[test]
fn test_git_cherry_pick_x_preserves_source_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut file = repo.filename("picked.txt");
    file.set_contents_no_stage(lines!["picked AI".ai()]);
    let source = repo.stage_all_and_commit("Feature AI").unwrap().commit_sha;
    repo.assert_file_committed_lines("picked.txt", lines!["picked AI".ai()]);
    repo.git(&["checkout", &main]).unwrap();

    repo.git(&["cherry-pick", "-x", &source]).unwrap();
    let mut file = repo.filename("picked.txt");
    file.assert_committed_lines(lines!["picked AI".ai()]);
}

#[test]
fn test_git_cherry_pick_stdin_preserves_every_source_commit() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let mut first = repo.filename("first.txt");
    first.set_contents_no_stage(lines!["first stdin AI".ai()]);
    let first_source = repo
        .stage_all_and_commit("First stdin source")
        .unwrap()
        .commit_sha;
    repo.assert_file_committed_lines("first.txt", lines!["first stdin AI".ai()]);
    let mut second = repo.filename("second.txt");
    second.set_contents_no_stage(lines!["second stdin AI".ai()]);
    let second_source = repo
        .stage_all_and_commit("Second stdin source")
        .unwrap()
        .commit_sha;
    repo.assert_file_committed_lines("second.txt", lines!["second stdin AI".ai()]);
    repo.git(&["checkout", &main]).unwrap();

    let input = format!("{first_source}\n{second_source}\n");
    repo.git_with_stdin(&["cherry-pick", "--stdin"], input.as_bytes())
        .unwrap();

    let mut first = repo.filename("first.txt");
    first.assert_committed_lines(lines!["first stdin AI".ai()]);
    let mut second = repo.filename("second.txt");
    second.assert_committed_lines(lines!["second stdin AI".ai()]);
}

#[test]
fn test_git_cherry_pick_mainline_preserves_merged_ai_source() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let main = repo.current_branch();
    repo.git(&["branch", "target"]).unwrap();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut merged = repo.filename("merged.txt");
    merged.set_contents_no_stage(lines!["merged AI".ai()]);
    repo.stage_all_and_commit("Feature AI").unwrap();
    repo.assert_file_committed_lines("merged.txt", lines!["merged AI".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main.txt"), "main human\n").unwrap();
    repo.stage_all_and_commit("Diverge main").unwrap();
    repo.assert_file_committed_lines("main.txt", lines!["main human".human()]);
    repo.git(&["merge", "--no-ff", "-m", "Merge feature", "feature"])
        .unwrap();
    repo.assert_file_committed_lines("merged.txt", lines!["merged AI".ai()]);
    let merge_commit = repo.git(&["rev-parse", "HEAD"]).unwrap();

    repo.git(&["checkout", "target"]).unwrap();
    repo.git(&["cherry-pick", "-m", "1", merge_commit.trim()])
        .unwrap();
    let mut merged = repo.filename("merged.txt");
    merged.assert_committed_lines(lines!["merged AI".ai()]);
}

#[test]
fn test_git_cherry_pick_multiple_mainline_merges_preserves_each_ai_source() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let main = repo.current_branch();
    repo.git(&["branch", "target"]).unwrap();

    repo.git(&["checkout", "-b", "feature-one"]).unwrap();
    let mut first = repo.filename("first-merge.txt");
    first.set_contents_no_stage(lines!["first merged AI".ai()]);
    repo.stage_all_and_commit("First feature AI").unwrap();
    repo.assert_file_committed_lines("first-merge.txt", lines!["first merged AI".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main-one.txt"), "main one\n").unwrap();
    repo.stage_all_and_commit("First main divergence").unwrap();
    repo.assert_file_committed_lines("main-one.txt", lines!["main one".human()]);
    repo.git(&["merge", "--no-ff", "-m", "First merge", "feature-one"])
        .unwrap();
    repo.assert_file_committed_lines("first-merge.txt", lines!["first merged AI".ai()]);
    let first_merge = repo.git(&["rev-parse", "HEAD"]).unwrap();

    repo.git(&["checkout", "-b", "feature-two"]).unwrap();
    let mut second = repo.filename("second-merge.txt");
    second.set_contents_no_stage(lines!["second merged AI".ai()]);
    repo.stage_all_and_commit("Second feature AI").unwrap();
    repo.assert_file_committed_lines("second-merge.txt", lines!["second merged AI".ai()]);
    repo.git(&["checkout", &main]).unwrap();
    fs::write(repo.path().join("main-two.txt"), "main two\n").unwrap();
    repo.stage_all_and_commit("Second main divergence").unwrap();
    repo.assert_file_committed_lines("main-two.txt", lines!["main two".human()]);
    repo.git(&["merge", "--no-ff", "-m", "Second merge", "feature-two"])
        .unwrap();
    repo.assert_file_committed_lines("second-merge.txt", lines!["second merged AI".ai()]);
    let second_merge = repo.git(&["rev-parse", "HEAD"]).unwrap();

    repo.git(&["checkout", "target"]).unwrap();
    repo.git(&[
        "cherry-pick",
        "-m",
        "1",
        first_merge.trim(),
        second_merge.trim(),
    ])
    .unwrap();
    let mut first = repo.filename("first-merge.txt");
    first.assert_committed_lines(lines!["first merged AI".ai()]);
    let mut second = repo.filename("second-merge.txt");
    second.assert_committed_lines(lines!["second merged AI".ai()]);
}

#[test]
fn test_git_cherry_pick_allow_empty_carries_unrelated_ai_checkpoint() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let main = repo.current_branch();
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    repo.git(&["commit", "--allow-empty", "-m", "Intentional empty source"])
        .unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    let empty_source = repo.git(&["rev-parse", "HEAD"]).unwrap();
    repo.git(&["checkout", &main]).unwrap();

    let mut dirty = repo.filename("dirty.txt");
    dirty.set_contents_no_stage(lines!["dirty AI around empty pick".ai()]);
    repo.git(&["cherry-pick", "--allow-empty", empty_source.trim()])
        .unwrap();
    repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
    repo.git(&["add", "--", "dirty.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit dirty AI after empty pick"])
        .unwrap();
    let mut dirty = repo.filename("dirty.txt");
    dirty.assert_committed_lines(lines!["dirty AI around empty pick".ai()]);
}

#[test]
fn test_git_cherry_pick_empty_drop_and_keep_carry_unrelated_ai_checkpoint() {
    for policy in ["drop", "keep"] {
        let repo = TestRepo::new();
        fs::write(repo.path().join("same.txt"), "base\n").unwrap();
        repo.stage_all_and_commit("Initial commit").unwrap();
        repo.assert_file_committed_lines("same.txt", lines!["base".human()]);
        let main = repo.current_branch();
        repo.git(&["checkout", "-b", "feature"]).unwrap();
        let mut same = repo.filename("same.txt");
        same.set_contents_no_stage(lines!["already applied".ai()]);
        let source = repo
            .stage_all_and_commit("Feature change")
            .unwrap()
            .commit_sha;
        repo.assert_file_committed_lines("same.txt", lines!["already applied".ai()]);

        repo.git(&["checkout", &main]).unwrap();
        fs::write(repo.path().join("same.txt"), "already applied").unwrap();
        repo.stage_all_and_commit("Apply same change as human")
            .unwrap();
        repo.assert_file_committed_lines("same.txt", lines!["already applied".human()]);
        let mut dirty = repo.filename("dirty.txt");
        dirty.set_contents_no_stage(lines!["dirty AI around empty policy".ai()]);

        let option = format!("--empty={policy}");
        repo.git(&["cherry-pick", &option, &source]).unwrap();
        repo.assert_file_committed_lines("same.txt", lines!["already applied".human()]);
        repo.git(&["add", "--", "dirty.txt"]).unwrap();
        repo.git(&["commit", "-m", "Commit dirty AI after empty policy"])
            .unwrap();
        let mut dirty = repo.filename("dirty.txt");
        dirty.assert_committed_lines(lines!["dirty AI around empty policy".ai()]);
    }
}

#[test]
#[ignore = "dedicated daemon spawn-scaling guard"]
fn cherry_pick_mainline_spawn_count_is_constant_in_merge_count() {
    fn run(merge_count: usize) -> usize {
        let log_dir = std::env::temp_dir().join(format!(
            "git-ai-mainline-spawnlog-{}-{merge_count}",
            std::process::id()
        ));
        fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("spawns.log");
        let _ = fs::remove_file(&log_path);
        let repo =
            TestRepo::new_with_daemon_env(&[("GIT_AI_SPAWN_LOG", log_path.to_str().unwrap())]);
        fs::write(repo.path().join("base.txt"), "base\n").unwrap();
        repo.stage_all_and_commit("Initial commit").unwrap();
        repo.assert_file_committed_lines("base.txt", lines!["base".human()]);
        let main = repo.current_branch();
        repo.git(&["branch", "target"]).unwrap();
        let mut merges = Vec::new();
        for index in 0..merge_count {
            let feature = format!("feature-{index}");
            repo.git(&["checkout", "-b", &feature]).unwrap();
            let filename = format!("feature-{index}.txt");
            let mut file = repo.filename(&filename);
            file.set_contents_no_stage(lines![format!("AI {index}").ai()]);
            repo.stage_all_and_commit(&format!("Feature AI {index}"))
                .unwrap();
            repo.assert_file_committed_lines(&filename, lines![format!("AI {index}").ai()]);
            repo.git(&["checkout", &main]).unwrap();
            fs::write(
                repo.path().join(format!("main-{index}.txt")),
                format!("main {index}\n"),
            )
            .unwrap();
            repo.stage_all_and_commit(&format!("Main divergence {index}"))
                .unwrap();
            repo.assert_file_committed_lines(
                &format!("main-{index}.txt"),
                lines![format!("main {index}").human()],
            );
            repo.git(&[
                "merge",
                "--no-ff",
                "-m",
                &format!("Merge {index}"),
                &feature,
            ])
            .unwrap();
            repo.assert_file_committed_lines(&filename, lines![format!("AI {index}").ai()]);
            merges.push(repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string());
        }
        repo.git(&["checkout", "target"]).unwrap();
        repo.sync_daemon();
        let before = fs::read_to_string(&log_path)
            .map(|contents| contents.lines().count())
            .unwrap_or(0);
        let mut args = vec!["cherry-pick".to_string(), "-m".to_string(), "1".to_string()];
        args.extend(merges);
        repo.git(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .unwrap();
        repo.sync_daemon();
        let after = fs::read_to_string(&log_path)
            .map(|contents| contents.lines().count())
            .unwrap_or(0);
        let _ = fs::remove_dir_all(&log_dir);
        after - before
    }

    let small = run(2);
    let large = run(6);
    eprintln!("mainline cherry-pick spawns: 2 -> {small}, 6 -> {large}");
    assert!(
        large <= small + 4,
        "mainline cherry-pick spawn count scales: 2 -> {small}, 6 -> {large}"
    );
}
