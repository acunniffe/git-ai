use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::extract_json_object;
use git_ai::authorship::stats::CommitStats;
use insta::assert_debug_snapshot;
use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Extract the first complete JSON object from mixed stdout/stderr output.
fn stats_from_args(repo: &TestRepo, args: &[&str]) -> CommitStats {
    let raw = repo.git_ai(args).expect("git-ai stats should succeed");
    let json = extract_json_object(&raw);
    serde_json::from_str(&json).expect("valid stats json")
}

fn stats_while_restoring_authorship_note(
    repo: &TestRepo,
    commit_sha: &str,
    args: &[&str],
) -> String {
    let note = repo
        .read_authorship_note(commit_sha)
        .expect("commit should start with an authorship note");
    repo.git_og(&["notes", "--ref=ai", "remove", commit_sha])
        .expect("authorship note should be removable");

    let mut command =
        repo.git_ai_command_without_pre_sync_for_test(args, &[("GIT_AI_TEST_FORCE_TTY", "1")]);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("stats should start");
    let mut waiting_message = String::new();
    BufReader::new(child.stderr.take().expect("stats stderr should be piped"))
        .read_line(&mut waiting_message)
        .expect("stats should write its waiting indicator");
    assert!(
        waiting_message.contains("Waiting for git-ai to process this commit"),
        "interactive stats should show a waiting indicator, got:\n{waiting_message}"
    );

    repo.git_og(&["notes", "--ref=ai", "add", "-f", "-m", &note, commit_sha])
        .expect("authorship note should be restorable");

    let output = child.wait_with_output().expect("stats should finish");
    assert!(
        output.status.success(),
        "stats failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        waiting_message
    )
}

#[test]
fn test_stats_default_waits_for_recent_commit_authorship_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("recent-default.txt");
    file.set_contents(crate::lines!["AI line".ai()]);
    let commit = repo.stage_all_and_commit("recent AI commit").unwrap();
    let started = Instant::now();
    let output =
        stats_while_restoring_authorship_note(&repo, &commit.commit_sha, &["stats", "--json"]);

    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "stats returned before the delayed note was restored"
    );
    assert!(
        output.contains("Waiting for git-ai to process this commit"),
        "interactive stats should show a waiting indicator, got:\n{output}"
    );
    let stats: CommitStats = serde_json::from_str(&extract_json_object(&output)).unwrap();
    assert_eq!(stats.ai_additions, 1);
    assert_eq!(stats.unknown_additions, 0);
}

#[test]
fn test_stats_single_rev_waits_for_recent_commit_authorship_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("recent-rev.txt");
    file.set_contents(crate::lines!["AI line".ai()]);
    let commit = repo.stage_all_and_commit("recent AI commit").unwrap();
    let started = Instant::now();
    let output = stats_while_restoring_authorship_note(
        &repo,
        &commit.commit_sha,
        &["stats", &commit.commit_sha, "--json"],
    );

    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "stats <rev> returned before the delayed note was restored"
    );
    let stats: CommitStats = serde_json::from_str(&extract_json_object(&output)).unwrap();
    assert_eq!(stats.ai_additions, 1);
    assert_eq!(stats.unknown_additions, 0);
}

#[test]
fn test_stats_does_not_wait_for_old_commit_without_authorship_note() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("old.txt"), "old line\n").unwrap();
    repo.git_og(&["add", "old.txt"]).unwrap();
    repo.git_og_with_env(
        &["commit", "-m", "old commit"],
        &[
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
        ],
    )
    .unwrap();

    let output = repo
        .git_ai_with_env_without_pre_sync_for_test(
            &["stats", "--json"],
            &[("GIT_AI_TEST_FORCE_TTY", "1")],
        )
        .expect("stats should work without an authorship note");
    assert!(
        !output.contains("Waiting for git-ai to process this commit"),
        "stats should not wait for an old commit, got:\n{output}"
    );
    let stats: CommitStats = serde_json::from_str(&extract_json_object(&output)).unwrap();
    assert_eq!(stats.git_diff_added_lines, 1);
    assert_eq!(stats.git_diff_deleted_lines, 0);
    assert_eq!(stats.ai_additions, 0);
    assert_eq!(stats.human_additions, 0);
    assert_eq!(stats.unknown_additions, 1);
}

