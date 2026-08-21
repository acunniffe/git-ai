#[macro_use]
#[path = "integration/repos/mod.rs"]
mod repos;

use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::config::ConfigPatch;
use git_ai::notes::db::NotesDatabase;
use git_ai::notes::reference_server::ReferenceServer;
use repos::test_file::ExpectedLineExt;
use repos::test_repo::{DaemonTestScope, TestRepo, real_git_executable};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn wait_for_daemon_log(local: &TestRepo, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let contents = local.daemon_stderr_contents();
        if contents.contains(needle) {
            return contents;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon log did not contain {needle:?}; contents:\n{}",
            contents
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn notes_sync_http_backend_push_spawns_no_git_notes_transport() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes reference server");
    let backend_url = server.base_url();
    let log_dir = tempfile::tempdir().expect("create spawn-log directory");
    let spawn_log = log_dir.path().join("spawns.log");
    let spawn_log_string = spawn_log.to_string_lossy().to_string();
    let source = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", backend_url.as_str()),
        ("GIT_AI_API_KEY", "notes-sync-http-push-test-key"),
        ("GIT_AI_SPAWN_LOG", spawn_log_string.as_str()),
    ]);
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let upstream_path = upstream.path().to_string_lossy().to_string();
    source
        .git_og(&["remote", "add", "origin", upstream_path.as_str()])
        .expect("add origin should succeed");

    fs::write(
        source.path().join("http-push.rs"),
        "fn http_notes_only() {}\n",
    )
    .expect("write HTTP-backed AI file");
    source
        .git_ai(&["checkpoint", "mock_ai", "http-push.rs"])
        .expect("AI checkpoint should succeed");
    source.git(&["add", "-A"]).expect("add should succeed");
    source
        .git(&["commit", "-m", "HTTP notes push isolation"])
        .expect("commit should succeed");
    source.sync_daemon_force();
    let commit_sha = source
        .git(&["rev-parse", "HEAD"])
        .expect("resolve committed SHA")
        .trim()
        .to_string();
    assert_eq!(
        source
            .git(&["show", &format!("{}:http-push.rs", commit_sha)])
            .expect("read committed file"),
        "fn http_notes_only() {}\n",
        "committed content must match after the commit"
    );

    let notes_db_path = source.test_home_path().join(".git-ai/internal/notes-db");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(db) = NotesDatabase::open_at_path(&notes_db_path)
            && let Some(note) = db.get_note(&commit_sha).ok().flatten()
        {
            let log = AuthorshipLog::deserialize_from_string(&note)
                .expect("parse SQLite-backed authorship note");
            assert!(
                log.attestations.iter().any(|file| {
                    file.file_path == "http-push.rs"
                        && file.entries.iter().any(|entry| {
                            !entry.hash.starts_with("h_")
                                && entry.line_ranges.iter().any(|range| range.contains(1))
                        })
                }),
                "line 1 must retain AI attribution after the commit"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "HTTP-backed authorship note was not written to SQLite"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    fs::write(&spawn_log, "").expect("clear internal Git spawn log before push");
    source
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("branch push should succeed");
    source.sync_daemon_force();

    let spawned = fs::read_to_string(&spawn_log).unwrap_or_default();
    assert!(
        !spawned
            .lines()
            .any(|command| matches!(command, "fetch" | "push")),
        "HTTP notes backend must not spawn Git notes fetch/push commands; spawns:\n{}",
        spawned
    );
    assert!(
        upstream.read_authorship_note(&commit_sha).is_none(),
        "HTTP-backed authorship notes must not be published to refs/notes/ai"
    );
}

#[test]
fn notes_push_fetch_timeout_never_blocks_user_push_or_daemon() {
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let shim = env!("CARGO_BIN_EXE_git-ai-test-git-shim");
    let patch = ConfigPatch {
        git_path: Some(shim.to_string()),
        feature_flags: Some(serde_json::json!({
            "transcript_streaming": false,
            "transcript_sweep": false
        })),
        ..Default::default()
    };
    let patch_json = serde_json::to_string(&patch).expect("serialize daemon config patch");
    let source = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_CONFIG_PATCH", patch_json.as_str()),
        ("GIT_AI_TEST_GIT_SHIM_TARGET", real_git_executable()),
        (
            "GIT_AI_TEST_GIT_SHIM_FALLBACK_TARGET",
            real_git_executable(),
        ),
        ("GIT_AI_TEST_GIT_SHIM_DELAY_COMMANDS", "fetch,push"),
        ("GIT_AI_TEST_GIT_SHIM_DELAY_MS", "1500"),
        ("GIT_AI_TEST_NOTES_SYNC_TIMEOUT_MS", "150"),
    ]);
    let upstream_path = upstream.path().to_string_lossy().to_string();
    source
        .git_og(&["remote", "add", "origin", upstream_path.as_str()])
        .expect("add origin should succeed");

    let mut file = source.filename("async-push.rs");
    file.set_contents(vec!["fn never_block_push() {}".ai()]);
    let commit = source
        .stage_all_and_commit("async notes push")
        .expect("commit should succeed");
    file.assert_committed_lines(vec!["fn never_block_push() {}".ai()]);

    let push_started = std::time::Instant::now();
    source
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("user branch push should succeed");
    assert!(
        push_started.elapsed() < std::time::Duration::from_secs(1),
        "background notes fetch/push delay must not be charged to the user push: {:?}",
        push_started.elapsed()
    );

    let sync_started = std::time::Instant::now();
    source.sync_daemon_force();
    assert!(
        sync_started.elapsed() < std::time::Duration::from_secs(1),
        "family synchronization must not wait for the Git notes worker: {:?}",
        sync_started.elapsed()
    );

    let await_started = std::time::Instant::now();
    let await_output = source
        .git_ai(&["await", "--timeout", "5"])
        .expect("await should drain the timed-out best-effort notes push");
    assert!(
        await_output.contains("finished"),
        "unexpected await output: {await_output}"
    );
    assert!(
        await_started.elapsed() < std::time::Duration::from_secs(3),
        "per-subprocess notes timeouts must bound await: {:?}",
        await_started.elapsed()
    );
    assert!(
        upstream.read_authorship_note(&commit.commit_sha).is_none(),
        "timed-out Git notes push must not update the remote notes ref"
    );
    let daemon_log = wait_for_daemon_log(&source, "internal Git notes transport timed out");
    assert!(
        daemon_log.contains("git_notes_push_worker") && daemon_log.contains("phase=\"timeout\""),
        "daemon log must report the worker timeout with structured context:\n{}",
        daemon_log
    );
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{seq}", std::process::id()))
}

