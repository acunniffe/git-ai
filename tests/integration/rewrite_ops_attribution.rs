/// Deterministic regression tests for attribution bugs found by the fuzzer
/// on the rewrite-ops branch. Each test models a specific fuzzer failure pattern
/// using explicit file writes and checkpoint calls.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use git_ai::authorship::attribution_tracker::LineAttribution;
use git_ai::git::repo_storage::InitialAttributions;

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;

// =============================================================================
// Category 0: Trace2 ref-cursor branch lifecycle
// =============================================================================

/// Deleting a branch removes its reflog file. Recreating the same branch name
/// starts a new reflog generation at byte 0, so the daemon cursor must clear any
/// offset it learned from the previous generation.
#[test]
fn test_branch_delete_recreate_resets_trace2_ref_cursor() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines!["base".ai()]);

    let main_branch = repo.current_branch();
    fs::write(&file_path, "base\nmain\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("main advance").unwrap();
    file.assert_committed_lines(crate::lines!["base".ai(), "main".ai()]);

    let initial = repo.git(&["rev-parse", "HEAD~1"]).unwrap();
    let initial = initial.trim().to_string();
    repo.git(&["checkout", "-b", "rebase-side", initial.as_str()])
        .unwrap();
    fs::write(&file_path, "base\nside\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("side advance").unwrap();
    file.assert_committed_lines(crate::lines!["base".ai(), "side".ai()]);

    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["branch", "-D", "rebase-side"]).unwrap();

    repo.git(&["checkout", "-b", "rebase-side", initial.as_str()])
        .unwrap();
    file.assert_committed_lines(crate::lines!["base".ai()]);
}

/// If an out-of-band raw git commit moves HEAD without trace2/hook handling,
/// the next traced commit must not consume that stale HEAD reflog entry as its
/// own ref transition.
#[test]
fn test_raw_git_commit_before_traced_commit_does_not_poison_ref_cursor() {
    let repo = TestRepo::new();

    let base_path = repo.path().join("base.txt");
    fs::write(&base_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "base.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let raw_path = repo.path().join("raw.txt");
    fs::write(&raw_path, "raw human\n").unwrap();
    repo.git_og(&["add", "raw.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "raw human commit"]).unwrap();

    let ai_path = repo.path().join("ai.txt");
    fs::write(&ai_path, "ai tracked\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai.txt"]).unwrap();
    repo.stage_all_and_commit("ai tracked commit").unwrap();

    let mut ai_file = repo.filename("ai.txt");
    ai_file.assert_committed_lines(crate::lines!["ai tracked".ai()]);
}

#[test]
fn test_daemon_reports_post_commit_side_effect_error() {
    let repo = TestRepo::new();

    let base_path = repo.path().join("base.txt");
    fs::write(&base_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let working_log = repo.current_working_logs();
    let mut files = HashMap::new();
    files.insert(
        "broken.txt".to_string(),
        vec![LineAttribution {
            start_line: 1,
            end_line: 1,
            author_id: "missing-snapshot-ai".to_string(),
            overrode: None,
        }],
    );
    working_log
        .write_initial(InitialAttributions {
            files,
            ..InitialAttributions::default()
        })
        .unwrap();

    fs::write(repo.path().join("broken.txt"), "broken\n").unwrap();
    repo.git(&["add", "broken.txt"]).unwrap();
    repo.git(&["commit", "-m", "commit with broken initial"])
        .unwrap();

    let sync = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        repo.sync_daemon_force();
    }));
    assert!(
        sync.is_err(),
        "post-commit side-effect failure must be reported through daemon sync"
    );
}