#[test]
fn test_stats_range_does_not_wait_for_missing_authorship_note() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("range-wait.txt"), "first\n").unwrap();
    repo.git_og(&["add", "range-wait.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "first raw commit"]).unwrap();
    let first = repo.git_og(&["rev-parse", "HEAD"]).unwrap();

    fs::write(repo.path().join("range-wait.txt"), "first\nsecond\n").unwrap();
    repo.git_og(&["add", "range-wait.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "second raw commit"]).unwrap();
    let second = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    let range = format!("{}..{}", first.trim(), second.trim());

    let output = repo
        .git_ai_with_env_without_pre_sync_for_test(
            &["stats", &range, "--json"],
            &[("GIT_AI_TEST_FORCE_TTY", "1")],
        )
        .expect("stats range should work without authorship notes");
    assert!(
        !output.contains("Waiting for git-ai to process this commit"),
        "stats range should not wait for a missing note, got:\n{output}"
    );
    let stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&extract_json_object(&output)).unwrap();
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert_eq!(stats.authorship_stats.commits_with_authorship, 0);
    assert_eq!(
        stats.authorship_stats.commits_without_authorship,
        vec![second.trim().to_string()]
    );
    assert_eq!(stats.range_stats.git_diff_added_lines, 1);
    assert_eq!(stats.range_stats.git_diff_deleted_lines, 0);
    assert_eq!(stats.range_stats.ai_additions, 0);
    assert_eq!(stats.range_stats.human_additions, 0);
    assert_eq!(stats.range_stats.unknown_additions, 1);
}

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

fn configure_repo_external_diff_helper(repo: &TestRepo) -> String {
    let marker = "STATS_EXTERNAL_DIFF_MARKER";
    let helper_path = repo.path().join("stats-ext-diff-helper.sh");
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
fn test_authorship_log_stats() {
    let repo = TestRepo::new();

    // Create an initial commit
    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI creates a brand new file with planets
    let mut file = repo.filename("planets.txt");
    file.set_contents(crate::lines![
        "Mercury".human(),
        "Venus".human(),
        "Earth".ai(),
        "Mars".ai(),
        "Jupiter".human(),
        "Saturn".ai(),
        "Uranus".ai(),
        "Neptune".ai(),
        "Pluto (dwarf)".ai(),
    ]);

    file.set_contents(crate::lines![
        "Mercury".human(),
        "Venus".human(),
        "Earth".ai(),
        "Mars".ai(),
        "Jupiter".human(),
        "Saturn".ai(),
        "Uranus".ai(),
        "Neptune (override)".human(),
        "Pluto (dwarf)".ai(),
    ]);

    // First commit should have all the planets
    let first_commit = repo.stage_all_and_commit("Add planets").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "Mercury".human(),
        "Venus".human(),
        "Earth".ai(),
        "Mars".ai(),
        "Jupiter".human(),
        "Saturn".ai(),
        "Uranus".ai(),
        "Neptune (override)".human(),
        "Pluto (dwarf)".ai(),
    ]);

    assert_eq!(first_commit.authorship_log.attestations.len(), 1);

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();
    // The integration harness now uses mock_known_human (CheckpointKind::KnownHuman), which
    // produces h_-prefixed attestation entries for lines written under a human checkpoint.
    // Neptune (override) — human-overrides-AI line — gets h_<hash> attestation.
    // Mercury, Venus, Jupiter also get h_<hash> attestation from the KnownHuman checkpoint.
    // All 4 human-written lines now count as human_additions; unknown_additions = 0.
    // Neptune (override) is now h_<hash> attested, so it counts as human_additions only.
    assert_eq!(stats.human_additions, 4);
    assert_eq!(stats.unknown_additions, 0);
    assert_eq!(stats.ai_additions, 5); // Neptune (override) no longer counted as mixed AI
    assert_eq!(stats.ai_accepted, 5);
    assert_eq!(stats.git_diff_deleted_lines, 0);
    assert_eq!(stats.git_diff_added_lines, 9);

    assert_eq!(stats.tool_model_breakdown.len(), 1);
    // ai_additions = ai_accepted = 5
    assert_eq!(
        stats
            .tool_model_breakdown
            .get("mock_ai::unknown")
            .unwrap()
            .ai_additions,
        5
    );
    assert_eq!(
        stats
            .tool_model_breakdown
            .get("mock_ai::unknown")
            .unwrap()
            .ai_accepted,
        5
    );
}

#[test]
fn test_stats_cli_range() {
    let repo = TestRepo::new();

    // Initial human commit
    let mut file = repo.filename("range.txt");
    file.set_contents(crate::lines!["Line 1".human()]);
    let first = repo.stage_all_and_commit("Initial human").unwrap();

    // AI adds a line in a second commit
    file.set_contents(crate::lines!["Line 1".human(), "Line 2".ai()]);
    let second = repo.stage_all_and_commit("AI adds line").unwrap();

    // Sanity check individual commit stats
    let range = format!("{}..{}", first.commit_sha, second.commit_sha);
    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("git-ai stats range should succeed");

    let output = extract_json_object(&raw);
    let stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&output).unwrap();

    // Range should only include the AI commit's diff and report at least one AI-added line
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert!(
        stats.range_stats.ai_additions >= 1,
        "expected at least one AI addition in range, got {}",
        stats.range_stats.ai_additions
    );
    assert!(
        stats.range_stats.git_diff_added_lines >= stats.range_stats.ai_additions,
        "git diff added lines ({}) should be >= ai_additions ({})",
        stats.range_stats.git_diff_added_lines,
        stats.range_stats.ai_additions
    );
}

