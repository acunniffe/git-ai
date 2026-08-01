use super::*;

#[test]
fn daemon_failed_rebase_does_not_consume_later_continue_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut shared_file = repo.filename("shared.txt");
    shared_file.set_contents(lines!["line 1".human(), "line 2".human()]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    let mut feature_file = repo.filename("shared.txt");
    feature_file.set_contents(lines!["line 1".human(), "AI feature line 2".ai()]);
    repo.stage_all_and_commit("AI feature changes")
        .expect("feature commit should succeed");
    let feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse feature should succeed")
        .trim()
        .to_string();
    assert!(
        repo.read_authorship_note(&feature_sha).is_some(),
        "feature commit should have a note before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    let mut main_file = repo.filename("shared.txt");
    main_file.set_contents(lines!["line 1".human(), "main change line 2".human()]);
    repo.stage_all_and_commit("main conflicting change")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );

    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI feature line 2\n",
    )
    .expect("failed to write resolved conflict");
    repo.git_og(&["add", "shared.txt"])
        .expect("raw add should succeed");
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("raw rebase --continue should succeed");
    let rebased_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased HEAD should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_sha, feature_sha,
        "rebase --continue should create a rewritten commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let continue_session_arg = format!("git-ai.testSyncSession={continue_session}");

    let mut frames = TraceCommandFrames::new(
        "failed-rebase-start",
        &[
            "git",
            "-c",
            rebase_session_arg.as_str(),
            "-C",
            worktree.as_str(),
            "rebase",
            default_branch.as_str(),
        ],
        worktree.as_str(),
        git_dir.as_str(),
        1_000,
    )
    .with_exit_code(1)
    .into_frames();
    frames.extend(
        TraceCommandFrames::new(
            "rebase-continue",
            &[
                "git",
                "-c",
                continue_session_arg.as_str(),
                "-C",
                worktree.as_str(),
                "rebase",
                "--continue",
            ],
            worktree.as_str(),
            git_dir.as_str(),
            2_000,
        )
        .into_frames(),
    );
    send_trace_frames(&trace_socket, &frames);
    repo.sync_daemon_external_completion_sessions(&[rebase_session, continue_session]);

    assert!(
        repo.read_authorship_note(&rebased_sha).is_some(),
        "rebased commit should get the remapped note even when failed rebase processing is delayed until after --continue"
    );
}

#[test]
fn daemon_late_cherry_pick_trace_uses_actual_destination_not_stale_commit_entry() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("picked.txt");
    file.set_contents(lines!["base".human()]);
    let base_commit = repo
        .stage_all_and_commit("base")
        .expect("base commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "source"])
        .expect("checkout source should succeed");
    file.insert_at(1, lines!["AI picked line".ai()]);
    let source_commit = repo
        .stage_all_and_commit("source change")
        .expect("source commit should succeed");
    repo.read_authorship_note(&source_commit.commit_sha)
        .expect("source commit should have an authorship note");

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");

    let mut main_file = repo.filename("main.txt");
    main_file.set_contents(lines!["main branch line".human()]);
    let main_tip = repo
        .stage_all_and_commit("main branch advance")
        .expect("main branch advance should succeed");

    fs::write(repo.path().join("stale.txt"), "stale\n").expect("write stale file");
    repo.git_og(&["add", "stale.txt"])
        .expect("raw stale add should succeed");
    repo.git_og(&["commit", "-m", "stale plain commit"])
        .expect("raw stale commit should succeed");
    let stale_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse stale commit should succeed")
        .trim()
        .to_string();
    assert_ne!(stale_commit, base_commit.commit_sha);
    assert!(
        repo.read_authorship_note(&stale_commit).is_none(),
        "raw stale commit should not have an authorship note"
    );

    repo.git_og(&["reset", "--hard", &main_tip.commit_sha])
        .expect("raw reset should succeed");
    repo.restart_dedicated_daemon_for_test();

    repo.git_og(&["cherry-pick", &source_commit.commit_sha])
        .expect("raw cherry-pick should succeed");
    let picked_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse picked commit should succeed")
        .trim()
        .to_string();
    assert_ne!(picked_commit, source_commit.commit_sha);
    assert_ne!(picked_commit, stale_commit);
    assert!(
        repo.read_authorship_note(&picked_commit).is_none(),
        "raw cherry-pick should not write the note before synthetic trace processing"
    );

    let cherry_pick_session = repos::test_repo::new_daemon_test_sync_session_id();
    let cherry_pick_session_arg = format!("git-ai.testSyncSession={cherry_pick_session}");
    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "late-cherry-pick",
                "argv": ["git", "-c", cherry_pick_session_arg, "-C", worktree, "cherry-pick", source_commit.commit_sha],
                "worktree": worktree,
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "late-cherry-pick",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "late-cherry-pick",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("late-cherry-pick", 0, 1_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[cherry_pick_session]);

    assert!(
        repo.read_authorship_note(&stale_commit).is_none(),
        "stale historical commit must not receive the cherry-pick note"
    );
    let mut file = repo.filename("picked.txt");
    file.assert_lines_and_blame(lines!["base".ai(), "AI picked line".ai(),]);
}

