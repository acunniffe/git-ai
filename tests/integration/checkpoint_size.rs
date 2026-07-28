use crate::repos::test_repo::TestRepo;
use git_ai::authorship::working_log::CheckpointKind;
use rand::{RngExt, distr::Alphanumeric};
use serde_json::json;
use std::{fs, time::Instant};

#[test]
fn test_checkpoint_size_logging_large_ai_rewrites() {
    eprintln!("test_checkpoint_size_logging_large_ai_rewrites started...");
    let repo = TestRepo::new();
    let mut rng = rand::rng();

    // (target_lines, iterations)
    let configs: &[(usize, usize)] = &[
        (2, 5),
        (20, 5),
        (200, 5),
        (500, 5),
        (1000, 5),
        // (2_000, 5), // uncomment for heavier run
    ];

    let file_path = repo.path().join("large_ai_file.txt");

    for (config_idx, (target_lines, iterations)) in configs.iter().copied().enumerate() {
        eprintln!("config {config_idx}: target_lines={target_lines}, iterations={iterations}");

        let mut durations = Vec::with_capacity(iterations);

        for iteration in 0..iterations {
            // Build a fresh file with random AI-authored content for this iteration.
            let mut content = String::with_capacity(target_lines * 48);
            for line_idx in 0..target_lines {
                let random_fragment: String =
                    (0..24).map(|_| rng.sample(Alphanumeric) as char).collect();
                content.push_str(&format!(
                    "ai_line_{config_idx}_{iteration}_{line_idx}_{random_fragment}\n"
                ));
            }

            eprintln!("config {config_idx} iteration {iteration} (starting checkpoint)");

            let start = Instant::now();
            fs::write(&file_path, &content).expect("should write large file");

            // Mark the entire rewrite as AI-authored for this iteration.
            let git_ai_output = repo
                .git_ai(&["checkpoint", "mock_ai", "large_ai_file.txt"])
                .expect("git-ai checkpoint should succeed");

            eprintln!("git-ai checkpoint output:\n{git_ai_output}\n");

            durations.push(start.elapsed());

            eprintln!(
                "config {config_idx} iteration {iteration} duration: {} ms",
                start.elapsed().as_millis()
            );
        }

        let mut sorted = durations.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        let max = sorted[sorted.len() - 1];

        for (idx, duration) in durations.iter().enumerate() {
            println!(
                "config {config_idx} iteration {idx}: {} ms",
                duration.as_millis()
            );
        }
        println!(
            "config {config_idx} median duration: {} ms, max duration: {} ms",
            median.as_millis(),
            max.as_millis()
        );

        let working_log = repo.current_working_logs();
        let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
        let size = fs::metadata(&checkpoints_file)
            .expect("checkpoints.jsonl should exist")
            .len();

        println!(
            "config {config_idx} checkpoints.jsonl path: {:?}, size (bytes): {}",
            checkpoints_file, size
        );
    }
}

#[test]
fn test_checkpoint_skips_oversized_files() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|p| p.max_checkpoint_file_size_bytes = Some(64));

    let small_path = repo.path().join("small.txt");
    let big_path = repo.path().join("big.txt");
    fs::write(&small_path, "small content\n").expect("write small file");
    fs::write(&big_path, "x".repeat(256)).expect("write big file");

    repo.git_ai(&["checkpoint", "mock_ai", "small.txt", "big.txt"])
        .expect("git-ai checkpoint should succeed");

    let working_log = repo.working_logs_for_base_commit("initial");
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 1, "expected exactly one checkpoint");
    let latest = checkpoints.last().unwrap();
    assert_eq!(
        latest.entries.len(),
        1,
        "expected one entry: oversized file should be skipped"
    );
    assert_eq!(latest.entries[0].file, "small.txt");
}

#[test]
fn test_checkpoint_saves_normal_files_under_limit() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|p| p.max_checkpoint_file_size_bytes = Some(1024));

    let file_path = repo.path().join("normal.txt");
    let content = "this is a normal file\nwith a few lines\n";
    fs::write(&file_path, content).expect("write normal file");

    repo.git_ai(&["checkpoint", "mock_ai", "normal.txt"])
        .expect("git-ai checkpoint should succeed");

    let working_log = repo.working_logs_for_base_commit("initial");
    let checkpoints = working_log.read_all_checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 1, "expected exactly one checkpoint");
    let latest = checkpoints.last().unwrap();
    assert_eq!(
        latest.entries.len(),
        1,
        "expected one entry for normal file"
    );
    assert_eq!(latest.entries[0].file, "normal.txt");
}