#[test]
fn test_stats_cli_range_ignores_repo_external_diff_helper() {
    let repo = TestRepo::new();

    let mut file = repo.filename("stats-range-ext.txt");
    file.set_contents(crate::lines!["base".human()]);
    let first = repo.stage_all_and_commit("initial").unwrap();

    file.set_contents(crate::lines!["base".human(), "ai line".ai()]);
    let second = repo.stage_all_and_commit("ai second").unwrap();

    let marker = configure_repo_external_diff_helper(&repo);
    let proxied_diff = repo
        .git(&["diff", &first.commit_sha, &second.commit_sha])
        .expect("proxied git diff should succeed");
    assert!(
        proxied_diff.contains(&marker),
        "sanity check: proxied git diff should use configured external helper"
    );

    let range = format!("{}..{}", first.commit_sha, second.commit_sha);
    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("git-ai stats range should succeed with external diff configured");
    assert!(
        !raw.contains(&marker),
        "git-ai stats output should not include external helper output, got:\n{}",
        raw
    );

    let output = extract_json_object(&raw);
    let stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&output).unwrap();
    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert!(
        stats.range_stats.git_diff_added_lines >= 1,
        "expected at least one added line in range, got {}",
        stats.range_stats.git_diff_added_lines
    );
    assert!(stats.range_stats.ai_additions >= 1);
}

#[test]
fn test_stats_cli_range_with_hostile_diff_config() {
    let repo = TestRepo::new();

    let mut file = repo.filename("stats-range-hostile.txt");
    file.set_contents(crate::lines!["base".human()]);
    let first = repo.stage_all_and_commit("initial").unwrap();

    file.set_contents(crate::lines!["base".human(), "ai line".ai()]);
    let second = repo.stage_all_and_commit("ai second").unwrap();

    configure_hostile_diff_settings(&repo);

    let range = format!("{}..{}", first.commit_sha, second.commit_sha);
    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("git-ai stats range should succeed with hostile diff config");
    let output = extract_json_object(&raw);
    let stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&output).unwrap();

    assert_eq!(stats.authorship_stats.total_commits, 1);
    assert!(stats.range_stats.git_diff_added_lines >= 1);
    assert!(stats.range_stats.ai_additions >= 1);
}

#[test]
fn test_stats_cli_empty_tree_range() {
    let repo = TestRepo::new();

    // First commit: AI line
    let mut file = repo.filename("history.txt");
    file.set_contents(crate::lines!["AI Line 1".ai()]);
    let _first = repo.stage_all_and_commit("Initial AI").unwrap();
    file.assert_committed_lines(crate::lines!["AI Line 1".ai()]);

    // Second commit: human line
    file.set_contents(crate::lines!["AI Line 1".ai(), "Human Line 2".human()]);
    repo.stage_all_and_commit("Human adds line").unwrap();
    file.assert_committed_lines(crate::lines!["AI Line 1".ai(), "Human Line 2".human(),]);

    // Git's empty tree OID
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse HEAD should succeed")
        .trim()
        .to_string();
    let range = format!("{}..{}", empty_tree, head);

    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("git-ai stats empty-tree range should succeed");

    let output = extract_json_object(&raw);
    let stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&output).unwrap();

    // Entire history from empty tree to HEAD:
    // - 2 commits in range
    // - 1 AI-added line, 1 human-added line in final diff
    assert_eq!(stats.authorship_stats.total_commits, 2);
    assert_eq!(stats.range_stats.git_diff_added_lines, 2);
    assert_eq!(stats.range_stats.ai_additions, 1);
    assert_eq!(stats.range_stats.human_additions, 1);
    assert_eq!(stats.range_stats.unknown_additions, 0);
}

