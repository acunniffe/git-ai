use super::*;

#[test]
#[serial]
fn daemon_pure_trace_socket_high_throughput_ai_commit_burst_preserves_exact_blame() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let file_count = 16usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_completions = 0u64;
    for idx in 0..file_count {
        let file_rel = format!("daemon-race-file-{idx}.txt");
        let file_path = repo.path().join(file_rel.as_str());
        fs::write(&file_path, format!("ai-line-{idx}\n"))
            .expect("failed to write ai burst test file");

        repo.git_ai_with_env(
            &["checkpoint", "mock_ai", file_rel.as_str()],
            &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
        )
        .expect("delegated ai checkpoint should succeed");
        expected_completions += 1;

        repo.git_og_with_env(&["add", file_rel.as_str()], &env_refs)
            .expect("staging ai burst file should succeed");
        expected_completions += 1;
    }

    // Wait for all checkpoints and adds to complete before committing
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    repo.git_og_with_env(&["commit", "-m", "ai burst commit"], &env_refs)
        .expect("ai burst commit should succeed");
    expected_completions += 1;

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let mut file = repo.filename(format!("daemon-race-file-{idx}.txt").as_str());
        file.assert_lines_and_blame(lines![format!("ai-line-{idx}").ai()]);
    }
}

#[test]
#[serial]
fn daemon_pure_trace_socket_concurrent_worktree_burst_preserves_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let worker_a_dir = repo.path().to_path_buf();
    let worker_b_dir = unique_worktree_path(&repo, "daemon-race-worker-b");
    let worker_b_dir_str = worker_b_dir.to_string_lossy().to_string();

    repo.git_og_with_env(&["checkout", "-b", "daemon-race-worker-a"], &env_refs)
        .expect("checkout worker-a branch should succeed");
    repo.git_og_with_env(
        &[
            "worktree",
            "add",
            "-b",
            "daemon-race-worker-b",
            worker_b_dir_str.as_str(),
        ],
        &env_refs,
    )
    .expect("worktree add worker-b should succeed");
    wait_for_expected_top_level_completions(&repo, 0, 2);

    let file_count = 10usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_completions = 0u64;
    for idx in 0..file_count {
        let file_a = format!("daemon-race-a-{idx}.txt");
        harness.write_ai_line_checkpoint_and_add(
            &worker_a_dir,
            file_a.as_str(),
            format!("a-ai-line-{idx}").as_str(),
        );
        expected_completions += 2; // checkpoint + add

        let file_b = format!("daemon-race-b-{idx}.txt");
        harness.write_ai_line_checkpoint_and_add(
            &worker_b_dir,
            file_b.as_str(),
            format!("b-ai-line-{idx}").as_str(),
        );
        expected_completions += 2; // checkpoint + add
    }

    // Wait for all checkpoints and adds to complete before committing
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    harness.run_traced_git(&worker_a_dir, &["commit", "-m", "worker-a burst commit"]);
    harness.run_traced_git(&worker_b_dir, &["commit", "-m", "worker-b burst commit"]);
    expected_completions += 2; // both commits

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let file_a = format!("daemon-race-a-{idx}.txt");
        let file_b = format!("daemon-race-b-{idx}.txt");
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_a_dir,
            file_a.as_str(),
            format!("a-ai-line-{idx}").as_str(),
        );
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_b_dir,
            file_b.as_str(),
            format!("b-ai-line-{idx}").as_str(),
        );
    }

    let _ = repo.git_og_with_env(
        &["worktree", "remove", "--force", worker_b_dir_str.as_str()],
        &env_refs,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_concurrent_checkpoint_requests_preserve_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let workdir = repo.path().to_path_buf();

    let file_count = 12usize;
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected = Vec::new();
    for idx in 0..file_count {
        let file_rel = format!("daemon-race-concurrent-checkpoint-{idx}.txt");
        let line = format!("ai-line-{idx}");
        fs::write(workdir.join(file_rel.as_str()), format!("{line}\n"))
            .expect("failed to write concurrent checkpoint test file");
        expected.push((file_rel, line));
    }

    #[cfg(windows)]
    {
        for (file_rel, _) in &expected {
            harness.run_delegated_checkpoint(&workdir, file_rel.as_str());
        }
    }
    #[cfg(not(windows))]
    {
        let mut checkpoint_threads = Vec::new();
        for (file_rel, _) in &expected {
            let thread_workdir = workdir.clone();
            let harness = harness.clone();
            let file_rel = file_rel.clone();
            checkpoint_threads.push(thread::spawn(move || {
                harness.run_delegated_checkpoint(&thread_workdir, file_rel.as_str());
            }));
        }
        for handle in checkpoint_threads {
            handle
                .join()
                .expect("concurrent delegated checkpoint thread should not panic");
        }
    }

    // Wait for all concurrent checkpoints to complete before adding
    let mut expected_completions = file_count as u64;
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    repo.git_og_with_env(&["add", "."], &env_refs)
        .expect("staging concurrent checkpoint files should succeed");
    expected_completions += 1;

    repo.git_og_with_env(
        &["commit", "-m", "concurrent delegated checkpoint burst"],
        &env_refs,
    )
    .expect("commit for concurrent checkpoint files should succeed");
    expected_completions += 1;

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for (file_rel, line) in expected {
        let mut file = repo.filename(file_rel.as_str());
        file.assert_lines_and_blame(lines![line.ai()]);
    }
}