fn run_git(args: &[&str]) -> String {
    let output = Command::new(real_git_executable())
        .args(args)
        .output()
        .expect("git command should execute");

    assert!(
        output.status.success(),
        "git {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn read_note_from_worktree(repo_path: &Path, commit_sha: &str) -> Option<String> {
    repos::test_repo::TestRepo::new_at_path(repo_path).read_authorship_note(commit_sha)
}

fn panic_payload_to_string(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "unknown panic payload".to_string(),
        },
    }
}

worktree_test_wrappers! {
    fn notes_sync_clone_fetches_authorship_notes_from_origin() {

        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("clone-seed.txt"), "seed\n")
            .expect("failed to write clone seed file");
        local
            .git_og(&["add", "clone-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "clone-seed-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        let clone_dir = unique_temp_path("notes-sync-clone");
        let clone_dir_str = clone_dir.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&clone_dir);

        local
            .git(&["clone", upstream_str.as_str(), clone_dir_str.as_str()])
            .expect("clone should succeed");

        let cloned_note = read_note_from_worktree(&clone_dir, &seed_sha);
        assert!(
            cloned_note.is_some(),
            "cloned repository should have fetched authorship notes for commit {}",
            seed_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_clone_reports_local_note_update_failure() {
        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("clone-locked-seed.txt"), "seed\n")
            .expect("failed to write clone locked seed file");
        local
            .git_og(&["add", "clone-locked-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "clone locked seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "clone-locked-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        let template_dir = unique_temp_path("notes-sync-clone-template");
        let template_notes_dir = template_dir.join("refs/notes");
        fs::create_dir_all(&template_notes_dir).expect("template notes dir should be creatable");
        fs::write(template_notes_dir.join("ai.lock"), "stale lock\n")
            .expect("template notes lock should be writable");

        let clone_dir = unique_temp_path("notes-sync-clone-locked");
        let clone_dir_str = clone_dir.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let template_str = template_dir.to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&clone_dir);

        let cloned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.git(&[
                "clone",
                "--template",
                template_str.as_str(),
                upstream_str.as_str(),
                clone_dir_str.as_str(),
            ])
        }));
        let panic_message = panic_payload_to_string(cloned.expect_err(
            "clone target daemon sync must fail when notes import cannot update refs/notes/ai",
        ));
        assert!(
            panic_message.contains("daemon completion log reported an error"),
            "clone target daemon sync must report notes import failure instead of timing out or silently losing authorship for {}; got: {}",
            seed_sha,
            panic_message
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_clone_relative_target_from_external_cwd_fetches_authorship_notes() {
        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("clone-relative-seed.txt"), "seed\n")
            .expect("failed to write clone-relative seed file");
        local
            .git_og(&["add", "clone-relative-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "clone-relative-seed-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        let external_cwd = unique_temp_path("notes-sync-clone-relative-cwd");
        let _ = fs::remove_dir_all(&external_cwd);
        fs::create_dir_all(&external_cwd).expect("failed to create external cwd");

        let relative_target = "nested/relative-clone";
        let upstream_str = upstream.path().to_string_lossy().to_string();

        local
            .git_from_working_dir(&external_cwd, &["clone", upstream_str.as_str(), relative_target])
            .expect("clone from external cwd should succeed");

        let clone_dir = external_cwd.join(relative_target);
        assert!(
            clone_dir.exists(),
            "relative clone target should exist at {}",
            clone_dir.display()
        );

        let cloned_note = read_note_from_worktree(&clone_dir, &seed_sha);
        assert!(
            cloned_note.is_some(),
            "cloned repository should have fetched authorship notes for commit {}",
            seed_sha
        );
    }
}

// Regression test: clone from a non-repo directory must be handled from trace2
// alone because there is no existing repository context for the clone target.
worktree_test_wrappers! {
    fn notes_sync_clone_from_non_repo_directory_fetches_authorship_notes() {
        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("non-repo-clone-seed.txt"), "seed\n")
            .expect("failed to write seed file");
        local
            .git_og(&["add", "non-repo-clone-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "non-repo-clone-seed-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        // Clone from a non-repo directory (not inside any git repository).
        // This is the common production scenario for first-time clones.
        let external_cwd = unique_temp_path("notes-sync-clone-non-repo-cwd");
        let _ = fs::remove_dir_all(&external_cwd);
        fs::create_dir_all(&external_cwd).expect("failed to create non-repo cwd");

        let clone_target = "cloned-repo";
        let upstream_str = upstream.path().to_string_lossy().to_string();

        local
            .git_from_working_dir(
                &external_cwd,
                &["clone", upstream_str.as_str(), clone_target],
            )
            .expect("clone from non-repo cwd should succeed");

        let clone_dir = external_cwd.join(clone_target);
        assert!(
            clone_dir.exists(),
            "clone target should exist at {}",
            clone_dir.display()
        );

        let cloned_note = read_note_from_worktree(&clone_dir, &seed_sha);
        assert!(
            cloned_note.is_some(),
            "cloned repository should have fetched authorship notes for commit {} (clone from non-repo directory)",
            seed_sha
        );
    }
}

// Regression test: clone with an absolute target path from a non-repo CWD.
// Exercises the side-effect target resolution path where the clone target is
// specified as an absolute path (common in scripted / CI workflows and when
// the user types `git clone URL /some/absolute/path`).
worktree_test_wrappers! {
    fn notes_sync_clone_absolute_target_from_non_repo_cwd_fetches_authorship_notes() {

        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("abs-clone-seed.txt"), "seed\n")
            .expect("failed to write seed file");
        local
            .git_og(&["add", "abs-clone-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "abs-clone-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        // Clone from a non-repo directory using an absolute target path.
        let external_cwd = unique_temp_path("notes-sync-abs-target-cwd");
        let _ = fs::remove_dir_all(&external_cwd);
        fs::create_dir_all(&external_cwd).expect("failed to create external cwd");

        let clone_dir = unique_temp_path("notes-sync-abs-target-clone");
        let _ = fs::remove_dir_all(&clone_dir);
        let clone_dir_str = clone_dir.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();

        local
            .git_from_working_dir(
                &external_cwd,
                &["clone", upstream_str.as_str(), clone_dir_str.as_str()],
            )
            .expect("clone with absolute target should succeed");

        assert!(
            clone_dir.exists(),
            "clone target should exist at {}",
            clone_dir.display()
        );

        let cloned_note = read_note_from_worktree(&clone_dir, &seed_sha);
        assert!(
            cloned_note.is_some(),
            "cloned repository should have fetched authorship notes for commit {} (absolute target from non-repo CWD)",
            seed_sha
        );
    }
}

// Regression test: clone with NO explicit target directory (implicit target
// derived from the source URL/path).  This is the common user scenario:
//   cd ~/projects && git clone https://github.com/user/repo
// In trace2, the root process emits def_repo with the correct clone destination,
// but child processes (remote-https, index-pack) emit def_repo with the CWD as
// worktree.  The normalizer must prefer the root def_repo and ignore children.
worktree_test_wrappers! {
    fn notes_sync_clone_implicit_target_from_non_repo_cwd_fetches_authorship_notes() {

        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("implicit-seed.txt"), "seed\n")
            .expect("failed to write seed file");
        local
            .git_og(&["add", "implicit-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "implicit-clone-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        // Clone from a non-repo CWD with NO explicit target — git derives the
        // directory name from the source path (the upstream bare repo's basename).
        let external_cwd = unique_temp_path("notes-sync-clone-implicit-cwd");
        let _ = fs::remove_dir_all(&external_cwd);
        fs::create_dir_all(&external_cwd).expect("failed to create external cwd");

        let upstream_str = upstream.path().to_string_lossy().to_string();
        // Derive the expected directory name the same way git does: basename of the source.
        let expected_dir_name = Path::new(&upstream_str)
            .file_name()
            .expect("upstream path should have a filename")
            .to_string_lossy()
            .to_string();
        // Strip .git suffix if present (matches git's behavior)
        let expected_dir_name = expected_dir_name
            .strip_suffix(".git")
            .unwrap_or(&expected_dir_name);

        local
            .git_from_working_dir(&external_cwd, &["clone", upstream_str.as_str()])
            .expect("clone with implicit target should succeed");

        let clone_dir = external_cwd.join(expected_dir_name);
        assert!(
            clone_dir.exists(),
            "implicit clone target should exist at {}",
            clone_dir.display()
        );

        let cloned_note = read_note_from_worktree(&clone_dir, &seed_sha);
        assert!(
            cloned_note.is_some(),
            "cloned repository should have fetched authorship notes for commit {} (implicit target from non-repo CWD)",
            seed_sha
        );
    }
}

#[test]
fn notes_sync_http_backend_clone_warms_notes_cache() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes reference server");
    let backend_url = server.base_url();

    let source = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", backend_url.as_str()),
        ("GIT_AI_API_KEY", "notes-sync-http-clone-test-key"),
    ]);
    let notes_db_path = source
        .test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("notes-db");
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let upstream_str = upstream.path().to_string_lossy().to_string();

    source
        .git_og(&["remote", "add", "origin", upstream_str.as_str()])
        .expect("add origin should succeed");
    fs::write(source.path().join("http-clone-seed.txt"), "seed\n")
        .expect("failed to write HTTP clone seed file");
    source
        .git_og(&["add", "http-clone-seed.txt"])
        .expect("add should succeed");
    source
        .git_og(&["commit", "-m", "HTTP clone seed commit"])
        .expect("seed commit should succeed");
    source
        .git_og(&["push", "-u", "origin", "HEAD"])
        .expect("initial push should succeed");

    let seed_sha = source
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    let remote_note = "http-clone-seed-note".to_string();
    server.store().put(seed_sha.clone(), remote_note.clone());

    {
        let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db");
        assert_eq!(
            db.get_note(&seed_sha).expect("read note before clone"),
            None,
            "HTTP notes cache should be empty before clone"
        );
    }

    let clone_dir = unique_temp_path("notes-sync-http-clone");
    let clone_dir_str = clone_dir.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&clone_dir);
    source
        .git(&["clone", upstream_str.as_str(), clone_dir_str.as_str()])
        .expect("clone should succeed");

    let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db after clone");
    assert_eq!(
        db.get_note(&seed_sha).expect("read note after clone"),
        Some(remote_note),
        "clone with HTTP notes backend should warm the local notes cache for {}",
        seed_sha
    );

    let daemon_log_path = source
        .test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("daemon")
        .join("daemon.test.stderr.log");
    let daemon_log =
        fs::read_to_string(&daemon_log_path).expect("read test daemon stderr log after clone");
    assert!(
        daemon_log.contains("handling clone notes sync"),
        "daemon log should record the clone notes side effect\npath: {}\ncontents:\n{}",
        daemon_log_path.display(),
        daemon_log
    );
    assert!(
        daemon_log.contains("fetching authorship notes")
            && daemon_log.contains("backend=http")
            && daemon_log.contains("remote=origin"),
        "daemon log should record the HTTP notes fetch\npath: {}\ncontents:\n{}",
        daemon_log_path.display(),
        daemon_log
    );
}

#[test]
fn notes_sync_http_backend_internal_fetch_warms_notes_cache() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes reference server");
    let backend_url = server.base_url();
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let seed = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let upstream_str = upstream.path().to_string_lossy().to_string();
    seed.git_og(&["remote", "add", "origin", upstream_str.as_str()])
        .expect("add seed origin");
    fs::write(seed.path().join("internal-fetch.txt"), "seed\n").expect("write seed file");
    seed.git_og(&["add", "internal-fetch.txt"])
        .expect("stage seed file");
    seed.git_og(&["commit", "-m", "internal fetch seed"])
        .expect("commit seed file");
    let mut seed_file = seed.filename("internal-fetch.txt");
    seed_file.assert_committed_lines(vec!["seed".unattributed_human()]);
    seed.git_og(&["push", "-u", "origin", "HEAD"])
        .expect("push seed branch");
    let seed_sha = seed
        .git_og(&["rev-parse", "HEAD"])
        .expect("resolve seed SHA")
        .trim()
        .to_string();
    let remote_note = "internal-http-note".to_string();
    server.store().put(seed_sha.clone(), remote_note.clone());

    let local = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", backend_url.as_str()),
        ("GIT_AI_API_KEY", "notes-sync-http-internal-fetch-test-key"),
    ]);
    local
        .git_og(&["remote", "add", "origin", upstream_str.as_str()])
        .expect("add local origin");
    local
        .git_og(&["fetch", "origin"])
        .expect("fetch branch before warming notes");
    local
        .git_og(&["checkout", "--detach", seed_sha.as_str()])
        .expect("check out fetched seed commit");

    let notes_db_path = local.test_home_path().join(".git-ai/internal/notes-db");
    let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db before fetch");
    assert_eq!(db.get_note(&seed_sha).expect("read empty cache"), None);
    drop(db);

    let request = serde_json::json!({ "remote_name": "origin" }).to_string();
    let output = local
        .git_ai_with_env(
            &["fetch-authorship-notes", "--json", request.as_str()],
            &[
                ("GIT_AI_NOTES_BACKEND_KIND", "http"),
                ("GIT_AI_NOTES_BACKEND_URL", backend_url.as_str()),
                ("GIT_AI_API_KEY", "notes-sync-http-internal-fetch-test-key"),
            ],
        )
        .expect("internal HTTP fetch should succeed");
    let response: serde_json::Value =
        serde_json::from_str(output.trim()).expect("parse internal fetch response");
    assert_eq!(response["notes_existence"], "found");

    let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db after fetch");
    assert_eq!(
        db.get_note(&seed_sha).expect("read warmed cache"),
        Some(remote_note)
    );
}