#[test]
fn test_markdown_stats_deletion_only() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    let stats = CommitStats {
        human_additions: 0,
        unknown_additions: 0,
        ai_additions: 0,
        ai_accepted: 0,

        git_diff_deleted_lines: 5,
        git_diff_added_lines: 0,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_all_human() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    let stats = CommitStats {
        human_additions: 10,
        unknown_additions: 0,
        ai_additions: 0,
        ai_accepted: 0,

        git_diff_deleted_lines: 0,
        git_diff_added_lines: 10,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_all_ai() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    let stats = CommitStats {
        human_additions: 0,
        unknown_additions: 0,
        ai_additions: 15,
        ai_accepted: 15,

        git_diff_deleted_lines: 0,
        git_diff_added_lines: 15,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_mixed() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    let stats = CommitStats {
        human_additions: 10,
        unknown_additions: 0,
        ai_additions: 15,
        ai_accepted: 15,

        git_diff_deleted_lines: 5,
        git_diff_added_lines: 30,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_no_mixed() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    let stats = CommitStats {
        human_additions: 8,
        unknown_additions: 0,
        ai_additions: 12,
        ai_accepted: 12,

        git_diff_deleted_lines: 0,
        git_diff_added_lines: 20,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_minimal_human() {
    use git_ai::authorship::stats::write_stats_to_markdown;
    use std::collections::BTreeMap;

    // Test that humans get at least 2 visible blocks if they have more than 1 line
    let stats = CommitStats {
        human_additions: 2,
        unknown_additions: 0,
        ai_additions: 98,
        ai_accepted: 98,

        git_diff_deleted_lines: 0,
        git_diff_added_lines: 100,
        tool_model_breakdown: BTreeMap::new(),
    };

    let markdown = write_stats_to_markdown(&stats);

    assert_debug_snapshot!(markdown);
}

#[test]
fn test_markdown_stats_formatting() {
    use git_ai::authorship::stats::{ToolModelHeadlineStats, write_stats_to_markdown};
    use std::collections::BTreeMap;

    let mut tool_model_breakdown = BTreeMap::new();
    tool_model_breakdown.insert(
        "cursor::claude-3.5-sonnet".to_string(),
        ToolModelHeadlineStats {
            ai_additions: 6,
            ai_accepted: 6,
        },
    );

    let stats = CommitStats {
        human_additions: 5,
        unknown_additions: 0,
        ai_additions: 6,
        ai_accepted: 6,
        git_diff_deleted_lines: 2,
        git_diff_added_lines: 13,
        tool_model_breakdown,
    };

    let markdown = write_stats_to_markdown(&stats);
    println!("{}", markdown);
    assert_debug_snapshot!(markdown);
}

#[test]
fn test_stats_default_ignores_snapshot_files() {
    let repo = TestRepo::new();
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    repo.filename("src/main.rs")
        .set_contents(crate::lines!["fn main() {}".ai()]);
    repo.filename("__snapshots__/main.snap")
        .set_contents(crate::lines![
            "snapshot line 1",
            "snapshot line 2",
            "snapshot line 3"
        ]);
    repo.stage_all_and_commit("Add source and snapshot")
        .unwrap();

    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(stats.git_diff_added_lines, 1);
    assert_eq!(stats.ai_additions, 1);
}

#[test]
fn test_stats_default_ignores_lockfiles_and_generated_files() {
    let repo = TestRepo::new();
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    repo.filename("src/lib.rs")
        .set_contents(crate::lines!["pub fn answer() -> u32 { 42 }".ai()]);
    repo.filename("Cargo.lock")
        .set_contents(vec!["lock".to_string().repeat(5); 650]);
    repo.filename("api.generated.ts")
        .set_contents(vec!["export type X = string;".to_string(); 500]);
    repo.stage_all_and_commit("Add source and generated artifacts")
        .unwrap();

    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(stats.git_diff_added_lines, 1);
    assert_eq!(stats.ai_additions, 1);
}

#[test]
fn test_stats_ignores_linguist_generated_patterns() {
    let repo = TestRepo::new();
    repo.filename(".gitattributes")
        .set_contents(crate::lines!["generated/** linguist-generated=true"]);
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit with gitattributes")
        .unwrap();

    repo.filename("src/main.rs")
        .set_contents(crate::lines!["fn run() {}".ai()]);
    repo.filename("generated/schema.ts")
        .set_contents(crate::lines!["export const schema = {};"]);
    repo.stage_all_and_commit("Add source and linguist-generated file")
        .unwrap();

    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(stats.git_diff_added_lines, 1);
    assert_eq!(stats.ai_additions, 1);
}

#[test]
fn test_stats_keeps_negative_linguist_patterns_counted() {
    let repo = TestRepo::new();
    repo.filename(".gitattributes").set_contents(crate::lines![
        "generated/** linguist-generated=true",
        "manual/** linguist-generated=false"
    ]);
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit with attrs")
        .unwrap();

    repo.filename("generated/out.ts")
        .set_contents(crate::lines!["export const ignored = true;"]);
    repo.filename("manual/kept.ts")
        .set_contents(crate::lines!["export const counted = true;".ai()]);
    repo.stage_all_and_commit("Add generated and manual files")
        .unwrap();

    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(stats.git_diff_added_lines, 1);
    assert_eq!(stats.ai_additions, 1);
}

#[test]
fn test_stats_in_bare_clone_uses_root_gitattributes_linguist_generated() {
    let repo = TestRepo::new();
    repo.filename(".gitattributes")
        .set_contents(crate::lines!["generated/** linguist-generated=true"]);
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit with gitattributes")
        .unwrap();

    repo.filename("src/main.rs")
        .set_contents(crate::lines!["fn run() {}".ai()]);
    repo.filename("generated/schema.ts")
        .set_contents(crate::lines!["export const schema = {};"]);
    repo.stage_all_and_commit("Add source and linguist-generated file")
        .unwrap();

    let temp = tempfile::tempdir().expect("tempdir");
    let bare = temp.path().join("repo.git");
    run_git(
        temp.path(),
        &[
            "clone",
            "--bare",
            repo.path().to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );

    let output = Command::new(crate::repos::test_repo::get_binary_path())
        .args(["stats", "HEAD", "--json"])
        .current_dir(&bare)
        .env(
            "GIT_AI_TEST_DB_PATH",
            temp.path().join("db").to_str().unwrap(),
        )
        .output()
        .expect("git-ai stats should run in bare repo");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git-ai stats failed in bare clone:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    let combined = if stdout.is_empty() {
        stderr.to_string()
    } else if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{}{}", stdout, stderr)
    };
    let json = extract_json_object(&combined);
    let stats: CommitStats = serde_json::from_str(&json).expect("valid stats json");
    assert_eq!(stats.git_diff_added_lines, 1);
}

#[test]
fn test_stats_ignore_flag_is_additive_to_defaults() {
    let repo = TestRepo::new();
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    repo.filename("src/main.rs")
        .set_contents(crate::lines!["fn main() {}".ai()]);
    repo.filename("docs/keep.txt")
        .set_contents(crate::lines!["this line is human"]);
    repo.stage_all_and_commit("Add docs and source").unwrap();

    let baseline = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(baseline.git_diff_added_lines, 2);

    let ignored = stats_from_args(
        &repo,
        &["stats", "HEAD", "--json", "--ignore", "docs/keep.txt"],
    );
    assert_eq!(ignored.git_diff_added_lines, 1);
}

#[test]
fn test_stats_range_uses_default_ignores() {
    let repo = TestRepo::new();
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    let first = repo.stage_all_and_commit("Initial commit").unwrap();

    repo.filename("src/main.rs")
        .set_contents(crate::lines!["fn main() {}".ai()]);
    repo.filename("Cargo.lock")
        .set_contents(vec!["lockdata".to_string(); 700]);
    let second = repo
        .stage_all_and_commit("Add source and lockfile")
        .unwrap();

    let range = format!("{}..{}", first.commit_sha, second.commit_sha);
    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("git-ai stats range should succeed");
    let json = extract_json_object(&raw);
    let range_stats: git_ai::authorship::range_authorship::RangeAuthorshipStats =
        serde_json::from_str(&json).unwrap();

    assert_eq!(range_stats.range_stats.git_diff_added_lines, 1);
    assert_eq!(range_stats.range_stats.ai_additions, 1);
}

#[test]
fn test_post_commit_large_ignored_files_do_not_trigger_skip_warning() {
    let repo = TestRepo::new();
    repo.filename("README.md")
        .set_contents(crate::lines!["# Repo"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    repo.filename("Cargo.lock")
        .set_contents(vec!["lockfile-entry".to_string(); 7001]);
    let commit = repo
        .stage_all_and_commit("Large lockfile update")
        .expect("commit should succeed");

    assert!(
        !commit
            .stdout
            .contains("Skipped git-ai stats for large commit"),
        "large ignored files should not trigger post-commit skip warning: {}",
        commit.stdout
    );

    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(stats.git_diff_added_lines, 0);
    assert_eq!(stats.ai_additions, 0);
    assert_eq!(stats.human_additions, 0);
}

#[test]
fn test_stats_ignores_renamed_files() {
    // Test that stats correctly ignores pure renames (no content changes)
    // Reproduces issue #923
    let repo = TestRepo::new();

    // Initial commit with files in a directory
    repo.filename("misc/Development Notes.md")
        .set_contents(crate::lines![
            "# Development Notes",
            "",
            "Some content here",
            "More content",
            "Even more",
            "Line 6",
            "Line 7",
            "Line 8",
            "Line 9",
            "Line 10",
            "Line 11",
            "Line 12",
            "Line 13",
            "Line 14",
            "Line 15",
            "Line 16"
        ]);
    repo.filename("misc/Usage Guide.md")
        .set_contents(crate::lines!["# Usage Guide", "", "Usage info"]);
    repo.stage_all_and_commit("Initial commit with misc directory")
        .unwrap();

    // Rename the directory (pure rename, no content changes)
    let misc_dev = repo.path().join("misc/Development Notes.md");
    let misc_usage = repo.path().join("misc/Usage Guide.md");
    let new_dir = repo.path().join("Misc Docs");
    fs::create_dir(&new_dir).unwrap();
    fs::rename(&misc_dev, new_dir.join("Development Notes.md")).unwrap();
    fs::rename(&misc_usage, new_dir.join("Usage Guide.md")).unwrap();
    fs::remove_dir(repo.path().join("misc")).unwrap();

    repo.stage_all_and_commit("Rename misc to Misc Docs")
        .unwrap();

    // Verify that git ai diff recognizes this as a rename
    let diff_output = repo.git_ai(&["diff", "HEAD"]).unwrap();
    assert!(
        diff_output.contains("similarity index 100%") || diff_output.contains("rename from"),
        "git ai diff should recognize pure renames"
    );

    // Stats should show 0 additions and 0 deletions for pure renames
    let stats = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
    assert_eq!(
        stats.git_diff_added_lines, 0,
        "Pure renames should not count as additions"
    );
    assert_eq!(
        stats.git_diff_deleted_lines, 0,
        "Pure renames should not count as deletions"
    );
    assert_eq!(stats.ai_additions, 0);
    assert_eq!(stats.human_additions, 0);
}

crate::reuse_tests_in_worktree!(
    test_authorship_log_stats,
    test_stats_cli_range,
    test_stats_cli_empty_tree_range,
    test_markdown_stats_deletion_only,
    test_markdown_stats_all_human,
    test_markdown_stats_all_ai,
    test_markdown_stats_mixed,
    test_markdown_stats_no_mixed,
    test_markdown_stats_minimal_human,
    test_markdown_stats_formatting,
    test_stats_default_ignores_snapshot_files,
    test_stats_default_ignores_lockfiles_and_generated_files,
    test_stats_ignores_linguist_generated_patterns,
    test_stats_keeps_negative_linguist_patterns_counted,
    test_stats_in_bare_clone_uses_root_gitattributes_linguist_generated,
    test_stats_ignore_flag_is_additive_to_defaults,
    test_stats_range_uses_default_ignores,
    test_post_commit_large_ignored_files_do_not_trigger_skip_warning,
    test_stats_ignores_renamed_files,
);