#[test]
fn test_revert_older_commit_restores_original_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("revert.txt");

    fs::write(&file_path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert.txt"])
        .unwrap();
    fs::write(&file_path, "keep\nrestored ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "revert.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial mixed attribution")
        .unwrap();

    let mut file = repo.filename("revert.txt");
    file.assert_committed_lines(crate::lines!["keep".human(), "restored ai".ai()]);

    fs::write(&file_path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai line").unwrap();
    let delete_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    file.assert_committed_lines(crate::lines!["keep".human()]);

    fs::write(repo.path().join("advance.txt"), "later human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "advance.txt"])
        .unwrap();
    repo.stage_all_and_commit("later unrelated commit").unwrap();
    let mut advance = repo.filename("advance.txt");
    advance.assert_committed_lines(crate::lines!["later human".human()]);

    repo.git(&["revert", &delete_commit]).unwrap();
    file.assert_committed_lines(crate::lines!["keep".human(), "restored ai".ai()]);
}

#[test]
fn test_revert_revision_expression_restores_original_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("revert_expr.txt");

    fs::write(&file_path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert_expr.txt"])
        .unwrap();
    fs::write(&file_path, "keep\nrestored ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "revert_expr.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial mixed attribution")
        .unwrap();

    let mut file = repo.filename("revert_expr.txt");
    file.assert_committed_lines(crate::lines!["keep".human(), "restored ai".ai()]);

    fs::write(&file_path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert_expr.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai line").unwrap();
    file.assert_committed_lines(crate::lines!["keep".human()]);

    fs::write(repo.path().join("advance.txt"), "later human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "advance.txt"])
        .unwrap();
    repo.stage_all_and_commit("later unrelated commit").unwrap();
    repo.filename("advance.txt")
        .assert_committed_lines(crate::lines!["later human".human()]);

    repo.git(&["revert", "HEAD~1"]).unwrap();
    file.assert_committed_lines(crate::lines!["keep".human(), "restored ai".ai()]);
}

/// Multi-commit `git revert <del_a> <del_b> <del_c>` (one invocation, several
/// destinations) exercises the per-destination revert loop. Each reverted
/// "delete" commit must restore its file's original AI attribution. This pins
/// the behavior so the per-commit revert work can be batched without regression.
#[test]
fn test_revert_multiple_commits_restores_each_original_attribution() {
    let repo = TestRepo::new();
    let fa = repo.path().join("a.txt");
    let fb = repo.path().join("b.txt");
    let fc = repo.path().join("c.txt");

    // Base: three files, each one human line.
    fs::write(&fa, "a base\n").unwrap();
    fs::write(&fb, "b base\n").unwrap();
    fs::write(&fc, "c base\n").unwrap();
    repo.stage_all_and_commit("base three files").unwrap();

    // Add an AI line to each file (committed once so attribution is recorded).
    fs::write(&fa, "a base\nAI a\n").unwrap();
    fs::write(&fb, "b base\nAI b\n").unwrap();
    fs::write(&fc, "c base\nAI c\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "a.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "b.txt"]).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "c.txt"]).unwrap();
    repo.stage_all_and_commit("add ai lines").unwrap();
    repo.filename("a.txt")
        .assert_committed_lines(crate::lines!["a base".human(), "AI a".ai()]);

    // Delete each AI line in its own commit → three separate "delete" commits,
    // each touching a different file (no conflicts when reverted together).
    fs::write(&fa, "a base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "a.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai a").unwrap();
    let del_a = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    fs::write(&fb, "b base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "b.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai b").unwrap();
    let del_b = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    fs::write(&fc, "c base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "c.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai c").unwrap();
    let del_c = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Revert all three deletes in ONE command → one revert command, three
    // destination commits processed by the batched revert path. The revert
    // CONTENT is restored for every file (each AI line comes back), and the
    // Every destination recovers history-aware source provenance. Later source
    // bases may not contain the original AI note directly, so the batched
    // fallback searches reachable notes from the pre-revert tip without adding
    // per-commit Git spawns.
    repo.git(&["revert", "--no-edit", &del_a, &del_b, &del_c])
        .unwrap();

    // Content restored for all three files.
    let a = repo.read_file("a.txt").unwrap();
    let b = repo.read_file("b.txt").unwrap();
    let c = repo.read_file("c.txt").unwrap();
    assert!(a.contains("AI a"), "a.txt content restored");
    assert!(b.contains("AI b"), "b.txt content restored");
    assert!(c.contains("AI c"), "c.txt content restored");

    repo.filename("a.txt")
        .assert_committed_lines(crate::lines!["a base".human(), "AI a".ai()]);
    repo.filename("b.txt")
        .assert_committed_lines(crate::lines!["b base".human(), "AI b".ai()]);
    repo.filename("c.txt")
        .assert_committed_lines(crate::lines!["c base".human(), "AI c".ai()]);
}

#[test]
fn test_revert_restored_ai_attribution_survives_shifted_line_numbers() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("revert_shift.txt");

    fs::write(&file_path, "keep\nrestored ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "revert_shift.txt"])
        .unwrap();
    repo.stage_all_and_commit("source ai line").unwrap();
    let mut file = repo.filename("revert_shift.txt");
    file.assert_committed_lines(crate::lines!["keep".ai(), "restored ai".ai()]);

    fs::write(&file_path, "keep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert_shift.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete ai line").unwrap();
    let delete_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    file.assert_committed_lines(crate::lines!["keep".ai()]);

    fs::write(&file_path, "later human\nkeep\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "revert_shift.txt"])
        .unwrap();
    repo.stage_all_and_commit("prepend later human line")
        .unwrap();
    file.assert_committed_lines(crate::lines!["later human".human(), "keep".ai()]);

    repo.git(&["revert", &delete_commit]).unwrap();
    file.assert_committed_lines(crate::lines![
        "later human".human(),
        "keep".ai(),
        "restored ai".ai(),
    ]);
}

fn commit_ai_line(repo: &TestRepo, filename: &str, line: &str, message: &str) {
    let path = repo.path().join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, format!("{line}\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", filename]).unwrap();
    repo.stage_all_and_commit(message).unwrap();

    let mut file = repo.filename(filename);
    file.assert_committed_lines(crate::lines![line.ai()]);
}

fn claude_checkpoint(repo: &TestRepo, event: &str, file_path: &Path, session_id: &str) {
    let transcript_path = repo.path().join(format!("{session_id}.jsonl"));
    if !transcript_path.exists() {
        fs::write(&transcript_path, "").unwrap();
    }
    let hook_input = json!({
        "cwd": repo.path().to_string_lossy().to_string(),
        "hook_event_name": event,
        "tool_name": "Edit",
        "session_id": session_id,
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_use_id": format!("{session_id}-{event}"),
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();

    repo.git_ai(&["checkpoint", "claude", "--hook-input", &hook_input])
        .unwrap_or_else(|error| panic!("claude {event} checkpoint failed: {error}"));
}

struct DelayedAiCommit {
    repo: TestRepo,
}

fn delayed_ai_commit_without_harness_sync_with_delay(delay_ms: u64) -> DelayedAiCommit {
    let delay_spec = format!("commit={delay_ms}");
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        delay_spec.as_str(),
    )]);
    let file_path = repo.path().join("reader.txt");
    fs::write(&file_path, "AI reader line\n").unwrap();
    claude_checkpoint(&repo, "PostToolUse", &file_path, "reader-session");
    repo.git(&["add", "reader.txt"]).unwrap();
    repo.git_without_test_sync_for_test(&["commit", "-m", "delayed ai reader commit"], &[])
        .unwrap();

    DelayedAiCommit { repo }
}

#[test]
fn test_production_show_does_not_hidden_sync_pending_commit_authorship_side_effect() {
    let fixture = delayed_ai_commit_without_harness_sync_with_delay(5000);

    let output = fixture
        .repo
        .git_ai_without_pre_sync_for_test(&["show", "HEAD"])
        .expect("immediate show should succeed");

    assert!(
        !output.contains("\"tool\":\"claude\"") && !output.contains("\"tool\": \"claude\""),
        "production show performed a hidden daemon sync before rendering:\n{output}"
    );
}

#[test]
fn test_named_branch_conflict_rebase_keeps_feature_side_ai_attribution() {
    let repo = TestRepo::new();
    let animals_path = repo.path().join("jokes-animals.csv");
    let dad_path = repo.path().join("jokes-dad.csv");

    let animals_base = "\
setup,punchline
What do you call a bear with no teeth?,A gummy bear
Why did the chicken go to the movie?,To see the hen-ema
What do you call an alligator in a vest?,An investigator
";
    let dad_base = "\
setup,punchline
Why don't scientists trust atoms?,Because they make up everything
What did the ocean say to the beach?,Nothing it just waved
";

    fs::write(&animals_path, animals_base).unwrap();
    fs::write(&dad_path, dad_base).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-dad.csv"])
        .unwrap();
    repo.stage_all_and_commit("base jokes").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "scenario-3-multi-file-conflict"])
        .unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What do you call a sleeping bull?,A dozer\n"),
    )
    .unwrap();
    fs::write(
        &dad_path,
        format!("{dad_base}What do you call a bear in the rain?,A drizzly bear\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-dad.csv"])
        .unwrap();
    repo.stage_all_and_commit("Add bull and drizzly bear jokes")
        .unwrap();
    let feature_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What's a cat's favorite color?,Purr-ple\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.stage_all_and_commit("Add purple cat joke").unwrap();

    let rebase_result = repo.git(&["rebase", &main_branch, "scenario-3-multi-file-conflict"]);
    assert!(
        rebase_result.is_err(),
        "named-branch rebase should conflict on jokes-animals.csv"
    );

    fs::write(
        &animals_path,
        format!(
            "{animals_base}What's a cat's favorite color?,Purr-ple\nWhat do you call a sleeping bull?,A dozer\n"
        ),
    )
    .unwrap();
    repo.git(&["add", "jokes-animals.csv"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let rebased_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(
        rebased_head, feature_commit,
        "rebase should rewrite the feature commit"
    );
    let note = repo
        .read_authorship_note(&rebased_head)
        .expect("rebased feature commit should have an authorship note");
    assert!(
        note.contains("jokes-animals.csv"),
        "rebased note should retain conflict-file attribution: {note}"
    );

    let mut animals = repo.filename("jokes-animals.csv");
    animals.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "What do you call a bear with no teeth?,A gummy bear".ai(),
        "Why did the chicken go to the movie?,To see the hen-ema".ai(),
        "What do you call an alligator in a vest?,An investigator".ai(),
        "What's a cat's favorite color?,Purr-ple".ai(),
        "What do you call a sleeping bull?,A dozer".ai(),
    ]);
}

#[test]
fn test_named_branch_conflict_rebase_agent_keep_both_without_intermediate_sync() {
    let repo = TestRepo::new();
    let animals_path = repo.path().join("jokes-animals.csv");
    let dad_path = repo.path().join("jokes-dad.csv");

    let animals_base = "\
setup,punchline
What do you call a bear with no teeth?,A gummy bear
Why did the chicken go to the movie?,To see the hen-ema
What do you call an alligator in a vest?,An investigator
";
    let dad_base = "\
setup,punchline
Why don't scientists trust atoms?,Because they make up everything
What did the ocean say to the beach?,Nothing it just waved
";

    fs::write(&animals_path, animals_base).unwrap();
    fs::write(&dad_path, dad_base).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-dad.csv"])
        .unwrap();
    repo.stage_all_and_commit("base jokes").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "scenario-3-multi-file-conflict"])
        .unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What do you call a sleeping bull?,A dozer\n"),
    )
    .unwrap();
    fs::write(
        &dad_path,
        format!("{dad_base}What do you call a bear in the rain?,A drizzly bear\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-dad.csv"])
        .unwrap();
    repo.stage_all_and_commit("Add bull and drizzly bear jokes")
        .unwrap();

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What's a cat's favorite color?,Purr-ple\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.stage_all_and_commit("Add purple cat joke").unwrap();

    let rebase_result = repo.git(&["rebase", &main_branch, "scenario-3-multi-file-conflict"]);
    assert!(
        rebase_result.is_err(),
        "named-branch rebase should conflict on jokes-animals.csv"
    );

    // This mirrors a real agent edit: a pre-edit checkpoint first captures the
    // conflict-marker file, then the AI checkpoint captures the resolved file.
    repo.git_ai(&["checkpoint", "human", "jokes-animals.csv"])
        .unwrap();
    fs::write(
        &animals_path,
        format!(
            "{animals_base}What's a cat's favorite color?,Purr-ple\nWhat do you call a sleeping bull?,A dozer\n"
        ),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-animals.csv"])
        .unwrap();
    repo.git(&["add", "jokes-animals.csv"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let mut animals = repo.filename("jokes-animals.csv");
    animals.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "What do you call a bear with no teeth?,A gummy bear".ai(),
        "Why did the chicken go to the movie?,To see the hen-ema".ai(),
        "What do you call an alligator in a vest?,An investigator".ai(),
        "What's a cat's favorite color?,Purr-ple".ai(),
        "What do you call a sleeping bull?,A dozer".ai(),
    ]);
}