worktree_test_wrappers! {
    fn notes_sync_fetch_does_not_import_authorship_notes() {
        let (local, _upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("fetch-seed.txt"), "seed\n")
            .expect("failed to write fetch seed file");
        local
            .git_og(&["add", "fetch-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "fetch-seed-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("pushing branch should succeed");
        local
            .git_og(&["push", "origin", "refs/notes/ai"])
            .expect("pushing notes should succeed");

        let _ = local.git_og(&["update-ref", "-d", "refs/notes/ai"]);
        assert!(
            local.read_authorship_note(&seed_sha).is_none(),
            "local note should be absent before fetch"
        );

        local
            .git(&["fetch", "origin"])
            .expect("fetch should succeed");

        let fetched_note = local.read_authorship_note(&seed_sha);
        assert!(
            fetched_note.is_none(),
            "plain git fetch should not import authorship note for commit {}",
            seed_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_pull_fast_forward_imports_authorship_notes() {
        let (local, upstream) = TestRepo::new_with_remote();
        let default_branch = local.current_branch();

        fs::write(local.path().join("pull-base.txt"), "base\n")
            .expect("failed to write pull base file");
        local
            .git_og(&["add", "pull-base.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "base commit"])
            .expect("base commit should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("initial push should succeed");

        let remote_clone = unique_temp_path("notes-sync-pull-remote");
        let remote_clone_str = remote_clone.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&remote_clone);

        run_git(&["clone", upstream_str.as_str(), remote_clone_str.as_str()]);
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

        fs::write(remote_clone.join("pull-remote.txt"), "remote\n")
            .expect("failed to write remote pull file");
        run_git(&["-C", remote_clone_str.as_str(), "add", "pull-remote.txt"]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "commit",
            "-m",
            "remote pull commit",
        ]);

        let remote_sha = run_git(&["-C", remote_clone_str.as_str(), "rev-parse", "HEAD"]);

        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "pull-remote-note",
            remote_sha.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            default_branch.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            "refs/notes/ai",
        ]);

        assert!(
            local.read_authorship_note(&remote_sha).is_none(),
            "local note should be absent before pull"
        );

        local
            .git(&["pull", "--ff-only", "origin", default_branch.as_str()])
            .expect("pull --ff-only should succeed");

        let pulled_note = local.read_authorship_note(&remote_sha);
        assert!(
            pulled_note.is_some(),
            "pull should import authorship note for remote commit {}",
            remote_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_pull_reports_local_note_update_failure() {
        let (local, upstream) = TestRepo::new_with_remote();
        let default_branch = local.current_branch();

        fs::write(local.path().join("pull-base.txt"), "base\n")
            .expect("failed to write pull base file");
        local
            .git_og(&["add", "pull-base.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "base commit"])
            .expect("base commit should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("initial push should succeed");

        let remote_clone = unique_temp_path("notes-sync-pull-locked-remote");
        let remote_clone_str = remote_clone.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&remote_clone);

        run_git(&["clone", upstream_str.as_str(), remote_clone_str.as_str()]);
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

        fs::write(remote_clone.join("pull-locked.txt"), "remote\n")
            .expect("failed to write remote pull file");
        run_git(&["-C", remote_clone_str.as_str(), "add", "pull-locked.txt"]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "commit",
            "-m",
            "remote pull commit with locked notes",
        ]);

        let remote_sha = run_git(&["-C", remote_clone_str.as_str(), "rev-parse", "HEAD"]);

        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "pull-locked-note",
            remote_sha.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            default_branch.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            "refs/notes/ai",
        ]);

        assert!(
            local.read_authorship_note(&remote_sha).is_none(),
            "local note should be absent before pull"
        );

        let notes_dir = local.path().join(".git/refs/notes");
        fs::create_dir_all(&notes_dir).expect("notes dir should be creatable");
        fs::write(notes_dir.join("ai.lock"), "stale lock\n")
            .expect("notes lock should be writable");

        local
            .git(&["pull", "--ff-only", "origin", default_branch.as_str()])
            .expect("pull --ff-only should succeed before daemon notes side effect runs");

        let sync = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.sync_daemon_force();
        }));
        let panic_message = panic_payload_to_string(
            sync.expect_err("daemon sync must fail when pull notes import cannot update refs/notes/ai"),
        );
        assert!(
            panic_message.contains("daemon completion log reported an error"),
            "daemon sync must report notes side-effect failure instead of silently losing authorship for {}; got: {}",
            remote_sha,
            panic_message
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_push_failure_is_best_effort_and_retries_on_later_push() {
        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("push-locked.txt"), "local\n")
            .expect("failed to write push file");
        local
            .git_og(&["add", "push-locked.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "push locked notes commit"])
            .expect("commit should succeed");
        let commit_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();
        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "push-locked-note",
                commit_sha.as_str(),
            ])
            .expect("adding local note should succeed");

        let remote_notes_dir = upstream.path().join("refs/notes");
        fs::create_dir_all(&remote_notes_dir).expect("remote notes dir should be creatable");
        fs::write(remote_notes_dir.join("ai.lock"), "stale lock\n")
            .expect("remote notes lock should be writable");

        local
            .git(&["push", "-u", "origin", "HEAD"])
            .expect("branch push should succeed before daemon notes side effect runs");

        local.sync_daemon_force();
        let daemon_log = wait_for_daemon_log(&local, "asynchronous Git notes push failed");
        assert!(
            local
                .read_authorship_note_in_git_dir(upstream.path(), &commit_sha)
                .is_none(),
            "locked remote must not receive the authorship note"
        );
        assert!(
            daemon_log.contains("git_notes_push_worker")
                && daemon_log.contains("asynchronous Git notes push failed"),
            "daemon must log the best-effort notes push failure; log:\n{}",
            daemon_log
        );

        fs::remove_file(remote_notes_dir.join("ai.lock"))
            .expect("failed to unlock remote notes ref");
        local
            .git(&["push", "origin", "HEAD"])
            .expect("later branch push should succeed");
        assert!(
            local
                .wait_for_authorship_note_in_git_dir(upstream.path(), &commit_sha)
                .is_some(),
            "a later user push should retry and deliver the durable local note"
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_push_to_explicit_path_pushes_authorship_to_same_destination() {
        let (local, _origin) = TestRepo::new_with_remote();
        let explicit_destination = repos::test_repo::TestRepo::new_bare();

        fs::write(local.path().join("push-explicit-path.txt"), "local\n")
            .expect("failed to write explicit path push file");
        local
            .git_og(&["add", "push-explicit-path.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "push explicit path notes commit"])
            .expect("commit should succeed");
        let commit_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();
        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "push-explicit-path-note",
                commit_sha.as_str(),
            ])
            .expect("adding local note should succeed");

        let explicit_destination_path = explicit_destination.path().to_string_lossy().to_string();
        local
            .git(&[
                "push",
                explicit_destination_path.as_str(),
                "HEAD:refs/heads/main",
            ])
            .expect("branch push to explicit path should succeed");
        let pushed_note = local
            .wait_for_authorship_note_in_git_dir(explicit_destination.path(), &commit_sha);
        assert!(
            pushed_note.is_some(),
            "git push to an explicit repository path must push authorship notes to that same destination for {}",
            commit_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_pull_fast_forward_syncs_only_selected_remote() {
        let (local, upstream) = TestRepo::new_with_remote();
        let backup = repos::test_repo::TestRepo::new_bare();
        let default_branch = local.current_branch();

        fs::write(local.path().join("pull-base.txt"), "base\n")
            .expect("failed to write pull base file");
        local
            .git_og(&["add", "pull-base.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "base commit"])
            .expect("base commit should succeed");

        let base_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("initial push to origin should succeed");

        let backup_path = backup.path().to_string_lossy().to_string();
        local
            .git_og(&["remote", "add", "backup", backup_path.as_str()])
            .expect("adding backup remote should succeed");
        local
            .git_og(&["push", "backup", "HEAD"])
            .expect("initial push to backup should succeed");

        let backup_clone = unique_temp_path("notes-sync-pull-backup-remote");
        let backup_clone_str = backup_clone.to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&backup_clone);

        run_git(&["clone", backup_path.as_str(), backup_clone_str.as_str()]);
        run_git(&[
            "-C",
            backup_clone_str.as_str(),
            "config",
            "user.name",
            "Test User",
        ]);
        run_git(&[
            "-C",
            backup_clone_str.as_str(),
            "config",
            "user.email",
            "test@example.com",
        ]);
        run_git(&[
            "-C",
            backup_clone_str.as_str(),
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "backup-remote-note",
            base_sha.as_str(),
        ]);
        run_git(&[
            "-C",
            backup_clone_str.as_str(),
            "push",
            "origin",
            "refs/notes/ai",
        ]);

        let origin_clone = unique_temp_path("notes-sync-pull-origin-remote");
        let origin_clone_str = origin_clone.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&origin_clone);

        run_git(&["clone", upstream_str.as_str(), origin_clone_str.as_str()]);
        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "config",
            "user.name",
            "Test User",
        ]);
        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "config",
            "user.email",
            "test@example.com",
        ]);

        fs::write(origin_clone.join("pull-selected-remote.txt"), "remote\n")
            .expect("failed to write selected remote file");
        run_git(&["-C", origin_clone_str.as_str(), "add", "pull-selected-remote.txt"]);
        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "commit",
            "-m",
            "remote pull commit",
        ]);

        let remote_sha = run_git(&["-C", origin_clone_str.as_str(), "rev-parse", "HEAD"]);

        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "origin-remote-note",
            remote_sha.as_str(),
        ]);
        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "push",
            "origin",
            default_branch.as_str(),
        ]);
        run_git(&[
            "-C",
            origin_clone_str.as_str(),
            "push",
            "origin",
            "refs/notes/ai",
        ]);

        assert!(
            local.read_authorship_note(&base_sha).is_none(),
            "backup remote note should be absent before pull"
        );
        assert!(
            local.read_authorship_note(&remote_sha).is_none(),
            "origin remote note should be absent before pull"
        );

        local
            .git(&["pull", "--ff-only", "origin", default_branch.as_str()])
            .expect("pull --ff-only should succeed");

        let pulled_origin_note = local.read_authorship_note(&remote_sha);
        assert!(
            pulled_origin_note.is_some(),
            "pull should import authorship note for selected remote commit {}",
            remote_sha
        );

        let leaked_backup_note = local.read_authorship_note(&base_sha);
        assert!(
            leaked_backup_note.is_none(),
            "pull from origin should not import backup remote note for commit {}",
            base_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_pull_rebase_imports_authorship_notes() {
        let (local, upstream) = TestRepo::new_with_remote();
        let default_branch = local.current_branch();

        fs::write(local.path().join("rebase-base.txt"), "base\n")
            .expect("failed to write rebase base file");
        local
            .git_og(&["add", "rebase-base.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "base commit"])
            .expect("base commit should succeed");
        local
            .git_og(&["push", "-u", "origin", "HEAD"])
            .expect("initial push should succeed");

        fs::write(local.path().join("local-only.txt"), "local\n")
            .expect("failed to write local-only file");
        local
            .git_og(&["add", "local-only.txt"])
            .expect("add local-only should succeed");
        local
            .git_og(&["commit", "-m", "local commit"])
            .expect("local commit should succeed");

        let remote_clone = unique_temp_path("notes-sync-rebase-remote");
        let remote_clone_str = remote_clone.to_string_lossy().to_string();
        let upstream_str = upstream.path().to_string_lossy().to_string();
        let _ = fs::remove_dir_all(&remote_clone);

        run_git(&["clone", upstream_str.as_str(), remote_clone_str.as_str()]);
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
            .expect("failed to write remote-only file");
        run_git(&["-C", remote_clone_str.as_str(), "add", "remote-only.txt"]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "commit",
            "-m",
            "remote commit",
        ]);

        let remote_sha = run_git(&["-C", remote_clone_str.as_str(), "rev-parse", "HEAD"]);

        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "notes",
            "--ref=ai",
            "add",
            "-m",
            "pull-rebase-remote-note",
            remote_sha.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            default_branch.as_str(),
        ]);
        run_git(&[
            "-C",
            remote_clone_str.as_str(),
            "push",
            "origin",
            "refs/notes/ai",
        ]);

        assert!(
            local.read_authorship_note(&remote_sha).is_none(),
            "local note should be absent before pull --rebase"
        );

        local
            .git(&["pull", "--rebase", "origin", default_branch.as_str()])
            .expect("pull --rebase should succeed");

        let pulled_note = local.read_authorship_note(&remote_sha);
        assert!(
            pulled_note.is_some(),
            "pull --rebase should import authorship note for remote commit {}",
            remote_sha
        );
    }
}