#[test]
#[serial]
fn daemon_pure_trace_socket_parallel_worktree_streams_preserve_exact_line_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];

    let harness = WorkdirRaceHarness::new(&repo, trace_socket.clone());
    let worker_a_dir = repo.path().to_path_buf();
    let worker_b_dir = unique_worktree_path(&repo, "daemon-race-worker-b-parallel");
    let worker_b_dir_str = worker_b_dir.to_string_lossy().to_string();

    repo.git_og_with_env(
        &["checkout", "-b", "daemon-race-parallel-worker-a"],
        &env_refs,
    )
    .expect("checkout parallel worker-a branch should succeed");
    repo.git_og_with_env(
        &[
            "worktree",
            "add",
            "-b",
            "daemon-race-parallel-worker-b",
            worker_b_dir_str.as_str(),
        ],
        &env_refs,
    )
    .expect("worktree add parallel worker-b should succeed");
    wait_for_expected_top_level_completions(&repo, 0, 2);

    let file_count = 8usize;
    let completion_baseline = repo.daemon_total_completion_count();

    // Spawn threads to do checkpoint+add in parallel, but WITHOUT committing yet
    let worker_a_harness = harness.clone();
    let worker_a_dir_clone = worker_a_dir.clone();
    let worker_a = thread::spawn(move || {
        for idx in 0..file_count {
            let file = format!("daemon-race-parallel-a-{idx}.txt");
            let line = format!("a-parallel-ai-line-{idx}");
            worker_a_harness.write_ai_line_checkpoint_and_add(
                &worker_a_dir_clone,
                file.as_str(),
                line.as_str(),
            );
        }
    });

    let worker_b_harness = harness.clone();
    let worker_b_dir_clone = worker_b_dir.clone();
    let worker_b = thread::spawn(move || {
        for idx in 0..file_count {
            let file = format!("daemon-race-parallel-b-{idx}.txt");
            let line = format!("b-parallel-ai-line-{idx}");
            worker_b_harness.write_ai_line_checkpoint_and_add(
                &worker_b_dir_clone,
                file.as_str(),
                line.as_str(),
            );
        }
    });

    worker_a
        .join()
        .expect("parallel worker-a thread should not panic");
    worker_b
        .join()
        .expect("parallel worker-b thread should not panic");

    // Wait for all checkpoints and adds to complete before committing
    let mut expected_completions = (file_count as u64) * 2 * 2; // checkpoints + adds for both workers
    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    // Now do the commits after all checkpoints are processed
    harness.run_traced_git(&worker_a_dir, &["commit", "-m", "parallel worker-a commit"]);
    harness.run_traced_git(&worker_b_dir, &["commit", "-m", "parallel worker-b commit"]);
    expected_completions += 2; // both commits

    wait_for_expected_top_level_completions(&repo, completion_baseline, expected_completions);

    for idx in 0..file_count {
        let file_a = format!("daemon-race-parallel-a-{idx}.txt");
        let file_b = format!("daemon-race-parallel-b-{idx}.txt");
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_a_dir,
            file_a.as_str(),
            format!("a-parallel-ai-line-{idx}").as_str(),
        );
        assert_single_ai_line_for_workdir(
            &repo,
            &worker_b_dir,
            file_b.as_str(),
            format!("b-parallel-ai-line-{idx}").as_str(),
        );
    }

    let _ = repo.git_og_with_env(
        &["worktree", "remove", "--force", worker_b_dir_str.as_str()],
        &env_refs,
    );
}