#[test]
fn test_named_branch_conflict_rebase_real_claude_keep_both_without_intermediate_sync() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_DELAY_SIDE_EFFECT_MS_FOR_COMMAND",
        "rebase=1500",
    )]);
    let animals_path = repo.path().join("jokes-animals.csv");
    let dad_path = repo.path().join("jokes-dad.csv");

    let animals_base = "\
setup,punchline
What do you call a bear with no teeth?,A gummy bear
Why did the chicken go to the movie?,To see the hen-ema
What do you call an alligator in a vest?,An investigator
";
    let dad_base = "\
setup,punchline
Why don't scientists trust atoms?,Because they make up everything
What did the ocean say to the beach?,Nothing it just waved
";

    fs::write(&animals_path, animals_base).unwrap();
    fs::write(&dad_path, dad_base).unwrap();
    claude_checkpoint(&repo, "PostToolUse", &animals_path, "setup-session");
    claude_checkpoint(&repo, "PostToolUse", &dad_path, "setup-session");
    repo.stage_all_and_commit("base jokes").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "scenario-3-multi-file-conflict"])
        .unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What do you call a sleeping bull?,A dozer\n"),
    )
    .unwrap();
    fs::write(
        &dad_path,
        format!("{dad_base}What do you call a bear in the rain?,A drizzly bear\n"),
    )
    .unwrap();
    claude_checkpoint(&repo, "PostToolUse", &animals_path, "feature-session");
    claude_checkpoint(&repo, "PostToolUse", &dad_path, "feature-session");
    repo.stage_all_and_commit("Add bull and drizzly bear jokes")
        .unwrap();

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(
        &animals_path,
        format!("{animals_base}What's a cat's favorite color?,Purr-ple\n"),
    )
    .unwrap();
    claude_checkpoint(&repo, "PostToolUse", &animals_path, "main-session");
    repo.stage_all_and_commit("Add purple cat joke").unwrap();

    let rebase_result = repo.git(&["rebase", &main_branch, "scenario-3-multi-file-conflict"]);
    assert!(
        rebase_result.is_err(),
        "named-branch rebase should conflict on jokes-animals.csv"
    );

    claude_checkpoint(&repo, "PreToolUse", &animals_path, "resolve-session");
    fs::write(
        &animals_path,
        format!(
            "{animals_base}What's a cat's favorite color?,Purr-ple\nWhat do you call a sleeping bull?,A dozer\n"
        ),
    )
    .unwrap();
    claude_checkpoint(&repo, "PostToolUse", &animals_path, "resolve-session");
    repo.git(&["add", "jokes-animals.csv"]).unwrap();
    repo.git_without_test_sync_for_test(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .unwrap();

    let mut animals = repo.filename("jokes-animals.csv");
    animals.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "What do you call a bear with no teeth?,A gummy bear".ai(),
        "Why did the chicken go to the movie?,To see the hen-ema".ai(),
        "What do you call an alligator in a vest?,An investigator".ai(),
        "What's a cat's favorite color?,Purr-ple".ai(),
        "What do you call a sleeping bull?,A dozer".ai(),
    ]);
}

#[test]
fn test_named_branch_conflict_rebase_agent_rewrite_without_intermediate_sync() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("conflict.csv");

    let base = "\
setup,punchline
base joke,base punchline
";
    fs::write(&file_path, base).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("base jokes").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature-rewrite-conflict"])
        .unwrap();
    fs::write(
        &file_path,
        format!("{base}feature joke,feature punchline\n"),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("feature joke").unwrap();

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, format!("{base}main joke,main punchline\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("main joke").unwrap();

    let rebase_result = repo.git(&["rebase", &main_branch, "feature-rewrite-conflict"]);
    assert!(rebase_result.is_err(), "rebase should stop for a conflict");

    repo.git_ai(&["checkpoint", "human", "conflict.csv"])
        .unwrap();
    fs::write(&file_path, format!("{base}rewritten by ai,new punchline\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.git(&["add", "conflict.csv"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let mut file = repo.filename("conflict.csv");
    file.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "base joke,base punchline".ai(),
        "rewritten by ai,new punchline".ai(),
    ]);
}

#[test]
fn test_cherry_pick_conflict_ai_rewrite_resolution_is_attributed() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("conflict.csv");

    let base = "id,value\nbase,seed\n";
    fs::write(&file_path, base).unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "source"]).unwrap();
    fs::write(&file_path, format!("{base}feature,source\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("source line").unwrap();
    let source_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut file = repo.filename("conflict.csv");
    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "feature,source".ai(),
    ]);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, format!("{base}main,target\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("main line").unwrap();
    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "main,target".ai(),
    ]);

    assert!(
        repo.git(&["cherry-pick", &source_sha]).is_err(),
        "cherry-pick should conflict"
    );

    repo.git_ai(&["checkpoint", "human", "conflict.csv"])
        .unwrap();
    fs::write(&file_path, format!("{base}main,target\nresolver,rewrite\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.git(&["add", "conflict.csv"]).unwrap();
    repo.git_with_env(
        &["cherry-pick", "--continue"],
        &[("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();

    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "main,target".ai(),
        "resolver,rewrite".ai(),
    ]);
}

#[test]
fn test_squash_merge_conflict_keep_both_preserves_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("conflict.csv");

    let base = "id,value\nbase,seed\n";
    fs::write(&file_path, base).unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&file_path, format!("{base}feature,squash\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("feature line").unwrap();
    let mut file = repo.filename("conflict.csv");
    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "feature,squash".ai(),
    ]);

    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, format!("{base}main,squash\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.stage_all_and_commit("main line").unwrap();
    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "main,squash".ai(),
    ]);

    assert!(
        repo.git(&["merge", "--squash", "feature"]).is_err(),
        "squash merge should conflict"
    );

    repo.git_ai(&["checkpoint", "human", "conflict.csv"])
        .unwrap();
    fs::write(&file_path, format!("{base}main,squash\nfeature,squash\n")).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.csv"])
        .unwrap();
    repo.git(&["add", "conflict.csv"]).unwrap();
    repo.commit("squash feature").unwrap();

    file.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "main,squash".ai(),
        "feature,squash".ai(),
    ]);
}

#[test]
fn test_rebase_autostash_preserves_uncommitted_ai_worktree_attribution() {
    let repo = TestRepo::new();
    let committed_path = repo.path().join("committed.csv");
    let worktree_path = repo.path().join("worktree.csv");

    fs::write(&committed_path, "id,value\nbase,seed\n").unwrap();
    fs::write(&worktree_path, "id,value\nwork,seed\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&committed_path, "id,value\nbase,seed\nfeature,rebase\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "committed.csv"])
        .unwrap();
    repo.stage_all_and_commit("feature committed line").unwrap();
    let mut committed = repo.filename("committed.csv");
    committed.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "feature,rebase".ai(),
    ]);

    fs::write(&worktree_path, "id,value\nwork,seed\nworktree,ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "worktree.csv"])
        .unwrap();
    let mut worktree = repo.filename("worktree.csv");

    repo.git(&["stash", "push", "-m", "temporary-worktree"])
        .unwrap();
    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(repo.path().join("main.csv"), "id,value\nmain,rebase\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.csv"]).unwrap();
    repo.stage_all_and_commit("main unrelated line").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["stash", "pop"]).unwrap();

    repo.git(&["rebase", &main_branch, "--autostash"]).unwrap();

    committed.assert_lines_and_blame(crate::lines![
        "id,value".unattributed_human(),
        "base,seed".unattributed_human(),
        "feature,rebase".ai(),
    ]);

    repo.git(&["add", "worktree.csv"]).unwrap();
    repo.commit("commit autostashed worktree line").unwrap();
    worktree.assert_committed_lines(crate::lines![
        "id,value".unattributed_human(),
        "work,seed".unattributed_human(),
        "worktree,ai".ai(),
    ]);
}