#[test]
fn test_checkpoint_skips_files_over_total_size_budget() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|p| {
        p.max_checkpoint_file_size_bytes = Some(1024);
        p.max_checkpoint_total_size_bytes = Some(96);
        p.max_checkpoint_total_lines = Some(1000);
    });

    let kept_path = repo.path().join("a_kept.txt");
    let skipped_path = repo.path().join("z_skipped.txt");
    fs::write(&kept_path, "a".repeat(48)).expect("write kept file");
    fs::write(&skipped_path, "z".repeat(64)).expect("write skipped file");

    repo.git_ai(&["checkpoint", "mock_ai", "a_kept.txt", "z_skipped.txt"])
        .expect("git-ai checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 1, "expected exactly one checkpoint");
    let latest = checkpoints.last().unwrap();
    assert_eq!(
        latest.entries.len(),
        1,
        "expected aggregate byte budget to skip the second file"
    );
    assert_eq!(latest.entries[0].file, "a_kept.txt");
}

#[test]
fn test_checkpoint_skips_files_over_total_line_budget() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|p| {
        p.max_checkpoint_file_size_bytes = Some(1024);
        p.max_checkpoint_total_size_bytes = Some(1024);
        p.max_checkpoint_total_lines = Some(3);
    });

    let kept_path = repo.path().join("a_kept.txt");
    let skipped_path = repo.path().join("z_skipped.txt");
    fs::write(&kept_path, "one\ntwo\n").expect("write kept file");
    fs::write(&skipped_path, "three\nfour\n").expect("write skipped file");

    repo.git_ai(&["checkpoint", "mock_ai", "a_kept.txt", "z_skipped.txt"])
        .expect("git-ai checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 1, "expected exactly one checkpoint");
    let latest = checkpoints.last().unwrap();
    assert_eq!(
        latest.entries.len(),
        1,
        "expected aggregate line budget to skip the second file"
    );
    assert_eq!(latest.entries[0].file, "a_kept.txt");
}

#[test]
fn test_checkpoint_total_size_budget_applies_to_bash_checkpoints() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|p| {
        p.max_checkpoint_file_size_bytes = Some(1024);
        p.max_checkpoint_total_size_bytes = Some(96);
        p.max_checkpoint_total_lines = Some(1000);
    });

    let repo_root = repo.canonical_path();
    let kept_path = repo_root.join("a_kept.txt");
    let skipped_path = repo_root.join("z_skipped.txt");
    fs::write(&kept_path, "base\n").expect("write kept base");
    fs::write(&skipped_path, "base\n").expect("write skipped base");
    repo.stage_all_and_commit("Initial commit").unwrap();

    let transcript_path = repo_root.join("codex-session.jsonl");
    fs::write(&transcript_path, "{}\n").expect("write transcript");

    let pre_hook = json!({
        "session_id": "checkpoint-budget-session",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "checkpoint-budget-bash",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook])
        .expect("pre bash checkpoint should succeed");

    fs::write(&kept_path, "a".repeat(48)).expect("write kept edit");
    fs::write(&skipped_path, "z".repeat(64)).expect("write skipped edit");

    let post_hook = json!({
        "session_id": "checkpoint-budget-session",
        "cwd": repo_root.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "checkpoint-budget-bash",
        "tool_input": {
            "command": "echo hello"
        },
        "transcript_path": transcript_path.to_string_lossy()
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &post_hook])
        .expect("post bash checkpoint should succeed");

    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    let latest_ai = checkpoints
        .iter()
        .rev()
        .find(|checkpoint| checkpoint.kind == CheckpointKind::AiAgent)
        .expect("expected bash AI checkpoint");
    assert_eq!(
        latest_ai.entries.len(),
        1,
        "expected aggregate budget to apply to bash checkpoint payload"
    );
    assert_eq!(latest_ai.entries[0].file, "a_kept.txt");
}

crate::reuse_tests_in_worktree!(test_checkpoint_size_logging_large_ai_rewrites,);