// Daemon update check decision logic is tested by unit tests in
// src/commands/upgrade.rs (check_for_update_available_*). The integration
// tests that spawned a full daemon were removed because the post-shutdown
// self-update code made real HTTP calls that caused hangs/flakes.

#[test]
#[serial]
fn daemon_memory_does_not_grow_unbounded_under_trace_load() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    // Create a base commit so the repo has a valid HEAD.
    fs::write(repo.path().join("init.txt"), "init\n").expect("write failed");
    repo.git(&["add", "init.txt"]).expect("add failed");
    repo.git(&["commit", "-m", "init"]).expect("commit failed");

    let mut guard = DaemonGuard::start(&repo);
    let pid = guard.child.id();

    // Let the daemon settle after startup.
    thread::sleep(Duration::from_millis(500));
    let baseline_rss = get_rss_kb(pid).unwrap_or_else(|| {
        eprintln!(
            "WARN: /proc/{}/status not readable, skipping RSS check",
            pid
        );
        0
    });
    eprintln!("daemon pid={} baseline RSS={}KB", pid, baseline_rss);

    let worktree_str = repo.path().to_string_lossy().to_string();

    // Send 2000 complete git trace lifecycle rounds (start + exit + atexit).
    // Each round simulates a complete `git status` invocation with a unique SID.
    for batch in 0..20 {
        let mut frames = Vec::new();
        for i in 0..100u64 {
            let sid = format!("stress-{}-{}", batch, i);
            frames.push(serde_json::json!({
                "event": "start",
                "sid": &sid,
                "argv": ["git", "status"],
                "time_ns": 1000000000u64 + (batch * 100) as u64 + i,
            }));
            frames.push(serde_json::json!({
                "event": "def_repo",
                "sid": &sid,
                "worktree": &worktree_str,
                "repo": repo.path().join(".git").to_string_lossy().to_string(),
            }));
            frames.push(serde_json::json!({
                "event": "exit",
                "sid": &sid,
                "code": 0,
                "time_ns": 1000000001u64 + (batch * 100) as u64 + i,
            }));
            frames.push(trace_atexit_frame(
                &sid,
                0,
                1000000002u64 + (batch * 100) as u64 + i,
            ));
        }
        send_trace_frames(&guard.trace_socket_path, &frames);
        // Small delay to let the daemon process frames.
        thread::sleep(Duration::from_millis(50));
    }

    // Give the daemon time to finish processing all frames.
    thread::sleep(Duration::from_millis(500));

    let final_rss = get_rss_kb(pid).unwrap_or(0);
    let growth = final_rss.saturating_sub(baseline_rss);
    eprintln!(
        "daemon pid={} final RSS={}KB growth={}KB",
        pid, final_rss, growth
    );

    if baseline_rss > 0 && final_rss > 0 {
        // Memory growth should be bounded. With the leak fixes, growth should stay
        // well under 50 MB even after 2000 trace rounds.
        assert!(
            growth < 50_000,
            "daemon RSS grew by {}KB after 2000 trace rounds; expected < 50MB",
            growth,
        );
    } else {
        eprintln!("RSS measurement unavailable, verifying daemon survived load");
    }

    guard.shutdown();
}