fn head_reflog(repo: &TestRepo) -> PathBuf {
    repo.path().join(".git/logs/HEAD")
}

fn current_branch_reflog(repo: &TestRepo) -> PathBuf {
    repo.path()
        .join(".git/logs/refs/heads")
        .join(repo.current_branch())
}

fn truncate_reflog_to_first_entry(path: &Path) {
    let bytes = fs::read(path).unwrap();
    let first_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(bytes.len());
    assert!(
        first_end < bytes.len(),
        "expected multiple reflog entries in {}",
        path.display()
    );
    fs::write(path, &bytes[..first_end]).unwrap();
}

#[test]
fn test_empty_head_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    fs::write(head_reflog(&repo), "").unwrap();
    commit_ai_line(&repo, "next.txt", "next ai", "after empty head reflog");
}

#[test]
fn test_empty_branch_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    fs::write(current_branch_reflog(&repo), "").unwrap();
    commit_ai_line(&repo, "next.txt", "next ai", "after empty branch reflog");
}

#[test]
fn test_partially_pruned_head_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    commit_ai_line(&repo, "advance.txt", "advance", "advance cursor");
    truncate_reflog_to_first_entry(&head_reflog(&repo));
    commit_ai_line(
        &repo,
        "next.txt",
        "next ai",
        "after partially pruned head reflog",
    );
}

#[test]
fn test_partially_pruned_branch_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    commit_ai_line(&repo, "advance.txt", "advance", "advance cursor");
    truncate_reflog_to_first_entry(&current_branch_reflog(&repo));
    commit_ai_line(
        &repo,
        "next.txt",
        "next ai",
        "after partially pruned branch reflog",
    );
}

#[test]
fn test_deleted_head_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    fs::remove_file(head_reflog(&repo)).unwrap();
    commit_ai_line(&repo, "next.txt", "next ai", "after deleted head reflog");
}

#[test]
fn test_deleted_branch_reflog_does_not_break_trace2_ref_cursor() {
    let repo = TestRepo::new();

    commit_ai_line(&repo, "base.txt", "base", "initial");
    fs::remove_file(current_branch_reflog(&repo)).unwrap();
    commit_ai_line(&repo, "next.txt", "next ai", "after deleted branch reflog");
}

// =============================================================================
// Category A: Secondary file missing from authorship note
//
// Reproduction of fuzz_checkpoint_heavy_0:
// A multi-file commit includes fuzz_main.txt, fuzz_secondary_2.txt, and
// fuzz_secondary_3.txt — all with checkpointed edits — but the resulting
// authorship note only contains entries for some files, dropping others.
// =============================================================================

/// Multi-file commit where secondary file has AI checkpoint but is missing from note.
///
/// Models the fuzz_checkpoint_heavy_0 failure:
/// 1. Initial commit with AI on main file
/// 2. Selective commit of main file only (secondary stays dirty)
/// 3. Edit secondary files with checkpoints
/// 4. Commit all files together
/// 5. Note should include ALL files with attributed edits
#[test]
fn test_multifile_commit_secondary_file_missing_from_note() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit: AI edits on main file
    fs::write(&main_path, "AAA\nAAA\nAAA\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Edit both files, but only commit main
    fs::write(&main_path, "AAA\nAAA\nAAA\nBBB\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&sec_path, "CCC\nCCC\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();

    // Only stage and commit main.txt — secondary stays dirty
    repo.git(&["add", "main.txt"]).unwrap();
    repo.commit("commit main only").unwrap();

    // Now commit everything (secondary.txt is still dirty from before)
    fs::write(&sec_path, "CCC\nCCC\nDDD\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("commit all files").unwrap();

    // Both files should have attribution
    let mut main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(crate::lines![
        "AAA".ai(),
        "AAA".ai(),
        "AAA".ai(),
        "BBB".ai(),
    ]);

    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines!["CCC".ai(), "CCC".ai(), "DDD".ai(),]);
}

/// Simpler multi-file case: both files edited and committed in one shot.
#[test]
fn test_multifile_commit_both_files_attributed() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("other.txt");

    // Initial commit
    fs::write(&main_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Edit both files with AI checkpoints
    fs::write(&main_path, "base\nnew-main\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&sec_path, "new-other\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "other.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("multi-file commit").unwrap();

    let mut main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(crate::lines!["base".ai(), "new-main".ai()]);

    let mut sec_file = repo.filename("other.txt");
    sec_file.assert_committed_lines(crate::lines!["new-other".ai()]);
}

// =============================================================================
// Category B (human attributed as AI): Cherry-pick conflict + abort
//
// Reproduction of fuzz_combined_0:
// After a cherry-pick that conflicts and is aborted, the commit that follows
// has a note claiming ALL lines as AI, even though some were KnownHuman.
// The note's session range (1-5) doesn't distinguish human from AI lines.
// =============================================================================

/// Exact reproduction of fuzz_combined_0 failure sequence.
///
/// The critical sequence is:
/// 1. Delete-recreate file (8 lines: H=Ai×4, I=Human×1, J=Ai×3)
/// 2. checkpoint-storm (many rapid edits, 22 lines total), commit
/// 3. hard-reset HEAD~1 (back to 8 lines)
/// 4. overwrite-and-rollback: Y=Ai OverwriteAll 2, Z=Human Append 2, commit
/// 5. cherry-pick-conflict: feature branch prepends a=Human×4, main prepends b=Ai×1
///    cherry-pick conflicts, aborts
/// 6. verify: the "main commit" from step 5 has b(line1) + Y,Y,Z,Z
///    note should say line 1 = AI, lines 2-3 = AI (Y), lines 4-5 = Human (Z)
#[test]
fn test_cherry_pick_abort_main_commit_note_accuracy() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Step 1: Initial commit (simulates delete-recreate result)
    fs::write(
        &file_path,
        "HHHH\nHHHH\nHHHH\nHHHH\nIIII\nJJJJ\nJJJJ\nJJJJ\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    // Checkpoint the human line separately
    fs::write(
        &file_path,
        "HHHH\nHHHH\nHHHH\nHHHH\nIIII\nJJJJ\nJJJJ\nJJJJ\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.stage_all_and_commit("delete-recreate commit").unwrap();

    // Step 2: checkpoint-storm with many edits, then commit
    fs::write(
        &file_path,
        "storm1\nstorm2\nstorm3\nHHHH\nHHHH\nHHHH\nHHHH\nIIII\nJJJJ\nJJJJ\nJJJJ\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("storm commit").unwrap();

    // Step 3: hard-reset to the delete-recreate commit
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // Step 4: overwrite-and-rollback: overwrite entire file with AI, then append human
    fs::write(&file_path, "YYYY\nYYYY\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&file_path, "YYYY\nYYYY\nZZZZ\nZZZZ\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("overwrite-and-rollback").unwrap();

    // Step 5: cherry-pick-conflict
    // Create feature branch from HEAD~1 (the delete-recreate state)
    repo.git(&["checkout", "-b", "cp-feature", "HEAD~1"])
        .unwrap();
    // Feature: prepend human lines
    fs::write(
        &file_path,
        "aaaa\naaaa\naaaa\naaaa\nHHHH\nHHHH\nHHHH\nHHHH\nIIII\nJJJJ\nJJJJ\nJJJJ\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature: prepend human").unwrap();
    let feature_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Switch back to main (overwrite-and-rollback commit)
    repo.git(&["checkout", "-"]).unwrap();
    // Prepend AI line on main to create conflict
    fs::write(&file_path, "bbbb\nYYYY\nYYYY\nZZZZ\nZZZZ\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("main: prepend ai").unwrap();

    // Cherry-pick feature commit — should conflict (both prepend)
    let cp_result = repo.git(&["cherry-pick", &feature_sha]);
    if cp_result.is_err() {
        repo.git(&["cherry-pick", "--abort"]).ok();
    }

    // After abort: file should be in "main: prepend ai" state
    // = bbbb, YYYY, YYYY, ZZZZ, ZZZZ
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "bbbb".ai(),
        "YYYY".ai(),
        "YYYY".ai(),
        "ZZZZ".human(),
        "ZZZZ".human(),
    ]);
}

/// Simpler version: interleaved human and AI edits, note must not lump them together.
#[test]
fn test_interleaved_human_ai_edits_not_lumped() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Human edits: prepend 3 lines
    fs::write(&file_path, "human1\nhuman2\nhuman3\ninit\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    // AI edits: prepend 1 line
    fs::write(&file_path, "ai-top\nhuman1\nhuman2\nhuman3\ninit\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("mixed commit").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "ai-top".ai(),
        "human1".human(),
        "human2".human(),
        "human3".human(),
        "init".ai(),
    ]);
}

// =============================================================================
// Category B (AI attributed as human): Multi-squash produces incomplete note
//
// Reproduction of fuzz_destructive_0:
// After squashing 3 commits, the resulting note only covers some lines,
// leaving gaps where AI lines have no attestation (default to human).
// =============================================================================

/// Multi-squash: squash 3 commits with AI content, note must cover all AI lines.
///
/// Models the fuzz_destructive_0 failure:
/// 1. Make 3 commits on a feature branch with AI edits
/// 2. Squash merge them into main
/// 3. The squashed commit's note must attribute ALL AI lines
#[test]
fn test_multi_squash_incomplete_note() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let main_branch = repo.current_branch();

    // Feature branch: 3 commits with AI edits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    fs::write(&file_path, "base\nline-c\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature 1").unwrap();

    fs::write(&file_path, "base\nline-c\nline-d\nline-d\nline-d\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature 2").unwrap();

    // Third commit has a human DeleteAndInsert
    fs::write(&file_path, "base\nline-c\nhuman-e\nhuman-e\nline-d\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature 3").unwrap();

    // Switch to main and squash merge
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("squash all").unwrap();

    // Verify: all lines must have correct attribution
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "base".ai(),
        "line-c".ai(),
        "human-e".human(),
        "human-e".human(),
        "line-d".ai(),
    ]);
}