#[test]
fn notes_sync_http_backend_plain_pull_warms_notes_cache() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes reference server");
    let backend_url = server.base_url();

    let local = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", backend_url.as_str()),
        ("GIT_AI_API_KEY", "notes-sync-http-pull-test-key"),
    ]);
    let notes_db_path = local
        .test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("notes-db");
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    let default_branch = local.current_branch();
    let upstream_str = upstream.path().to_string_lossy().to_string();

    local
        .git_og(&["remote", "add", "origin", upstream_str.as_str()])
        .expect("add origin should succeed");
    fs::write(local.path().join("http-pull-base.txt"), "base\n")
        .expect("failed to write HTTP pull base file");
    local
        .git_og(&["add", "http-pull-base.txt"])
        .expect("add should succeed");
    local
        .git_og(&["commit", "-m", "base commit"])
        .expect("base commit should succeed");
    local
        .git_og(&["push", "-u", "origin", "HEAD"])
        .expect("initial push should succeed");
    local
        .git(&["config", "pull.ff", "only"])
        .expect("configure plain pull as fast-forward-only");

    let remote_clone = unique_temp_path("notes-sync-http-pull-remote");
    let remote_clone_str = remote_clone.to_string_lossy().to_string();
    let _ = fs::remove_dir_all(&remote_clone);

    run_git(&["clone", upstream_str.as_str(), remote_clone_str.as_str()]);
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

    fs::write(remote_clone.join("http-pull-remote.txt"), "remote\n")
        .expect("failed to write HTTP pull remote file");
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "add",
        "http-pull-remote.txt",
    ]);
    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "commit",
        "-m",
        "remote HTTP pull commit",
    ]);
    let remote_sha = run_git(&["-C", remote_clone_str.as_str(), "rev-parse", "HEAD"]);
    let remote_note = "http-pull-remote-note".to_string();
    server.store().put(remote_sha.clone(), remote_note.clone());

    run_git(&[
        "-C",
        remote_clone_str.as_str(),
        "push",
        "origin",
        default_branch.as_str(),
    ]);

    {
        let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db");
        assert_eq!(
            db.get_note(&remote_sha).expect("read note before pull"),
            None,
            "HTTP notes cache should be empty before pull"
        );
    }

    local.git(&["pull"]).expect("plain pull should succeed");
    local.sync_daemon_force();

    let db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db after pull");
    assert_eq!(
        db.get_note(&remote_sha).expect("read note after pull"),
        Some(remote_note),
        "plain pull with HTTP notes backend should warm the local notes cache for {}",
        remote_sha
    );

    let daemon_log_path = local
        .test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("daemon")
        .join("daemon.test.stderr.log");
    let daemon_log =
        fs::read_to_string(&daemon_log_path).expect("read test daemon stderr log after pull");
    assert!(
        daemon_log.contains("handling pull notes sync"),
        "daemon log should record the pull notes side effect\npath: {}\ncontents:\n{}",
        daemon_log_path.display(),
        daemon_log
    );
    assert!(
        daemon_log.contains("fetching authorship notes")
            && daemon_log.contains("backend=http")
            && daemon_log.contains("remote=origin"),
        "daemon log should record the HTTP notes fetch\npath: {}\ncontents:\n{}",
        daemon_log_path.display(),
        daemon_log
    );
}

