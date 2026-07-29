use crate::repos::test_file::ExpectedLineExt;

use crate::test_utils::fixture_path;
use git_ai::authorship::attribution_tracker::LineAttribution;
use git_ai::authorship::authorship_log::PromptRecord;
use git_ai::authorship::stats::CommitStats;
use git_ai::authorship::working_log::{AgentId, CheckpointKind};
use git_ai::git::repository as GitAiRepository;
use insta::assert_debug_snapshot;
use rand::RngExt;
use regex::Regex;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn stats_from_args(repo: &crate::repos::test_repo::TestRepo, args: &[&str]) -> CommitStats {
    let raw = repo.git_ai(args).expect("git-ai stats should succeed");
    let start = raw.find('{').unwrap_or(0);
    let end = raw.rfind('}').unwrap_or(raw.len().saturating_sub(1));
    serde_json::from_str(&raw[start..=end]).expect("valid stats json")
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

fn run_git_stdout(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn expected_worktree_storage_prefix(main_repo_root: &Path) -> PathBuf {
    let git_common_dir = PathBuf::from(run_git_stdout(
        main_repo_root,
        &["rev-parse", "--git-common-dir"],
    ));
    let git_common_dir = if git_common_dir.is_relative() {
        main_repo_root.join(git_common_dir)
    } else {
        git_common_dir
    };
    git_common_dir.join("ai").join("worktrees")
}

fn canonicalize_for_assert(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_blame_output(blame_output: &str) -> String {
    let re_sha = Regex::new(r"[0-9a-f]{40}|[0-9a-f]{7,}").expect("valid sha regex");
    let result = re_sha.replace_all(blame_output, "COMMIT_SHA");
    let re_timestamp = Regex::new(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} [\+\-]\d{4}")
        .expect("valid timestamp regex");
    let result = re_timestamp.replace_all(&result, "TIMESTAMP");
    let re_author = Regex::new(r"\(([^)]+?)\s+TIMESTAMP").expect("valid author regex");
    re_author
        .replace_all(&result, "(AUTHOR TIMESTAMP")
        .to_string()
}

fn normalize_blame_for_format_parity(blame_output: &str) -> String {
    blame_output
        .lines()
        .map(|line| {
            if let Some(start_paren) = line.find('(')
                && let Some(end_paren) = line.rfind(')')
            {
                let prefix = &line[..start_paren];
                let suffix = &line[end_paren + 1..];
                return format!("{prefix}(META){suffix}");
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn unique_worktree_path() -> PathBuf {
    let mut rng = rand::rng();
    let n: u64 = rng.random_range(0..10_000_000_000);
    std::env::temp_dir().join(format!("git-ai-worktree-{}", n))
}

crate::worktree_test_wrappers! {
    fn repository_paths_and_storage_are_worktree_aware() {
        let repo = TestRepo::new();

        let common_dir = PathBuf::from(
            repo.git(&["rev-parse", "--git-common-dir"])
                .expect("resolve common dir")
                .trim(),
        );
        let git_dir = PathBuf::from(
            repo.git(&["rev-parse", "--git-dir"])
                .expect("resolve git dir")
                .trim(),
        );

        assert!(
            repo.path().join(".git").is_file(),
            "linked worktree should have a .git file"
        );

        let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
            .expect("find git-ai repository");
        assert_eq!(
            gitai_repo.workdir().unwrap().canonicalize().unwrap(),
            repo.path().canonicalize().unwrap(),
            "workdir should match linked worktree root"
        );
        assert_eq!(
            gitai_repo.path().canonicalize().unwrap(),
            git_dir.canonicalize().unwrap(),
            "git dir should match rev-parse --git-dir for linked worktree"
        );

        let expected_prefix = common_dir.join("ai").join("worktrees");
        assert!(
            gitai_repo.storage.working_logs.starts_with(&expected_prefix),
            "working logs should live under common-dir isolated storage: {}",
            gitai_repo.storage.working_logs.display()
        );
    }
}

crate::worktree_test_wrappers! {
    fn checkpoint_and_blame_support_absolute_paths_in_worktree() {
        let repo = TestRepo::new();
        let mut file = repo.filename("src/lib.rs");
        file.set_contents(crate::lines!["fn a() {}".human(), "fn ai() {}".ai()]);
        repo.stage_all_and_commit("add file with ai lines").unwrap();

        let abs_path = repo.path().join("src/lib.rs");
        let output = repo
            .git_ai(&["blame", abs_path.to_str().unwrap()])
            .expect("blame should work for absolute path in worktree");
        assert!(output.contains("fn ai() {}"));
    }
}

crate::worktree_test_wrappers! {
    fn blame_boundary_and_abbrev_match_git_in_worktree() {
        let repo = TestRepo::new();
        let mut file = repo.filename("boundary.txt");
        file.set_contents(crate::lines!["root line".human(), "line to change".human()]);
        repo.stage_all_and_commit("root commit").unwrap();

        file.set_contents(crate::lines!["root line".human(), "updated line".human()]);
        repo.stage_all_and_commit("second commit").unwrap();

        let git_output = repo
            .git(&["blame", "--abbrev=12", "-b", "boundary.txt"])
            .expect("git blame with boundary flags should succeed");
        let git_ai_output = repo
            .git_ai(&["blame", "--abbrev", "12", "-b", "boundary.txt"])
            .expect("git-ai blame with boundary flags should succeed");

        assert_eq!(
            normalize_blame_for_format_parity(&git_ai_output),
            normalize_blame_for_format_parity(&git_output),
            "git-ai blame should match git formatting for boundary and abbrev in worktrees"
        );

        let git_root_output = repo
            .git(&["blame", "--abbrev=12", "--root", "boundary.txt"])
            .expect("git blame --root should succeed");
        let git_ai_root_output = repo
            .git_ai(&["blame", "--abbrev", "12", "--root", "boundary.txt"])
            .expect("git-ai blame --root should succeed");

        assert_eq!(
            normalize_blame_for_format_parity(&git_ai_root_output),
            normalize_blame_for_format_parity(&git_root_output),
            "git-ai blame should match git formatting for --root and abbrev in worktrees"
        );
    }
}

crate::worktree_test_wrappers! {
    fn diff_works_in_worktree_context() {
        let repo = TestRepo::new();
        let mut file = repo.filename("diff.txt");
        file.set_contents(crate::lines!["old".human()]);
        repo.stage_all_and_commit("initial").unwrap();

        file.set_contents(crate::lines!["new".ai()]);
        let commit = repo.stage_all_and_commit("ai update").unwrap();

        let output = repo
            .git_ai(&["diff", &commit.commit_sha])
            .expect("git-ai diff should succeed in worktree");

        assert!(output.contains("diff.txt"));
        assert!(output.contains("+new"));
    }
}

crate::worktree_test_wrappers! {
    fn stash_pop_preserves_ai_authorship() {
        let repo = TestRepo::new();
        let file_path = repo.path().join("stash.txt");
        fs::write(&file_path, "base\n").unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "stash.txt"])
            .unwrap();
        let mut file = repo.filename("stash.txt");
        repo.stage_all_and_commit("base").unwrap();

        file.set_contents(crate::lines!["base".human(), "ai stash line".ai()]);
        repo.git(&["stash", "push", "-u", "-m", "wip"]).unwrap();
        repo.git(&["stash", "pop"]).unwrap();
        repo.stage_all_and_commit("apply stash").unwrap();

        file.assert_lines_and_blame(crate::lines!["base".human(), "ai stash line".ai()]);
    }
}

crate::worktree_test_wrappers! {
    fn reset_mixed_reconstructs_working_log() {
        let repo = TestRepo::new();
        let file_path = repo.path().join("reset.txt");
        fs::write(&file_path, "base\n").unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "reset.txt"])
            .unwrap();
        let mut file = repo.filename("reset.txt");
        repo.stage_all_and_commit("base").unwrap();

        file.set_contents(crate::lines!["base".human(), "ai reset line".ai()]);
        repo.stage_all_and_commit("ai commit").unwrap();

        repo.git(&["reset", "--mixed", "HEAD~1"])
            .expect("mixed reset should succeed");
        repo.stage_all_and_commit("recommit after reset").unwrap();

        file.assert_lines_and_blame(crate::lines!["base".human(), "ai reset line".ai()]);
    }
}

crate::worktree_test_wrappers! {
    fn rebase_preserves_ai_authorship() {
        let repo = TestRepo::new();
        let file_path = repo.path().join("rebase.txt");
        fs::write(&file_path, "base\n").unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "rebase.txt"])
            .unwrap();
        let mut file = repo.filename("rebase.txt");
        repo.stage_all_and_commit("base").unwrap();
        repo.git(&["checkout", "-b", "integration"]).unwrap();

        repo.git(&["checkout", "-b", "feature", "integration"]).unwrap();
        file.set_contents(crate::lines!["base".human(), "feature ai line".ai()]);
        repo.stage_all_and_commit("feature ai").unwrap();

        repo.git(&["checkout", "integration"]).unwrap();
        let mut main_only = repo.filename("main-only.txt");
        main_only.set_contents(crate::lines!["main human".human()]);
        repo.stage_all_and_commit("main human commit").unwrap();

        repo.git(&["checkout", "feature"]).unwrap();
        repo.git(&["rebase", "integration"]).unwrap();

        file.assert_lines_and_blame(crate::lines!["base".human(), "feature ai line".ai()]);
    }
}

crate::worktree_test_wrappers! {
    fn cherry_pick_preserves_ai_authorship() {
        let repo = TestRepo::new();
        let file_path = repo.path().join("cherry.txt");
        fs::write(&file_path, "base\n").unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", "cherry.txt"])
            .unwrap();
        let mut file = repo.filename("cherry.txt");
        repo.stage_all_and_commit("base").unwrap();
        repo.git(&["checkout", "-b", "integration"]).unwrap();

        repo.git(&["checkout", "-b", "feature", "integration"]).unwrap();
        file.set_contents(crate::lines!["base".human(), "feature ai".ai()]);
        let ai_commit = repo.stage_all_and_commit("feature ai").unwrap();

        repo.git(&["checkout", "integration"]).unwrap();
        repo.git(&["cherry-pick", &ai_commit.commit_sha]).unwrap();

        file.assert_lines_and_blame(crate::lines!["base".human(), "feature ai".ai()]);
    }
}

crate::worktree_test_wrappers! {
    fn multi_worktree_storage_isolation_prevents_cross_talk() {
        let repo = TestRepo::new();
        let common_dir = PathBuf::from(
            repo.git(&["rev-parse", "--git-common-dir"])
                .expect("resolve common dir")
                .trim(),
        );
        let main_repo_dir = common_dir.parent().expect("main repo dir");
        let second_worktree = unique_worktree_path();

        run_git(
            main_repo_dir,
            &["worktree", "add", second_worktree.to_str().unwrap()],
        );

        let repo_one = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
            .expect("find first worktree repo");
        let repo_two =
            GitAiRepository::find_repository_in_path(second_worktree.to_str().unwrap())
                .expect("find second worktree repo");

        let expected_prefix = common_dir.join("ai").join("worktrees");
        assert!(repo_one.storage.working_logs.starts_with(&expected_prefix));
        assert!(repo_two.storage.working_logs.starts_with(&expected_prefix));
        assert_ne!(
            repo_one.storage.working_logs,
            repo_two.storage.working_logs,
            "distinct linked worktrees must not share the same working_logs path"
        );

        let wl_one = repo_one.storage.working_log_for_base_commit("initial").unwrap();
        let wl_two = repo_two.storage.working_log_for_base_commit("initial").unwrap();
        fs::write(wl_one.dir.join("sentinel"), "one").expect("write sentinel one");
        assert!(
            !wl_two.dir.join("sentinel").exists(),
            "worktree-local storage should remain isolated"
        );
    }
}

crate::worktree_test_wrappers! {
    fn worktree_initial_attributions_snapshot() {
        let repo = TestRepo::new();

        let mut readme = repo.filename("README.md");
        readme.set_contents(crate::lines!["# Test Repo"]);
        repo.stage_all_and_commit("initial commit").unwrap();

        let working_log = repo.current_working_logs();
        let mut initial_attributions = HashMap::new();
        initial_attributions.insert(
            "initial.txt".to_string(),
            vec![LineAttribution {
                start_line: 1,
                end_line: 2,
                author_id: "initial-ai-1".to_string(),
                overrode: None,
            }],
        );
        let mut prompts = HashMap::new();
        prompts.insert(
            "initial-ai-1".to_string(),
            PromptRecord {
                agent_id: AgentId {
                    tool: "test-tool".to_string(),
                    id: "session-1".to_string(),
                    model: "test-model".to_string(),
                },
                human_author: None,
                total_additions: 0,
                total_deletions: 0,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
            messages_url: None,
            },
        );
        let file_content = "a\nb\n";
        let mut initial_contents = HashMap::new();
        initial_contents.insert("initial.txt".to_string(), file_content.to_string());
        working_log
            .write_initial_attributions_with_contents(
                initial_attributions,
                prompts,
                BTreeMap::new(),
                initial_contents,
                BTreeMap::new(),
            )
            .expect("write initial attributions");

        fs::write(repo.path().join("initial.txt"), file_content).expect("write file");
        repo.git_ai(&["checkpoint"]).unwrap();
        repo.stage_all_and_commit("commit initial attribution")
            .unwrap();

        let blame_output = repo.git_ai(&["blame", "initial.txt"]).unwrap();
        let normalized = normalize_blame_output(&blame_output);
        assert_debug_snapshot!(normalized);
    }
}

crate::worktree_test_wrappers! {
    fn worktree_stats_snapshot() {
        let repo = TestRepo::new();
        let mut file = repo.filename("stats.txt");
        file.set_contents(crate::lines!["one".human(), "two".ai(), "three".ai()]);
        repo.stage_all_and_commit("stats seed").unwrap();

        let stats = repo.stats().expect("stats should succeed");
        assert_eq!(stats.unknown_additions, 0);
        assert_eq!(stats.human_additions + stats.ai_additions, 3);
        assert_eq!(stats.git_diff_added_lines, 3);
        assert_eq!(stats.git_diff_deleted_lines, 0);
    }
}

crate::worktree_test_wrappers! {
    fn stats_head_arg_uses_worktree_head() {
        // Regression test for issue #285: `git-ai stats head` in a worktree was
        // resolving HEAD to the *main* repository's HEAD instead of the worktree's
        // own HEAD, so it reported 100% human even for AI-heavy commits.
        let repo = TestRepo::new();

        // Make an AI-only commit in the worktree. At this point the worktree's
        // HEAD has diverged from the main repo's initial (empty) commit.
        let mut file = repo.filename("ai_work.txt");
        file.set_contents(crate::lines!["line_a".ai(), "line_b".ai()]);
        let commit = repo.stage_all_and_commit("ai commit in worktree").unwrap();

        // Sanity: the explicit-SHA path works correctly.
        let stats_by_sha = stats_from_args(
            &repo,
            &["stats", &commit.commit_sha, "--json"],
        );
        assert!(
            stats_by_sha.git_diff_added_lines > 0,
            "explicit SHA stats should see the 2 added lines"
        );

        // `stats HEAD` (uppercase) must resolve to the worktree's HEAD, not the
        // main repo's initial empty commit.
        let stats_head_upper = stats_from_args(&repo, &["stats", "HEAD", "--json"]);
        assert_eq!(
            stats_head_upper.git_diff_added_lines,
            stats_by_sha.git_diff_added_lines,
            "`stats HEAD` (uppercase) must match `stats <sha>`: got {} vs {}",
            stats_head_upper.git_diff_added_lines,
            stats_by_sha.git_diff_added_lines,
        );

        // `stats head` (lowercase) must behave identically to `stats HEAD` on
        // all platforms.  Before this fix, on case-insensitive filesystems
        // (macOS) 'head' could resolve to the *main* repo's HEAD instead of the
        // worktree's own HEAD; on case-sensitive Linux it was rejected outright.
        // The fix normalises 'head' → 'HEAD' before the git call.
        let stats_head_lower = stats_from_args(&repo, &["stats", "head", "--json"]);
        assert_eq!(
            stats_head_lower.git_diff_added_lines,
            stats_by_sha.git_diff_added_lines,
            "`stats head` (lowercase) must match `stats <sha>`: got {} vs {}",
            stats_head_lower.git_diff_added_lines,
            stats_by_sha.git_diff_added_lines,
        );
    }
}

// ── Linked-worktree checkpoint routing ──────────────────────────────────────
//
// These tests reproduce the bug where an agent whose CWD is the *main* repo
// writes a file into a *linked* worktree (created with `git worktree add`).
// Before the fix, git-ai would fail to store any checkpoint because:
//   1. It opened the main repo (via CWD), and
//   2. `git status <file>` from the main repo returns nothing for files that
//      live inside a linked worktree's working tree.
// After the fix, git-ai detects that the edited file is outside the main
// repo's boundary and falls back to per-file repository discovery, routing
// the checkpoint to the linked worktree's isolated storage.

/// Helper: simulate a PostToolUse (AiAgent) Claude Code hook call where the
/// session CWD is `session_cwd` but the file being written lives in a
/// *different* directory (`file_path`).  Returns the git-ai stdout+stderr.
fn simulate_claude_post_tool_use(
    repo: &crate::repos::test_repo::TestRepo,
    session_cwd: &Path,
    file_path: &Path,
) -> Result<String, String> {
    let transcript = fixture_path("example-claude-code.jsonl");
    let hook_input = json!({
        "cwd": session_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai_with_stdin(
        &["checkpoint", "claude", "--hook-input", "stdin"],
        hook_input.as_bytes(),
    )
}

/// Helper: simulate a PreToolUse (Human) Claude Code hook call.
fn simulate_claude_pre_tool_use(
    repo: &crate::repos::test_repo::TestRepo,
    session_cwd: &Path,
    file_path: &Path,
) -> Result<String, String> {
    let transcript = fixture_path("example-claude-code.jsonl");
    let hook_input = json!({
        "cwd": session_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        },
        "transcript_path": transcript.to_string_lossy().to_string()
    })
    .to_string();

    repo.git_ai_with_stdin(
        &["checkpoint", "claude", "--hook-input", "stdin"],
        hook_input.as_bytes(),
    )
}

#[test]
fn checkpoint_routes_to_linked_worktree_when_cwd_is_main_repo() {
    // Setup: main repo with an initial commit so we have a HEAD SHA.
    let repo = crate::repos::test_repo::TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(crate::lines!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();

    // Resolve the main repo's root (the TestRepo is already a linked worktree
    // in worktree_test_wrappers!, but here we use the plain TestRepo whose CWD
    // is the repo root itself).
    let main_repo_root = repo.path().to_path_buf();

    // Create a second linked worktree alongside the main working tree.
    let linked_wt = unique_worktree_path();
    run_git(
        &main_repo_root,
        &["worktree", "add", linked_wt.to_str().unwrap()],
    );

    // Write a file inside the linked worktree.
    let wt_file = linked_wt.join("feature.rs");
    fs::write(
        &wt_file,
        "fn feature() {}\nfn feature2() {}\nfn feature3() {}\n",
    )
    .expect("write wt file");

    // Simulate PostToolUse hook: CWD = main repo, file = linked worktree.
    // This is the exact scenario that caused BUG-A.
    simulate_claude_post_tool_use(&repo, &main_repo_root, &wt_file)
        .expect("checkpoint should succeed");

    // The checkpoint must land in the *linked worktree's* isolated storage,
    // not in the main repo's plain working_logs.
    let wt_repo = GitAiRepository::find_repository_in_path(linked_wt.to_str().unwrap())
        .expect("find linked worktree repo");

    // Storage must be rooted under the repository's shared git-common-dir.
    // We ask git for that path directly instead of canonicalizing the repo
    // root, because Windows canonicalize() uses extended-length path syntax.
    let expected_storage_prefix =
        canonicalize_for_assert(&expected_worktree_storage_prefix(&main_repo_root));
    let actual_storage = canonicalize_for_assert(&wt_repo.storage.working_logs);
    assert!(
        actual_storage.starts_with(&expected_storage_prefix),
        "linked worktree storage should be under git-common-dir/ai/worktrees:\nexpected prefix: {}\nactual: {}",
        expected_storage_prefix.display(),
        actual_storage.display()
    );

    let commit_sha = wt_repo
        .head()
        .expect("wt repo should have a HEAD")
        .target()
        .expect("HEAD should resolve");
    let wt_working_log = repo.working_logs_for_repo_path_and_base_commit(&linked_wt, &commit_sha);

    let checkpoints = wt_working_log
        .read_all_checkpoints()
        .expect("read checkpoints");

    assert!(
        !checkpoints.is_empty(),
        "expected at least one checkpoint in the linked worktree's working log"
    );

    let ai_checkpoint = checkpoints
        .iter()
        .find(|cp| cp.kind == CheckpointKind::AiAgent)
        .expect("expected an AiAgent checkpoint for the Write");

    assert_eq!(
        ai_checkpoint.entries.len(),
        1,
        "checkpoint should cover exactly the written file"
    );
    assert_eq!(
        ai_checkpoint.entries[0].file, "feature.rs",
        "checkpoint entry should use the worktree-relative path"
    );

    // Cleanup the temporary worktree.
    run_git(
        &main_repo_root,
        &["worktree", "remove", "--force", linked_wt.to_str().unwrap()],
    );
}

/// Same as `checkpoint_routes_to_linked_worktree_when_cwd_is_main_repo` but the linked
/// worktree lives *inside* the main repo's working tree (e.g. `.worktrees/feature`).
///
/// This is the exact structure that caused Bug-A / Bug-B: `path_is_in_workdir` returned
/// `true` because the file's path starts with the main repo's workdir, and only the
/// `.git` FILE detection (is_linked_worktree_git_file) distinguishes it from a regular
/// subdirectory.  Without the fix this test fails silently — the checkpoint is skipped
/// and `checkpoints.jsonl` remains empty.
#[test]
fn checkpoint_routes_to_nested_linked_worktree_when_cwd_is_main_repo() {
    let repo = crate::repos::test_repo::TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(crate::lines!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();

    let main_repo_root = repo.path().to_path_buf();

    // Worktree lives INSIDE the main repo's working tree.
    let linked_wt = main_repo_root.join(".worktrees").join("feature");
    run_git(
        &main_repo_root,
        &["worktree", "add", "--detach", linked_wt.to_str().unwrap()],
    );

    // Sanity: the .git file inside the nested worktree must point to /worktrees/
    let dot_git_content =
        fs::read_to_string(linked_wt.join(".git")).expect("read nested worktree .git file");
    assert!(
        dot_git_content.contains("/worktrees/"),
        "nested worktree .git file should contain /worktrees/: {}",
        dot_git_content.trim()
    );

    let wt_file = linked_wt.join("feature.rs");
    fs::write(
        &wt_file,
        "fn feature() {}\nfn feature2() {}\nfn feature3() {}\n",
    )
    .expect("write wt file");

    // PostToolUse: CWD = main repo, file = *nested* linked worktree.
    // Before the fix, path_is_in_workdir wrongly returned true here
    // (the file path starts_with the main workdir), so git status found
    // nothing and the checkpoint was silently dropped.
    simulate_claude_post_tool_use(&repo, &main_repo_root, &wt_file)
        .expect("checkpoint should succeed");

    let wt_repo = GitAiRepository::find_repository_in_path(linked_wt.to_str().unwrap())
        .expect("find nested worktree repo");

    // Storage must be rooted under the repository's shared git-common-dir.
    // We ask git for that path directly instead of canonicalizing the repo
    // root, because Windows canonicalize() uses extended-length path syntax.
    let expected_storage_prefix =
        canonicalize_for_assert(&expected_worktree_storage_prefix(&main_repo_root));
    let actual_storage = canonicalize_for_assert(&wt_repo.storage.working_logs);
    assert!(
        actual_storage.starts_with(&expected_storage_prefix),
        "nested worktree storage should be under git-common-dir/ai/worktrees:\nexpected prefix: {}\nactual: {}",
        expected_storage_prefix.display(),
        actual_storage.display()
    );

    let commit_sha = wt_repo
        .head()
        .expect("wt repo should have a HEAD")
        .target()
        .expect("HEAD should resolve");
    let wt_working_log = repo.working_logs_for_repo_path_and_base_commit(&linked_wt, &commit_sha);

    let checkpoints = wt_working_log
        .read_all_checkpoints()
        .expect("read checkpoints");

    assert!(
        !checkpoints.is_empty(),
        "expected a checkpoint in the nested worktree's working log — \
         if this fails the .git FILE boundary detection is broken"
    );

    let ai_checkpoint = checkpoints
        .iter()
        .find(|cp| cp.kind == CheckpointKind::AiAgent)
        .expect("expected an AiAgent checkpoint for the Write");

    assert_eq!(
        ai_checkpoint.entries.len(),
        1,
        "checkpoint should cover exactly the written file"
    );
    assert_eq!(
        ai_checkpoint.entries[0].file, "feature.rs",
        "checkpoint entry should use the worktree-relative path"
    );

    run_git(
        &main_repo_root,
        &["worktree", "remove", "--force", linked_wt.to_str().unwrap()],
    );
}

#[test]
fn human_checkpoint_routes_to_linked_worktree_when_cwd_is_main_repo() {
    // Same scenario as above, but for the PreToolUse (Human) hook.
    let repo = crate::repos::test_repo::TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(crate::lines!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();

    let main_repo_root = repo.path().to_path_buf();

    let linked_wt = unique_worktree_path();
    run_git(
        &main_repo_root,
        &["worktree", "add", linked_wt.to_str().unwrap()],
    );

    // Write a file in the worktree before the hook so there's an "uncaptured zone".
    let wt_file = linked_wt.join("preexisting.rs");
    fs::write(&wt_file, "fn old() {}\n").expect("write wt file");

    // PreToolUse: informs git-ai that this file is *about to* be edited.
    simulate_claude_pre_tool_use(&repo, &main_repo_root, &wt_file)
        .expect("pre-tool-use checkpoint should succeed");

    // The PreToolUse (Human) checkpoint should land in the worktree log.
    let wt_repo = GitAiRepository::find_repository_in_path(linked_wt.to_str().unwrap())
        .expect("find linked worktree repo");

    let commit_sha = wt_repo
        .head()
        .expect("wt repo should have a HEAD")
        .target()
        .expect("HEAD should resolve");
    let wt_working_log = repo.working_logs_for_repo_path_and_base_commit(&linked_wt, &commit_sha);

    let checkpoints = wt_working_log
        .read_all_checkpoints()
        .expect("read checkpoints");

    // A Human checkpoint is only stored when there is an uncaptured zone
    // (i.e., content not yet attributed).  With the pre-existing file content
    // and no prior AI checkpoint, a Human entry should be recorded.
    let has_human = checkpoints
        .iter()
        .any(|cp| cp.kind == CheckpointKind::Human);
    assert!(
        has_human,
        "expected a Human checkpoint in the linked worktree's working log"
    );

    run_git(
        &main_repo_root,
        &["worktree", "remove", "--force", linked_wt.to_str().unwrap()],
    );
}

/// Same as `human_checkpoint_routes_to_linked_worktree_when_cwd_is_main_repo` but
/// the worktree lives *inside* the main repo's working tree.  Without the
/// is_linked_worktree_git_file fix, path_is_in_workdir returns true, the Human
/// checkpoint is processed against the wrong repo, and this test fails.
#[test]
fn human_checkpoint_routes_to_nested_linked_worktree_when_cwd_is_main_repo() {
    let repo = crate::repos::test_repo::TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(crate::lines!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();

    let main_repo_root = repo.path().to_path_buf();

    // Worktree is INSIDE the main repo's working tree.
    let linked_wt = main_repo_root.join(".worktrees").join("pre-feature");
    run_git(
        &main_repo_root,
        &["worktree", "add", "--detach", linked_wt.to_str().unwrap()],
    );

    let wt_file = linked_wt.join("preexisting.rs");
    fs::write(&wt_file, "fn old() {}\n").expect("write wt file");

    simulate_claude_pre_tool_use(&repo, &main_repo_root, &wt_file)
        .expect("pre-tool-use checkpoint should succeed");

    let wt_repo = GitAiRepository::find_repository_in_path(linked_wt.to_str().unwrap())
        .expect("find nested worktree repo");

    let commit_sha = wt_repo
        .head()
        .expect("wt repo should have a HEAD")
        .target()
        .expect("HEAD should resolve");
    let wt_working_log = repo.working_logs_for_repo_path_and_base_commit(&linked_wt, &commit_sha);

    let checkpoints = wt_working_log
        .read_all_checkpoints()
        .expect("read checkpoints");

    let has_human = checkpoints
        .iter()
        .any(|cp| cp.kind == CheckpointKind::Human);
    assert!(
        has_human,
        "expected a Human checkpoint in the nested worktree's working log — \
         if this fails the .git FILE boundary detection is broken for PreToolUse"
    );

    run_git(
        &main_repo_root,
        &["worktree", "remove", "--force", linked_wt.to_str().unwrap()],
    );
}