/// Reset then re-edit and squash: AI lines in the middle must not fall into gaps.
#[test]
fn test_reset_reedit_squash_no_attribution_gaps() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit with mixed content
    fs::write(&file_path, "aaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Second commit: add more AI lines
    fs::write(&file_path, "aaa\nbbb\nccc\nddd\neee\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("add more").unwrap();

    // Reset to initial
    repo.git(&["reset", "--mixed", "HEAD~1"]).unwrap();

    // Re-edit: human prepends, then AI appends
    fs::write(&file_path, "human-top\naaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    fs::write(
        &file_path,
        "human-top\naaa\nbbb\nccc\nai-bot\nai-bot\nai-bot\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("re-edit after reset").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "human-top".human(),
        "aaa".ai(),
        "bbb".ai(),
        "ccc".ai(),
        "ai-bot".ai(),
        "ai-bot".ai(),
        "ai-bot".ai(),
    ]);
}

/// Rebase then commit: notes should transfer through rebase for rebased commits.
#[test]
fn test_rebase_preserves_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let main_branch = repo.current_branch();

    // Feature branch: AI commit
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&file_path, "base\nfeature-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature").unwrap();

    // Advance main with a non-conflicting change
    repo.git(&["checkout", &main_branch]).unwrap();
    let other_path = repo.path().join("other.txt");
    fs::write(&other_path, "main-work\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "other.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("advance main").unwrap();

    // Rebase feature onto main (through daemon)
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // Merge back (fast-forward)
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["merge", "feature"]).unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines!["base".ai(), "feature-ai".ai()]);
}

// =============================================================================
// Category C: Reset sequencing before subsequent checkpoints
//
// A checkpoint immediately after reset must see working-log state after reset,
// not stale state from the commit that reset just removed.
// =============================================================================

#[test]
fn test_hard_reset_then_ai_checkpoint_preserves_new_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(&file_path, "base\nextra\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    fs::write(&file_path, "new-1\nnew-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("after reset").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines!["new-1".ai(), "new-2".ai(),]);
}

/// Simpler test: does overwriting all content work without a reset?
#[test]
fn test_overwrite_all_content_ai() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    fs::write(&file_path, "new-1\nnew-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines!["new-1".ai(), "new-2".ai(),]);
}

/// Same as above but with --mixed reset to see if bug is --hard specific.
#[test]
fn test_mixed_reset_then_ai_checkpoint() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Second commit
    fs::write(&file_path, "base\nextra\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    // Mixed reset back to initial
    repo.git(&["reset", "--mixed", "HEAD~1"]).unwrap();

    // New AI edits after mixed reset (same content as hard reset test)
    fs::write(&file_path, "new-ai-1\nnew-ai-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("after mixed reset").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines!["new-ai-1".ai(), "new-ai-2".ai(),]);
}

/// Hard reset then mixed AI and human checkpoints — both must be correctly attributed.
#[test]
fn test_hard_reset_mixed_checkpoint_types() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Second commit to create something to reset
    fs::write(&file_path, "init\nmore\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    // Hard reset
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // Human edits first
    fs::write(&file_path, "human-line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    // Then AI appends
    fs::write(&file_path, "human-line\nai-line\nai-line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("post-reset mixed").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "human-line".human(),
        "ai-line".ai(),
        "ai-line".ai(),
    ]);
}

/// Amend: amending a commit should preserve attribution for unchanged lines
/// and correctly attribute new lines.
#[test]
fn test_amend_preserves_existing_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "first\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Second commit with AI
    fs::write(&file_path, "first\nsecond-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    // Amend: add a human line
    fs::write(&file_path, "first\nsecond-ai\nthird-human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "second amended"])
        .unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "first".ai(),
        "second-ai".ai(),
        "third-human".human(),
    ]);
}

// =============================================================================
// Category D: Overbroad AI session range (human lines inside AI range)
//
// Reproduction of fuzz_combined_0:
// When AI and KnownHuman checkpoints both fire before a single commit,
// the resulting note's AI session range covers ALL lines (1-N) instead of
// only the lines from the AI checkpoint. The KnownHuman checkpoint's lines
// are swallowed into the AI range.
// =============================================================================

/// AI checkpoint then KnownHuman checkpoint, single commit.
/// The note must NOT lump human lines into the AI session range.
///
/// Models fuzz_combined_0: note says `s_xxx 1-5` but lines 2-5 are KnownHuman.
#[test]
fn test_overbroad_ai_range_swallows_human_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // AI writes some lines
    fs::write(&file_path, "ai-line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    // Human appends more lines AFTER the AI checkpoint
    fs::write(&file_path, "ai-line\nhuman-1\nhuman-2\nhuman-3\nhuman-4\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("mixed").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "ai-line".ai(),
        "human-1".human(),
        "human-2".human(),
        "human-3".human(),
        "human-4".human(),
    ]);
}

/// Inverse order: KnownHuman first, then AI prepends. Both must be tracked.
#[test]
fn test_overbroad_human_first_then_ai_prepend() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Human writes 4 lines
    fs::write(&file_path, "human-a\nhuman-b\nhuman-c\nhuman-d\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    // AI prepends 1 line
    fs::write(&file_path, "ai-top\nhuman-a\nhuman-b\nhuman-c\nhuman-d\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("prepend ai").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "ai-top".ai(),
        "human-a".human(),
        "human-b".human(),
        "human-c".human(),
        "human-d".human(),
    ]);
}

/// AI OverwriteAll then Human Append — models the overwrite-and-rollback pattern.
/// The AI checkpoint covers ALL content initially, then human appends. The note
/// must NOT claim human-appended lines as AI.
///
/// Critical: uses OverwriteAll (deletes all existing content) which is a more
/// aggressive pattern than simple append/prepend.
#[test]
fn test_overbroad_overwrite_all_then_human_append() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit with some content
    fs::write(&file_path, "old-1\nold-2\nold-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // AI overwrites ALL content (OverwriteAll pattern)
    fs::write(&file_path, "ai-new-1\nai-new-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    // Human appends after AI overwrite
    fs::write(&file_path, "ai-new-1\nai-new-2\nhuman-1\nhuman-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("overwrite then append").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "ai-new-1".ai(),
        "ai-new-2".ai(),
        "human-1".human(),
        "human-2".human(),
    ]);
}