#[test]
fn daemon_failed_rebase_does_not_consume_later_skip_reflog_entry() {
    let repo = TestRepo::new_dedicated_daemon();
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    let mut file = repo.filename("file.txt");
    file.set_contents(lines!["line 1".human()]);
    repo.stage_all_and_commit("Initial")
        .expect("initial commit should succeed");

    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");
    file.replace_at(0, "AI line 1".ai());
    repo.stage_all_and_commit("AI changes")
        .expect("conflicting AI commit should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["// AI feature".ai()]);
    let feature_commit = repo
        .stage_all_and_commit("Add feature")
        .expect("feature commit should succeed");
    assert!(
        repo.read_authorship_note(&feature_commit.commit_sha)
            .is_some(),
        "feature commit should have a note before rebase"
    );

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    file.replace_at(0, "MAIN line 1".human());
    repo.stage_all_and_commit("Main changes")
        .expect("main commit should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    repo.sync_daemon();

    let rebase_result = repo.git_og(&["rebase", &default_branch]);
    assert!(
        rebase_result.is_err(),
        "raw rebase should fail due to conflict"
    );
    repo.git_og(&["rebase", "--skip"])
        .expect("raw rebase --skip should succeed");
    let rebased_feature_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse rebased feature should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_feature_sha, feature_commit.commit_sha,
        "rebase --skip should rewrite the following feature commit"
    );

    let rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let skip_session = repos::test_repo::new_daemon_test_sync_session_id();
    let rebase_session_arg = format!("git-ai.testSyncSession={rebase_session}");
    let skip_session_arg = format!("git-ai.testSyncSession={skip_session}");

    let mut frames = TraceCommandFrames::new(
        "failed-rebase-before-skip",
        &[
            "git",
            "-c",
            rebase_session_arg.as_str(),
            "-C",
            worktree.as_str(),
            "rebase",
            default_branch.as_str(),
        ],
        worktree.as_str(),
        git_dir.as_str(),
        1_000,
    )
    .with_exit_code(1)
    .into_frames();
    frames.extend(
        TraceCommandFrames::new(
            "rebase-skip",
            &[
                "git",
                "-c",
                skip_session_arg.as_str(),
                "-C",
                worktree.as_str(),
                "rebase",
                "--skip",
            ],
            worktree.as_str(),
            git_dir.as_str(),
            2_000,
        )
        .into_frames(),
    );
    send_trace_frames(&trace_socket, &frames);
    repo.sync_daemon_external_completion_sessions(&[rebase_session, skip_session]);

    assert!(
        repo.read_authorship_note(&rebased_feature_sha).is_some(),
        "rebased feature commit should get the remapped note when failed rebase processing is delayed until after --skip"
    );
    feature_file.assert_committed_lines(lines!["// AI feature".ai()]);
}

#[test]
#[serial]
fn daemon_trace_ingest_treats_atexit_as_terminal_for_reflog_capture() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let sid = "atexit-commit";
    let completion_baseline = repo.daemon_total_completion_count();

    send_trace_frames(
        &trace_socket,
        &[
            serde_json::json!({
                "event":"start",
                "sid":sid,
                "ts":1,
                "argv":["git","commit","-m","x"],
                "cwd":repo.path().to_string_lossy().to_string(),
            }),
            serde_json::json!({
                "event":"atexit",
                "sid":sid,
                "ts":2,
                "code":1
            }),
        ],
    );

    wait_for_expected_top_level_completions(&repo, completion_baseline, 1);

    let commands = completion_entries_for_command(&repo, "commit");
    assert!(
        commands.iter().any(|command| command.exit_code == Some(1)
            && command.status == "ok"
            && command.seq > 0),
        "atexit terminal frames should still produce a tracked commit command"
    );
}
