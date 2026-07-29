use super::*;

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_fast_forward_tracks_pull_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let remote_root = tempfile::tempdir().expect("remote tempdir should be created");
    let bare_remote = remote_root.path().join("origin.git");
    let remote_clone = remote_root.path().join("origin-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("pull-case.txt"), "base\nremote update\n")
        .expect("failed to write remote update");
    run_git(&["-C", remote_clone_str.as_str(), "add", "pull-case.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    repo.git_og_with_env(
        &["pull", "--ff-only", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("fast-forward pull should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(saw_pull_success, "pull success should be tracked");
    assert!(
        fs::read_to_string(repo.path().join("pull-case.txt"))
            .expect("pulled file should be readable")
            .contains("remote update"),
        "pull fast-forward should update the worktree contents"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_rebase_tracks_pull_and_rebase_completion() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("pull-rebase-base.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "pull-rebase-base.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let unique = repo
        .path()
        .file_name()
        .expect("test repo path should have filename")
        .to_string_lossy();
    let bare_remote = root.join(format!("origin-rebase-{unique}.git"));
    let remote_clone = root.join(format!("origin-rebase-work-{unique}"));
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("remote-only.txt"), "remote\n")
        .expect("failed to write remote file");
    run_git(&["-C", remote_clone_str.as_str(), "add", "remote-only.txt"]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote commit",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(repo.path().join("local-only.txt"), "local\n").expect("failed to write local file");
    repo.git_og_with_env(&["add", "local-only.txt"], &env_refs)
        .expect("local add should succeed");
    repo.git_og_with_env(&["commit", "-m", "local commit"], &env_refs)
        .expect("local commit should succeed");

    repo.git_og_with_env(
        &["pull", "--rebase", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pull --rebase should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 7);

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_rebase_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_rebase_success,
        "pull --rebase success should be tracked"
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_pull_autostash_preserves_local_changes_and_tracks_command() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    let run_git = |args: &[&str]| -> String {
        let output = Command::new(real_git_executable())
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    fs::write(repo.path().join("autostash-local.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "autostash-local.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    let root = repo
        .path()
        .parent()
        .expect("test repo path should have parent")
        .to_path_buf();
    let bare_remote = root.join("origin-autostash.git");
    let remote_clone = root.join("origin-autostash-work");
    let bare_remote_str = bare_remote.to_string_lossy().to_string();
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&bare_remote);
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["init", "--bare", bare_remote_str.as_str()]);
    repo.git_og_with_env(
        &["remote", "add", "origin", bare_remote_str.as_str()],
        &env_refs,
    )
    .expect("adding origin remote should succeed");
    repo.git_og_with_env(
        &["push", "-u", "origin", default_branch.as_str()],
        &env_refs,
    )
    .expect("pushing base branch should succeed");

    run_git(&[
        "clone",
        "--branch",
        default_branch.as_str(),
        bare_remote_str.as_str(),
        remote_clone_str.as_str(),
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.name",
        "Test User",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "config",
        "user.email",
        "test@example.com",
    ]);
    fs::write(remote_clone.join("autostash-remote.txt"), "remote\n")
        .expect("failed to write remote update file");
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "add",
        "autostash-remote.txt",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote update",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        format!("HEAD:{}", default_branch).as_str(),
    ]);

    fs::write(
        repo.path().join("autostash-local.txt"),
        "base\nlocal dirty change\n",
    )
    .expect("failed to write local dirty change");

    repo.git_og_with_env(
        &[
            "pull",
            "--rebase",
            "--autostash",
            "origin",
            default_branch.as_str(),
        ],
        &env_refs,
    )
    .expect("pull --rebase --autostash should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 5);

    let local_contents = fs::read_to_string(repo.path().join("autostash-local.txt"))
        .expect("local file should remain readable");
    assert!(
        local_contents.contains("local dirty change"),
        "autostash pull should preserve local dirty change content"
    );

    let pull_entries = completion_entries_for_command(&repo, "pull");
    let saw_pull_autostash_success = pull_entries.iter().any(|entry| entry.exit_code == Some(0));
    assert!(
        saw_pull_autostash_success,
        "pull --rebase --autostash success should be tracked"
    );
}

#[test]
fn daemon_delayed_pull_rebase_autostash_does_not_consume_later_commit() {
    let (local, _upstream) =
        TestRepo::new_with_remote_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&local);
    let worktree = repo_workdir_string(&local);
    let git_dir = local.path().join(".git").to_string_lossy().to_string();

    let mut readme = local.filename("README.md");
    readme.set_contents(lines!["# Test Repo".human()]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    readme.assert_committed_lines(lines!["# Test Repo".human()]);

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    let mut committed_ai = local.filename("ai_feature.txt");
    committed_ai.set_contents(lines![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    let local_ai = local
        .stage_all_and_commit("add AI feature")
        .expect("AI feature commit should succeed");
    committed_ai.assert_committed_lines(lines![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);

    let branch = local.current_branch();
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial commit should succeed");

    let mut upstream_file = local.filename("upstream_change.txt");
    upstream_file.set_contents(lines!["upstream content".human()]);
    local
        .stage_all_and_commit("upstream divergent commit")
        .expect("upstream commit should succeed");
    upstream_file.assert_committed_lines(lines!["upstream content".human()]);

    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("force push upstream commit should succeed");
    local
        .git(&["reset", "--hard", &local_ai.commit_sha])
        .expect("reset back to local AI commit should succeed");

    let mut uncommitted_ai = local.filename("uncommitted_ai.txt");
    uncommitted_ai.set_contents(lines!["Uncommitted AI line".ai()]);
    local
        .git_ai(&["checkpoint", "mock_ai", "uncommitted_ai.txt"])
        .expect("checkpoint should succeed");
    local.sync_daemon();

    local
        .git_og(&["pull", "--rebase", "--autostash"])
        .expect("raw pull --rebase --autostash should succeed");
    local
        .git_og(&["add", "-A"])
        .expect("raw add should succeed");
    local
        .git_og(&["commit", "-m", "commit uncommitted AI work"])
        .expect("raw commit should succeed");
    let final_commit = local
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse final commit should succeed")
        .trim()
        .to_string();

    let pull_session = repos::test_repo::new_daemon_test_sync_session_id();
    let commit_session = repos::test_repo::new_daemon_test_sync_session_id();
    let pull_session_arg = format!("git-ai.testSyncSession={pull_session}");
    let commit_session_arg = format!("git-ai.testSyncSession={commit_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-pull-autostash",
                "argv": ["git", "-c", pull_session_arg, "-C", worktree, "pull", "--rebase", "--autostash"],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-pull-autostash",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-pull-autostash",
                "code": 0,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-pull-autostash", 0, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-commit-after-pull",
                "argv": ["git", "-c", commit_session_arg, "-C", worktree, "commit", "-m", "commit uncommitted AI work"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-commit-after-pull",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-commit-after-pull",
                "code": 0,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-commit-after-pull", 0, 2_101u64),
        ],
    );
    local.sync_daemon_external_completion_sessions(&[pull_session, commit_session]);

    assert!(
        local.read_authorship_note(&final_commit).is_some(),
        "delayed pull processing must not consume the following commit reflog entry"
    );
    uncommitted_ai.assert_committed_lines(lines!["Uncommitted AI line".ai()]);
}

#[test]
fn daemon_delayed_failed_rebase_continue_does_not_consume_final_continue() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let trace_socket = daemon_trace_socket_path(&repo);
    let worktree = repo_workdir_string(&repo);
    let git_dir = repo.path().join(".git").to_string_lossy().to_string();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 0\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 0\nBATCH = 10\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "Initial config_b"]).unwrap();
    let main_branch = repo.current_branch();

    fs::write(repo.path().join("config_a.py"), "FLAG_A = 1\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets flag_a"]).unwrap();
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 50\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og(&["commit", "-m", "main sets config_b"])
        .unwrap();

    let base_sha = repo
        .git_og(&["rev-parse", "HEAD~2"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    let mut module_a = repo.filename("module_a.py");
    module_a.set_contents(lines!["class ModuleA:".ai(), "    pass".ai()]);
    let original_c1 = repo.stage_all_and_commit("feat: C1 add ModuleA").unwrap();
    module_a.assert_committed_lines(lines!["class ModuleA:".ai(), "    pass".ai()]);

    let mut config_a = repo.filename("config_a.py");
    config_a.set_contents(lines!["FLAG_A = 2".ai()]);
    let original_c2 = repo.stage_all_and_commit("feat: C2 sets flag_a").unwrap();
    config_a.assert_committed_lines(lines!["FLAG_A = 2".ai()]);

    let mut module_c = repo.filename("module_c.py");
    module_c.set_contents(lines!["class ModuleC:".ai(), "    pass".ai()]);
    let original_c3 = repo.stage_all_and_commit("feat: C3 add ModuleC").unwrap();
    module_c.assert_committed_lines(lines!["class ModuleC:".ai(), "    pass".ai()]);

    let mut config_b = repo.filename("config_b.py");
    config_b.set_contents(lines!["FLAG_B = 1".ai(), "BATCH = 200".ai()]);
    let original_c4 = repo.stage_all_and_commit("feat: C4 sets batch").unwrap();
    config_b.assert_committed_lines(lines!["FLAG_B = 1".ai(), "BATCH = 200".ai()]);

    let mut module_e = repo.filename("module_e.py");
    module_e.set_contents(lines!["class ModuleE:".ai(), "    pass".ai()]);
    let original_c5 = repo.stage_all_and_commit("feat: C5 add ModuleE").unwrap();
    module_e.assert_committed_lines(lines!["class ModuleE:".ai(), "    pass".ai()]);
    for commit in [
        &original_c1,
        &original_c2,
        &original_c3,
        &original_c4,
        &original_c5,
    ] {
        assert!(
            repo.read_authorship_note(&commit.commit_sha).is_some(),
            "original feature commit should have authorship note"
        );
    }
    repo.sync_daemon();

    assert!(
        repo.git_og(&["rebase", &main_branch]).is_err(),
        "initial raw rebase should stop at config_a conflict"
    );
    fs::write(repo.path().join("config_a.py"), "FLAG_A = 2\n").unwrap();
    repo.git_og(&["add", "config_a.py"]).unwrap();
    assert!(
        repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
            .is_err(),
        "first raw rebase --continue should stop at config_b conflict"
    );
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 75\n").unwrap();
    repo.git_og(&["add", "config_b.py"]).unwrap();
    repo.git_og_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
        .expect("final raw rebase --continue should finish");

    let final_chain = (0..5)
        .rev()
        .map(|offset| {
            let rev = if offset == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{offset}")
            };
            repo.git_og(&["rev-parse", &rev])
                .unwrap()
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>();

    let initial_rebase_session = repos::test_repo::new_daemon_test_sync_session_id();
    let first_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let final_continue_session = repos::test_repo::new_daemon_test_sync_session_id();
    let initial_session_arg = format!("git-ai.testSyncSession={initial_rebase_session}");
    let first_continue_session_arg = format!("git-ai.testSyncSession={first_continue_session}");
    let final_continue_session_arg = format!("git-ai.testSyncSession={final_continue_session}");

    send_trace_frames(
        &trace_socket,
        &[
            json!({
                "event": "start",
                "sid": "delayed-rebase-start",
                "argv": ["git", "-c", initial_session_arg, "-C", worktree, "rebase", main_branch],
                "time_ns": 1_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-rebase-start",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 1_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-rebase-start",
                "code": 1,
                "time_ns": 1_100u64,
            }),
            trace_atexit_frame("delayed-rebase-start", 1, 1_101u64),
            json!({
                "event": "start",
                "sid": "delayed-first-rebase-continue",
                "argv": ["git", "-c", first_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 2_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-first-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 2_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-first-rebase-continue",
                "code": 1,
                "time_ns": 2_100u64,
            }),
            trace_atexit_frame("delayed-first-rebase-continue", 1, 2_101u64),
            json!({
                "event": "start",
                "sid": "delayed-final-rebase-continue",
                "argv": ["git", "-c", final_continue_session_arg, "-C", worktree, "rebase", "--continue"],
                "time_ns": 3_000u64,
            }),
            json!({
                "event": "def_repo",
                "sid": "delayed-final-rebase-continue",
                "worktree": worktree,
                "repo": git_dir,
                "time_ns": 3_001u64,
            }),
            json!({
                "event": "exit",
                "sid": "delayed-final-rebase-continue",
                "code": 0,
                "time_ns": 3_100u64,
            }),
            trace_atexit_frame("delayed-final-rebase-continue", 0, 3_101u64),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[
        initial_rebase_session,
        first_continue_session,
        final_continue_session,
    ]);

    for (idx, sha) in final_chain.iter().enumerate() {
        assert!(
            repo.read_authorship_note(sha).is_some(),
            "rebased commit {} should have authorship note after delayed continue processing",
            idx + 1
        );
    }
    module_e.assert_committed_lines(lines!["class ModuleE:".ai(), "    pass".ai()]);
}