/// Hard reset THEN overwrite+human pattern — simple variant.
#[test]
fn test_overbroad_after_hard_reset_overwrite_human() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "line-1\nline-2\nline-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Second commit (something to reset from)
    fs::write(&file_path, "line-1\nline-2\nline-3\nextra\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("second").unwrap();

    // Hard reset back, then checkpoint immediately.
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // AI OverwriteAll
    fs::write(&file_path, "Y-ai\nY-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    // Human Append
    fs::write(&file_path, "Y-ai\nY-ai\nZ-human\nZ-human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("overwrite-and-rollback").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "Y-ai".ai(),
        "Y-ai".ai(),
        "Z-human".human(),
        "Z-human".human(),
    ]);
}

/// Exact fuzz_combined_0 pattern: many rapid checkpoints (storm), commit, hard
/// reset back, then AI overwrite + human append. The checkpoint storm creates
/// many working log entries that the hard reset must invalidate.
#[test]
fn test_overbroad_checkpoint_storm_then_reset_then_overwrite_human() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "aaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Checkpoint storm: many rapid edits, then commit
    fs::write(&file_path, "storm-1\nstorm-2\naaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&file_path, "storm-1\nstorm-2\nstorm-3\naaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(
        &file_path,
        "storm-1\nstorm-2\nstorm-3\nstorm-4\naaa\nbbb\nccc\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(
        &file_path,
        "storm-1\nstorm-2\nstorm-3\nstorm-4\nstorm-5\naaa\nbbb\nccc\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("storm commit").unwrap();

    // Hard reset back to initial, then checkpoint immediately.
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // AI OverwriteAll
    fs::write(&file_path, "Y-1\nY-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    // Human Append
    fs::write(&file_path, "Y-1\nY-2\nZ-1\nZ-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("post-reset overwrite").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "Y-1".ai(),
        "Y-2".ai(),
        "Z-1".human(),
        "Z-2".human(),
    ]);
}

/// Like above but adds a cherry-pick conflict + abort after the overwrite,
/// matching the exact tail of fuzz_combined_0.
#[test]
fn test_overbroad_storm_reset_overwrite_then_cherry_pick_abort() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "aaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Storm + commit
    fs::write(&file_path, "s1\ns2\ns3\naaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("storm").unwrap();

    // Hard reset, then checkpoint immediately.
    repo.git(&["reset", "--hard", "HEAD~1"]).unwrap();

    // OverwriteAll (AI) + Append (Human) + commit
    fs::write(&file_path, "Y-1\nY-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&file_path, "Y-1\nY-2\nZ-1\nZ-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("overwrite-and-rollback").unwrap();

    // Feature branch from initial, prepend human lines
    repo.git(&["checkout", "-b", "cp-feature", "HEAD~1"])
        .unwrap();
    fs::write(&file_path, "human-a\nhuman-b\naaa\nbbb\nccc\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature: prepend human").unwrap();
    let feature_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Back to main, prepend AI
    repo.git(&["checkout", "-"]).unwrap();
    fs::write(&file_path, "b-ai\nY-1\nY-2\nZ-1\nZ-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("main: prepend ai").unwrap();

    // Cherry-pick → conflict → abort
    let cp_result = repo.git(&["cherry-pick", &feature_sha]);
    if cp_result.is_err() {
        repo.git(&["cherry-pick", "--abort"]).ok();
    }

    // After abort: main state = b-ai, Y-1, Y-2, Z-1, Z-2
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "b-ai".ai(),
        "Y-1".ai(),
        "Y-2".ai(),
        "Z-1".human(),
        "Z-2".human(),
    ]);
}

// =============================================================================
// Category E: File rename not tracked in authorship note
//
// Reproduction of fuzz_seed_3:
// After `git mv old.txt new.txt`, the authorship note for the commit still
// references the old filename. Blame on the new file finds no matching note
// entry, so all lines default to human.
// =============================================================================

/// Simple rename: AI-attributed file is renamed, note must reference new name.
#[test]
fn test_rename_file_note_tracks_new_name() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("original.txt");

    // Initial commit with AI content
    fs::write(&file_path, "ai-1\nai-2\nai-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "original.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let mut file = repo.filename("original.txt");
    file.assert_committed_lines(crate::lines!["ai-1".ai(), "ai-2".ai(), "ai-3".ai(),]);

    // Rename the file
    repo.git(&["mv", "original.txt", "renamed.txt"]).unwrap();
    repo.commit("rename file").unwrap();

    // Attribution should follow the rename
    let mut renamed = repo.filename("renamed.txt");
    renamed.assert_committed_lines(crate::lines!["ai-1".ai(), "ai-2".ai(), "ai-3".ai(),]);
}

/// Rename + edit in same commit: new content should be attributed to the new name.
#[test]
fn test_rename_and_edit_same_commit() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("original.txt");

    // Initial commit with AI content
    fs::write(&file_path, "ai-1\nai-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "original.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Rename and add new AI content
    repo.git(&["mv", "original.txt", "renamed.txt"]).unwrap();
    let renamed_path = repo.path().join("renamed.txt");
    fs::write(&renamed_path, "ai-1\nai-2\nnew-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "renamed.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("rename and edit").unwrap();

    let mut renamed = repo.filename("renamed.txt");
    renamed.assert_committed_lines(crate::lines!["ai-1".ai(), "ai-2".ai(), "new-ai".ai(),]);
}

// =============================================================================
// Category F: Secondary file missing from multi-file commit note
//
// Reproduction of fuzz_seed_4 and fuzz_checkpoint_heavy_0:
// A commit touches multiple files, all with AI checkpoints, but the resulting
// authorship note only contains entries for some files (typically fuzz_main.txt),
// dropping others entirely.
// =============================================================================

/// Two files checkpointed, committed together — both must appear in note.
#[test]
fn test_multi_file_both_in_note() {
    let repo = TestRepo::new();
    let file_a = repo.path().join("file_a.txt");
    let file_b = repo.path().join("file_b.txt");

    // Initial commit
    fs::write(&file_a, "a-init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_a.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Edit both files with AI checkpoints
    fs::write(&file_a, "a-init\na-new\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_a.txt"])
        .unwrap();
    fs::write(&file_b, "b-new-1\nb-new-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file_b.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("multi-file").unwrap();

    let mut fa = repo.filename("file_a.txt");
    fa.assert_committed_lines(crate::lines!["a-init".ai(), "a-new".ai(),]);

    let mut fb = repo.filename("file_b.txt");
    fb.assert_committed_lines(crate::lines!["b-new-1".ai(), "b-new-2".ai(),]);
}

/// Three files: main + two secondaries. All have checkpoints. All must be in note.
/// Models fuzz_checkpoint_heavy_0 exactly.
#[test]
fn test_three_files_secondary_dropped_from_note() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec2_path = repo.path().join("secondary_2.txt");
    let sec3_path = repo.path().join("secondary_3.txt");

    // Initial commit on main
    fs::write(&main_path, "main-init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Multiple edits and checkpoints on all files
    fs::write(&main_path, "main-init\nmain-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    fs::write(&sec2_path, "sec2-line1\nsec2-line2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary_2.txt"])
        .unwrap();

    fs::write(&sec3_path, "sec3-line1\nsec3-line2\nsec3-line3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary_3.txt"])
        .unwrap();

    repo.git(&["add", "-A"]).unwrap();
    repo.commit("all three files").unwrap();

    let mut main = repo.filename("main.txt");
    main.assert_committed_lines(crate::lines!["main-init".ai(), "main-ai".ai(),]);

    let mut sec2 = repo.filename("secondary_2.txt");
    sec2.assert_committed_lines(crate::lines!["sec2-line1".ai(), "sec2-line2".ai(),]);

    let mut sec3 = repo.filename("secondary_3.txt");
    sec3.assert_committed_lines(crate::lines![
        "sec3-line1".ai(),
        "sec3-line2".ai(),
        "sec3-line3".ai(),
    ]);
}

/// Secondary file checkpointed BEFORE an intervening commit on another file.
/// The checkpoint's base_commit is now stale. On final commit, secondary is
/// dropped from the note because the working log base doesn't match HEAD.
///
/// This is the exact pattern from fuzz_checkpoint_heavy_0:
/// 1. Edit main + secondary, checkpoint both
/// 2. Commit ONLY main (selective-file-commit)
/// 3. More edits/checkpoints on main, more commits
/// 4. Commit everything — secondary's stale checkpoint is lost
#[test]
fn test_secondary_file_stale_checkpoint_across_commits() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit
    fs::write(&main_path, "main\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Checkpoint BOTH files
    fs::write(&main_path, "main\nmain-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    fs::write(&sec_path, "sec-1\nsec-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();

    // Commit ONLY main — secondary stays dirty with stale checkpoint
    repo.git(&["add", "main.txt"]).unwrap();
    repo.commit("main only").unwrap();

    // More work on main (advances HEAD further from secondary's checkpoint base)
    fs::write(&main_path, "main\nmain-2\nmain-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "main.txt"]).unwrap();
    repo.commit("advance main again").unwrap();

    // Now commit everything — secondary's checkpoint was based on initial commit
    fs::write(&sec_path, "sec-1\nsec-2\nsec-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("include secondary").unwrap();

    let mut sec = repo.filename("secondary.txt");
    sec.assert_committed_lines(crate::lines!["sec-1".ai(), "sec-2".ai(), "sec-3".ai(),]);
}

// =============================================================================
// Category G: Incomplete note ranges after squash/rebase
//
// Reproduction of fuzz_destructive_0:
// After squash merge, the resulting note's line ranges have gaps — some AI
// lines fall outside any attestation range and default to human.
// =============================================================================

/// Squash merge with multiple AI commits: all AI lines must be covered.
#[test]
fn test_squash_merge_incomplete_ranges() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let main_branch = repo.current_branch();

    // Feature branch with multiple AI commits that build on each other
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    fs::write(&file_path, "base\nfeat-1\nfeat-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feat commit 1").unwrap();

    fs::write(&file_path, "base\nfeat-1\nfeat-2\nfeat-3\nfeat-4\nfeat-5\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feat commit 2").unwrap();

    // Insert human lines in the middle
    fs::write(
        &file_path,
        "base\nfeat-1\nhuman-mid\nfeat-2\nfeat-3\nfeat-4\nfeat-5\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feat commit 3 (human insert)").unwrap();

    // Squash merge into main
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.commit("squash merge").unwrap();

    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "base".ai(),
        "feat-1".ai(),
        "human-mid".human(),
        "feat-2".ai(),
        "feat-3".ai(),
        "feat-4".ai(),
        "feat-5".ai(),
    ]);
}

// =============================================================================
// Category F: Multi-squash attribution preservation
// =============================================================================

/// After reset --soft HEAD~N + commit (manual squash), AI lines added in
/// intermediate commits must survive. This models the fuzzer's multi-squash
/// pattern: multiple commits with mixed operations including deletions that
/// cause the authorship note's line coverage to be incomplete — some lines
/// only appear in intermediate commits' notes, not the final one.
#[test]
fn test_multi_squash_preserves_intermediate_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Base commit
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Commit 1: DeleteAndInsert — delete line 1, insert 2 human lines at top
    fs::write(&file_path, "HH1\nHH2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("squash-1: delete-insert human").unwrap();

    // Commit 2: append AI line
    fs::write(&file_path, "HH1\nHH2\nAI-appended\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("squash-2: append AI").unwrap();

    // Commit 3: replace line 1 with different human content
    fs::write(&file_path, "HH-replaced\nHH2\nAI-appended\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("squash-3: replace human").unwrap();

    // Commit 4: prepend 2 AI lines
    fs::write(
        &file_path,
        "AI-pre1\nAI-pre2\nHH-replaced\nHH2\nAI-appended\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("squash-4: prepend AI").unwrap();

    // Squash all 4 into one
    repo.git(&["reset", "--soft", &base]).unwrap();
    repo.commit("squashed").unwrap();

    // Verify attribution
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "AI-pre1".ai(),
        "AI-pre2".ai(),
        "HH-replaced".human(),
        "HH2".human(),
        "AI-appended".ai(),
    ]);
}

/// After squash, a file that was only created in an intermediate commit must
/// still appear in the authorship note with correct attribution.
#[test]
fn test_multi_squash_preserves_secondary_file() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit
    fs::write(&main_path, "main\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Commit 1: edit main
    fs::write(&main_path, "main\nmain-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("edit main").unwrap();

    // Commit 2: create secondary file with mixed attribution
    fs::write(&sec_path, "sec-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();
    fs::write(&sec_path, "sec-ai\nsec-human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "secondary.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("add secondary").unwrap();

    // Commit 3: edit main again
    fs::write(&main_path, "main\nmain-2\nmain-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("edit main again").unwrap();

    // Squash all into one
    repo.git(&["reset", "--soft", &base]).unwrap();
    repo.commit("squashed").unwrap();

    // Both files must be in the note with correct attribution
    let mut main_file = repo.filename("main.txt");
    main_file.assert_committed_lines(crate::lines!["main".ai(), "main-2".ai(), "main-3".ai(),]);

    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines!["sec-ai".ai(), "sec-human".human(),]);
}

// =============================================================================
// Category G: Cherry-pick over-attribution
// =============================================================================

/// After cherry-pick, lines from the target branch must NOT be re-attributed
/// by the source commit's note. This models the fuzzer scenario: feature has
/// AI content, main has human content at a different position. Cherry-pick
/// applies cleanly but the note transfer must not claim main's lines as AI.
///
/// The setup ensures a clean cherry-pick: feature adds lines at the END of
/// the file, while main added lines at the BEGINNING. Git applies without conflict.
#[test]
fn test_cherry_pick_does_not_overattribute_target_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit: single shared line
    fs::write(&file_path, "shared\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Main: prepend human lines (non-conflicting position)
    fs::write(&file_path, "human-1\nhuman-2\nshared\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("main: prepend human").unwrap();

    // Feature branch from initial: append AI lines (non-conflicting position)
    repo.git(&["checkout", "-b", "feature", "HEAD~1"]).unwrap();
    fs::write(&file_path, "shared\nai-1\nai-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature: append AI").unwrap();
    let feature_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Back to main, cherry-pick feature
    repo.git(&["checkout", "-"]).unwrap();
    repo.git(&["cherry-pick", &feature_sha]).unwrap();

    // Result: human-1, human-2, shared, ai-1, ai-2
    // human lines must remain human, AI lines must be AI
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "human-1".human(),
        "human-2".human(),
        "shared".unattributed_human(),
        "ai-1".ai(),
        "ai-2".ai(),
    ]);
}

// =============================================================================
// Category H: Selective staging attribution carryover
// =============================================================================

/// When committing only one of multiple checkpointed files, the dirty file's
/// attribution must survive to the next commit via INITIAL carryover.
#[test]
fn test_selective_commit_preserves_dirty_file_attribution() {
    let repo = TestRepo::new();
    let main_path = repo.path().join("main.txt");
    let sec_path = repo.path().join("secondary.txt");

    // Initial commit
    fs::write(&main_path, "main-init\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Edit BOTH files with attribution
    fs::write(&main_path, "main-init\nmain-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();

    fs::write(&sec_path, "sec-ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "secondary.txt"])
        .unwrap();

    // Commit ONLY main, leave secondary dirty
    repo.git(&["add", "main.txt"]).unwrap();
    repo.commit("main only").unwrap();

    // Now commit secondary
    repo.git(&["add", "secondary.txt"]).unwrap();
    repo.commit("secondary").unwrap();

    // Secondary must retain its AI attribution
    let mut sec_file = repo.filename("secondary.txt");
    sec_file.assert_committed_lines(crate::lines!["sec-ai".ai(),]);
}

// =============================================================================
// Category E: Cherry-pick --no-commit loses attribution
//
// When cherry-pick is invoked with --no-commit, HEAD doesn't change so the
// daemon doesn't emit a CherryPickComplete event. The cherry-picked content
// gets staged but has no working log entries, so the subsequent commit loses
// attribution for those lines.
// =============================================================================

#[test]
fn test_cherry_pick_no_commit_preserves_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit with base content
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    // Feature branch: add AI lines
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&file_path, "base\nai-line-1\nai-line-2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature: AI lines").unwrap();
    let feature_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Back to main
    repo.git(&["checkout", "main"]).unwrap();

    // Cherry-pick with --no-commit (stages content without creating commit)
    repo.git(&["cherry-pick", "--no-commit", &feature_sha])
        .unwrap();

    // Now commit (attribution should be preserved from source commit's note)
    repo.commit("cherry-picked content").unwrap();

    // Verify AI lines retain attribution
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "base".human(),
        "ai-line-1".ai(),
        "ai-line-2".ai(),
    ]);
}

/// Rebase with conflict resolved via `checkout --theirs` should preserve attribution.
/// In rebase context, `--theirs` means the branch being rebased (feature branch).
#[test]
fn test_rebase_conflict_theirs_preserves_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("main.txt");

    // Initial commit
    fs::write(&file_path, "line-1\nline-2\nline-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.stage_all_and_commit("initial").unwrap();

    let main_branch = repo.current_branch();

    // Feature branch: AI prepend
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&file_path, "ai-prepend\nline-1\nline-2\nline-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "main.txt"]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("feature: AI prepend").unwrap();

    // Main: conflicting prepend
    repo.git(&["checkout", &main_branch]).unwrap();
    fs::write(&file_path, "main-prepend\nline-1\nline-2\nline-3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.commit("main: human prepend").unwrap();

    // Rebase feature onto main (will conflict)
    repo.git(&["checkout", "feature"]).unwrap();
    let _ = repo.git(&["rebase", &main_branch]); // expect conflict

    // Resolve by taking theirs (feature branch's version in rebase context)
    repo.git(&["checkout", "--theirs", "--", "main.txt"])
        .unwrap();
    repo.git(&["add", "main.txt"]).unwrap();

    // Set GIT_EDITOR to avoid interactive editor during rebase --continue
    let result = repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None);
    assert!(result.is_ok(), "rebase --continue failed: {:?}", result);

    // After rebase --continue, the rebased commit should retain AI attribution
    // In rebase context, --theirs = feature branch version = ai-prepend + original lines
    let mut file = repo.filename("main.txt");
    file.assert_committed_lines(crate::lines![
        "ai-prepend".ai(),
        "line-1".human(),
        "line-2".human(),
        "line-3".human(),
    ]);
}

/// Spawn-scaling guard: a multi-commit `git revert` must trigger a CONSTANT
/// number of daemon git spawns regardless of how many commits are reverted.
/// Reverting N commits in one command and 3*N commits in another must produce
/// nearly the same revert-side-effect spawn count (the batched path issues a
/// fixed set of rev-parse/diff-tree/cat-file/notes spawns, not per-commit ones).
/// `#[ignore]` because it shells a dedicated daemon and inspects a spawn log;
/// run explicitly with `--ignored`.
#[test]
#[ignore]
fn revert_spawn_count_is_constant_in_commit_count() {
    fn revert_n(n: usize) -> usize {
        let log_dir =
            std::env::temp_dir().join(format!("git-ai-spawnlog-{}-{}", std::process::id(), n));
        let _ = fs::create_dir_all(&log_dir);
        let log_path = log_dir.join("spawns.log");
        let _ = fs::remove_file(&log_path);
        let repo =
            TestRepo::new_with_daemon_env(&[("GIT_AI_SPAWN_LOG", log_path.to_str().unwrap())]);

        // Base with n files.
        for i in 0..n {
            fs::write(repo.path().join(format!("f{i}.txt")), format!("base {i}\n")).unwrap();
        }
        repo.stage_all_and_commit("base").unwrap();

        // Add an AI line to each, one commit.
        for i in 0..n {
            fs::write(
                repo.path().join(format!("f{i}.txt")),
                format!("base {i}\nAI {i}\n"),
            )
            .unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &format!("f{i}.txt")])
                .unwrap();
        }
        repo.stage_all_and_commit("add ai").unwrap();

        // Delete each AI line in its own commit → n delete commits.
        let mut del_commits = Vec::new();
        for i in 0..n {
            fs::write(repo.path().join(format!("f{i}.txt")), format!("base {i}\n")).unwrap();
            repo.git_ai(&["checkpoint", "mock_known_human", &format!("f{i}.txt")])
                .unwrap();
            repo.stage_all_and_commit(&format!("del {i}")).unwrap();
            del_commits.push(repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string());
        }

        repo.sync_daemon();
        let before = fs::read_to_string(&log_path)
            .map(|c| c.lines().count())
            .unwrap_or(0);

        // One revert command over all n delete commits.
        let mut args = vec!["revert".to_string(), "--no-edit".to_string()];
        args.extend(del_commits);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        repo.git(&arg_refs).unwrap();
        repo.sync_daemon();

        let after = fs::read_to_string(&log_path)
            .map(|c| c.lines().count())
            .unwrap_or(0);
        let _ = fs::remove_dir_all(&log_dir);
        after - before
    }

    let small = revert_n(2);
    let large = revert_n(8);
    eprintln!("revert spawns: n=2 -> {small}, n=8 -> {large}");
    // If revert work were per-commit, large would be ~4x small. With batching the
    // counts should be close (allow a small constant slack for git's own
    // revert-time invocations, which are not git-ai daemon spawns anyway).
    assert!(
        large <= small + 4,
        "revert spawn count scales with commit count: n=2 -> {small}, n=8 -> {large}"
    );
}

/// Spawn-scaling guard for the rebase path: rebasing a feature branch of N AI
/// commits onto an advanced main must trigger a CONSTANT number of daemon git
/// spawns regardless of N (the note-shift and conflict-resolution work is
/// batched). `#[ignore]`; run with `--ignored`.
#[test]
#[ignore]
fn rebase_spawn_count_is_constant_in_commit_count() {
    fn rebase_n(n: usize) -> usize {
        let log_dir = std::env::temp_dir().join(format!(
            "git-ai-spawnlog-rebase-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::create_dir_all(&log_dir);
        let log_path = log_dir.join("spawns.log");
        let _ = fs::remove_file(&log_path);
        let repo =
            TestRepo::new_with_daemon_env(&[("GIT_AI_SPAWN_LOG", log_path.to_str().unwrap())]);

        fs::write(repo.path().join("base.txt"), "base\n").unwrap();
        repo.stage_all_and_commit("base").unwrap();
        let default_branch = repo.current_branch();

        // Feature branch with N AI commits, each adding a line to its own file.
        repo.git(&["checkout", "-b", "feature"]).unwrap();
        for i in 0..n {
            fs::write(
                repo.path().join(format!("feat{i}.txt")),
                format!("AI feat {i}\n"),
            )
            .unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &format!("feat{i}.txt")])
                .unwrap();
            repo.stage_all_and_commit(&format!("feat {i}")).unwrap();
        }

        // Advance main so the rebase is a real non-fast-forward.
        repo.git(&["checkout", &default_branch]).unwrap();
        fs::write(repo.path().join("main.txt"), "main work\n").unwrap();
        repo.stage_all_and_commit("main advance").unwrap();

        repo.git(&["checkout", "feature"]).unwrap();
        repo.sync_daemon();
        let before = fs::read_to_string(&log_path)
            .map(|c| c.lines().count())
            .unwrap_or(0);

        repo.git(&["rebase", &default_branch]).unwrap();
        repo.sync_daemon();

        let after = fs::read_to_string(&log_path)
            .map(|c| c.lines().count())
            .unwrap_or(0);
        let _ = fs::remove_dir_all(&log_dir);
        after - before
    }

    let small = rebase_n(2);
    let large = rebase_n(8);
    eprintln!("rebase spawns: n=2 -> {small}, n=8 -> {large}");
    assert!(
        large <= small + 4,
        "rebase spawn count scales with commit count: n=2 -> {small}, n=8 -> {large}"
    );
}