worktree_test_wrappers! {
    fn notes_sync_push_propagates_authorship_notes_to_remote() {
        let (local, upstream) = TestRepo::new_with_remote();

        fs::write(local.path().join("push-seed.txt"), "seed\n")
            .expect("failed to write push seed file");
        local
            .git_og(&["add", "push-seed.txt"])
            .expect("add should succeed");
        local
            .git_og(&["commit", "-m", "seed commit"])
            .expect("seed commit should succeed");

        let seed_sha = local
            .git_og(&["rev-parse", "HEAD"])
            .expect("rev-parse should succeed")
            .trim()
            .to_string();

        local
            .git_og(&[
                "notes",
                "--ref=ai",
                "add",
                "-m",
                "push-seed-note",
                seed_sha.as_str(),
            ])
            .expect("adding notes should succeed");

        local
            .git(&["push", "-u", "origin", "HEAD"])
            .expect("push should succeed");
        let remote_note = local.wait_for_authorship_note_in_git_dir(upstream.path(), &seed_sha);
        assert!(
            remote_note.is_some(),
            "push should propagate authorship note for commit {} to upstream",
            seed_sha
        );
    }
}

worktree_test_wrappers! {
    fn notes_sync_push_propagates_authorship_notes_to_every_push_url() {
        let (local, upstream) = TestRepo::new_with_remote();
        let mirror = repos::test_repo::TestRepo::new_bare();
        let upstream_path = upstream.path().to_string_lossy().to_string();
        let mirror_path = mirror.path().to_string_lossy().to_string();
        local
            .git_og(&[
                "config",
                "--add",
                "remote.origin.pushurl",
                upstream_path.as_str(),
            ])
            .expect("first push URL should be configured");
        local
            .git_og(&[
                "config",
                "--add",
                "remote.origin.pushurl",
                mirror_path.as_str(),
            ])
            .expect("second push URL should be configured");

        let mut file = local.filename("push-mirrors.txt");
        file.set_contents(vec!["mirrored AI line".ai()]);
        let commit = local
            .stage_all_and_commit("push notes to every mirror")
            .expect("commit should succeed");
        let commit_sha = commit.commit_sha;
        file.assert_committed_lines(vec!["mirrored AI line".ai()]);

        local
            .git(&["push", "-u", "origin", "HEAD"])
            .expect("branch push to every push URL should succeed");
        assert!(
            local
                .wait_for_authorship_note_in_git_dir(upstream.path(), &commit_sha)
                .is_some(),
            "first push URL should receive authorship notes for {commit_sha}"
        );
        assert!(
            local
                .wait_for_authorship_note_in_git_dir(mirror.path(), &commit_sha)
                .is_some(),
            "second push URL should receive authorship notes for {commit_sha}"
        );
    }
}
