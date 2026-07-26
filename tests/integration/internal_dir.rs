//! Integration tests for the `GIT_AI_INTERNAL_DIR` feature.
//!
//! `GIT_AI_INTERNAL_DIR`, when set to a non-empty value, IS git-ai's internal
//! directory (verbatim, no extra `internal` segment), replacing the default
//! `~/.git-ai/internal`. Its purpose: when multiple machines share an NFS home
//! directory, each machine sets a unique machine-local `GIT_AI_INTERNAL_DIR` so
//! the machines never collide on SQLite databases, daemon sockets, or lock
//! files. When unset/empty, behavior MUST be byte-for-byte identical to the
//! default.
//!
//! These tests demonstrate the requirement on three levels:
//!   1. COLLISION when the var is unset / shared: identical resolved paths, a
//!      mutually-exclusive daemon lock on the shared path, and two "machines"
//!      sharing the very same SQLite files.
//!   2. ISOLATION when the var is set per machine: all resolved paths (4 DBs,
//!      control/trace sockets, daemon.lock, daemon.pid.json) pairwise disjoint,
//!      both daemon locks holdable simultaneously, and DB writes from one
//!      machine invisible to the other.
//!   3. END-TO-END concurrency: real daemons (two- and three-machine variants),
//!      each with its own `GIT_AI_INTERNAL_DIR`, drive real commit/authorship
//!      flows concurrently with correct line-level authorship on each -- plus the
//!      contrasting collision case where two daemons share one internal dir.
//!
//! The path-resolution tests resolve through the real production resolvers
//! (`internal_dir_path()`, `DaemonConfig::from_default_paths()`, and each DB's
//! `database_path()`) under each machine's `GIT_AI_INTERNAL_DIR`, so they fail if
//! the variable is ever ignored.
//!
//! Path-resolution tests mutate the process environment and so are `#[serial]`.
//! The end-to-end tests drive out-of-process daemons via the harness opt-in
//! `TestRepo::new_with_internal_dir`, so they do not touch the parent env and
//! need no serialization.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, configure_test_home_env, get_binary_path};
use git_ai::authorship::internal_db::InternalDatabase;
use git_ai::config::internal_dir_path;
use git_ai::daemon::{DaemonConfig, DaemonLock};
use git_ai::metrics::db::MetricsDatabase;
use git_ai::notes::db::NotesDatabase;
use git_ai::utils::LockFile;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// RAII guard that sets `GIT_AI_INTERNAL_DIR` for the duration of a `#[serial]`
/// test and restores the previous value (or unsets it) on drop, so tests do not
/// leak the env var into one another.
struct InternalDirEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl InternalDirEnvGuard {
    fn set(value: &Path) -> Self {
        let previous = std::env::var_os("GIT_AI_INTERNAL_DIR");
        unsafe { std::env::set_var("GIT_AI_INTERNAL_DIR", value) };
        Self { previous }
    }

    fn unset() -> Self {
        let previous = std::env::var_os("GIT_AI_INTERNAL_DIR");
        unsafe { std::env::remove_var("GIT_AI_INTERNAL_DIR") };
        Self { previous }
    }
}

impl Drop for InternalDirEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("GIT_AI_INTERNAL_DIR", value),
                None => std::env::remove_var("GIT_AI_INTERNAL_DIR"),
            }
        }
    }
}

/// RAII guard for resolving git-ai's production paths under a specific
/// `GIT_AI_INTERNAL_DIR`. Sets that env var AND clears the per-DB test-path
/// overrides (`GIT_AI_TEST_DB_PATH` / `GITAI_TEST_DB_PATH` /
/// `GIT_AI_TEST_METRICS_DB_PATH` / `GIT_AI_TEST_NOTES_DB_PATH`) so the real
/// `database_path()` resolvers exercise the `GIT_AI_INTERNAL_DIR` routing path
/// hermetically (regardless of any ambient override), restoring everything on
/// drop. Callers must be `#[serial(git_ai_internal_dir_env)]`.
struct ProductionResolveEnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl ProductionResolveEnvGuard {
    fn set(internal_dir: &Path) -> Self {
        const KEYS: [&str; 5] = [
            "GIT_AI_INTERNAL_DIR",
            "GIT_AI_TEST_DB_PATH",
            "GITAI_TEST_DB_PATH",
            "GIT_AI_TEST_METRICS_DB_PATH",
            "GIT_AI_TEST_NOTES_DB_PATH",
        ];
        let saved = KEYS
            .iter()
            .map(|&key| (key, std::env::var_os(key)))
            .collect();
        unsafe {
            std::env::set_var("GIT_AI_INTERNAL_DIR", internal_dir);
            std::env::remove_var("GIT_AI_TEST_DB_PATH");
            std::env::remove_var("GITAI_TEST_DB_PATH");
            std::env::remove_var("GIT_AI_TEST_METRICS_DB_PATH");
            std::env::remove_var("GIT_AI_TEST_NOTES_DB_PATH");
        }
        Self { saved }
    }
}

impl Drop for ProductionResolveEnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// The four SQLite database paths git-ai resolves for `internal_dir`, obtained
/// from the PRODUCTION resolvers under a clean env: `database_path()` for the
/// three DBs that route through `internal_dir_path()`, and the daemon config for
/// `transcripts-db`. Asserting these equal `<internal_dir>/<name>` proves the real
/// routing (NOT the test computing `internal_dir.join(name)` and comparing it to
/// itself). Caller must be `#[serial(git_ai_internal_dir_env)]`.
fn production_db_paths(internal_dir: &Path) -> [PathBuf; 4] {
    let _guard = ProductionResolveEnvGuard::set(internal_dir);
    let config = DaemonConfig::from_default_paths().expect("daemon config should resolve");
    [
        InternalDatabase::database_path_for_test().expect("authorship db path"),
        MetricsDatabase::database_path_for_test().expect("metrics db path"),
        NotesDatabase::database_path_for_test().expect("notes db path"),
        config.transcripts_db_path(),
    ]
}

/// Every machine-scoped runtime path git-ai resolves for `internal_dir`, obtained
/// from PRODUCTION code under a clean env (daemon control/trace sockets, lock,
/// daemon.pid.json, and the four SQLite DBs). Used to assert disjointness /
/// overlap between internal dirs THROUGH the real env-driven resolution. Caller
/// must be `#[serial(git_ai_internal_dir_env)]`.
fn production_runtime_paths(internal_dir: &Path) -> Vec<PathBuf> {
    let _guard = ProductionResolveEnvGuard::set(internal_dir);
    let config = DaemonConfig::from_default_paths().expect("daemon config should resolve");
    let pid_meta = config
        .lock_path
        .parent()
        .expect("daemon lock path must have a parent")
        .join("daemon.pid.json");
    vec![
        config.control_socket_path.clone(),
        config.trace_socket_path.clone(),
        config.lock_path.clone(),
        pid_meta,
        InternalDatabase::database_path_for_test().expect("authorship db path"),
        MetricsDatabase::database_path_for_test().expect("metrics db path"),
        NotesDatabase::database_path_for_test().expect("notes db path"),
        config.transcripts_db_path(),
    ]
}

fn unique_internal_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "git-ai-internal-dir-{}-{}-{}",
        label,
        std::process::id(),
        nanos
    ))
}

// ---------------------------------------------------------------------------
// 1. Path resolution: the var IS the internal dir (verbatim), unset == default
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_env_is_used_verbatim_for_all_runtime_paths() {
    let machine = unique_internal_dir("verbatim");
    let _guard = InternalDirEnvGuard::set(&machine);

    // internal_dir_path() returns the value VERBATIM -- no `.git-ai`/`internal`
    // segment appended.
    let resolved = internal_dir_path().expect("internal_dir_path should resolve when env set");
    assert_eq!(
        resolved, machine,
        "GIT_AI_INTERNAL_DIR must be used verbatim as the internal dir"
    );
    // The equality above already proves the value is used verbatim; this
    // independently guards against the specific regression of appending an
    // `internal` segment to the override.
    assert_ne!(
        resolved,
        machine.join("internal"),
        "GIT_AI_INTERNAL_DIR must be used verbatim, with no appended `internal` segment"
    );

    // The 4 SQLite DBs resolve directly under the internal dir -- proven through
    // the PRODUCTION resolvers (database_path() for the three that route via
    // internal_dir_path(); the daemon config for transcripts-db), hermetic against
    // any ambient per-DB test-path override. These FAIL if the env is ignored or a
    // DB filename changes (not the test comparing its own arithmetic to itself).
    let [authorship, metrics, notes, transcripts] = production_db_paths(&machine);
    assert_eq!(authorship, machine.join("db"));
    assert_eq!(metrics, machine.join("metrics-db"));
    assert_eq!(notes, machine.join("notes-db"));
    assert_eq!(transcripts, machine.join("transcripts-db"));

    // Daemon runtime files derive from the same internal dir: the `internal_dir`
    // field is the env value verbatim.
    let config = DaemonConfig::from_default_paths()
        .expect("from_default_paths should honor GIT_AI_INTERNAL_DIR");
    assert_eq!(
        config.internal_dir, machine,
        "DaemonConfig::from_default_paths must use the env internal dir verbatim"
    );

    // The daemon's test-completion log dir is unconditionally under the internal
    // dir (no fallback applies to it).
    assert_eq!(
        config.test_completion_log_dir(),
        machine.join("daemon").join("test-completions"),
        "completion log dir must live under the internal dir"
    );

    // The lock/sockets normally live in `<internal_dir>/daemon/`. On Unix the
    // socket path has a hard length cap; when an internal dir is long enough to
    // blow the cap, production relocates the lock+sockets to a short temp dir
    // hashed from the internal dir. Either way, all three must agree with the
    // canonical resolver, which is what every git-ai process on the machine
    // uses -- so the client and daemon never disagree.
    let canonical = DaemonConfig::from_internal_dir_for_test(&machine);
    assert_eq!(config.lock_path, canonical.lock_path);
    assert_eq!(config.control_socket_path, canonical.control_socket_path);
    assert_eq!(config.trace_socket_path, canonical.trace_socket_path);
    // Every runtime path is derived from the internal dir. The lock is always a
    // real file under `<internal_dir>/daemon/`. The sockets are either filesystem
    // paths under the internal dir (Unix), a short temp dir hashed from the
    // internal dir when the AF_UNIX 104-byte path cap would be exceeded (Unix
    // `git-ai-d-<hash>`), or a Windows named pipe whose name is hashed from the
    // internal dir (`\\.\pipe\git-ai-<hash>-...`). Accept all three forms.
    let derived_from_internal = |p: &Path| {
        let s = p.to_string_lossy();
        p.starts_with(&machine) || s.contains("git-ai-d-") || s.contains(r"\pipe\git-ai-")
    };
    assert!(derived_from_internal(&config.lock_path));
    assert!(derived_from_internal(&config.control_socket_path));
    assert!(derived_from_internal(&config.trace_socket_path));
}

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_empty_or_whitespace_value_is_treated_as_unset() {
    // With the var unset, capture the default resolution.
    let default_resolved = {
        let _guard = InternalDirEnvGuard::unset();
        internal_dir_path().expect("default internal_dir_path should resolve")
    };
    assert!(
        default_resolved.ends_with("internal"),
        "default internal dir should be ~/.git-ai/internal: {}",
        default_resolved.display()
    );

    // An empty value AND a whitespace-only value must each be byte-for-byte
    // identical to unset -- a whitespace-only value is a common misconfiguration
    // that must not be turned into a literal relative directory.
    for blank in ["", "   ", "\t"] {
        let resolved = {
            let _guard = InternalDirEnvGuard::set(Path::new(blank));
            internal_dir_path().expect("blank internal_dir_path should resolve")
        };
        assert_eq!(
            resolved, default_resolved,
            "GIT_AI_INTERNAL_DIR={blank:?} must behave exactly like unset"
        );
    }
}

// ---------------------------------------------------------------------------
// 1 + 2. Collision (shared/unset) vs Isolation (distinct) of resolved paths
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_distinct_machines_resolve_pairwise_disjoint_paths() {
    let machine_a = unique_internal_dir("isolation-a");
    let machine_b = unique_internal_dir("isolation-b");
    assert_ne!(machine_a, machine_b);

    // Resolve every runtime path for each machine THROUGH PRODUCTION CODE under
    // that machine's GIT_AI_INTERNAL_DIR (env-grounded, hermetic), then assert
    // pairwise disjointness.
    let paths_a = production_runtime_paths(&machine_a);
    let paths_b = production_runtime_paths(&machine_b);

    // ISOLATION: every runtime path for machine A is disjoint from every
    // runtime path for machine B. No socket, lock, pid file, or DB collides.
    for pa in &paths_a {
        for pb in &paths_b {
            assert_ne!(
                pa,
                pb,
                "distinct internal dirs must not share any runtime path: {} vs {}",
                pa.display(),
                pb.display()
            );
        }
    }

    // And each machine's set is internally distinct (db != metrics-db, etc.).
    for i in 0..paths_a.len() {
        for j in (i + 1)..paths_a.len() {
            assert_ne!(
                paths_a[i], paths_a[j],
                "runtime paths within a single machine must be distinct"
            );
        }
    }
}

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_shared_machines_resolve_identical_colliding_paths() {
    // COLLISION: two machines that point GIT_AI_INTERNAL_DIR at the SAME dir (the
    // case the feature exists to prevent -- an NFS home with the var unset, or two
    // hosts misconfigured to the same path) resolve, THROUGH PRODUCTION CODE, to
    // the identical internal dir and therefore the identical DB and daemon
    // socket/lock files. This drives the real resolvers internal_dir_path() and
    // DaemonConfig::from_default_paths() (which read the env var), so it FAILS if
    // the variable is ever ignored -- it is not an `f(x) == f(x)` tautology.
    let resolve = |value: &Path| -> (PathBuf, [PathBuf; 4], (PathBuf, PathBuf, PathBuf)) {
        let _guard = InternalDirEnvGuard::set(value);
        let internal = internal_dir_path().expect("internal dir should resolve");
        let config = DaemonConfig::from_default_paths().expect("daemon config should resolve");
        let dbs = production_db_paths(value);
        (
            internal,
            dbs,
            (
                config.control_socket_path,
                config.trace_socket_path,
                config.lock_path,
            ),
        )
    };

    let shared = unique_internal_dir("collision-shared");
    let (internal_a, db_a, sockets_a) = resolve(&shared);
    let (internal_b, db_b, sockets_b) = resolve(&shared);

    // The production resolver HONORED the env on both machines (it would resolve
    // ~/.git-ai/internal, not `shared`, if the var were ignored).
    assert_eq!(internal_a, shared);
    assert_eq!(internal_b, shared);
    // ...so both machines collide on the identical internal dir, DB files, and
    // daemon sockets/lock.
    assert_eq!(internal_a, internal_b);
    assert_eq!(db_a, db_b);
    assert_eq!(sockets_a, sockets_b);

    // Contrast: a DISTINCT value resolves to a DIFFERENT internal dir, which is
    // exactly how a unique GIT_AI_INTERNAL_DIR per host avoids the clash.
    let distinct = unique_internal_dir("collision-distinct");
    let (internal_distinct, _, _) = resolve(&distinct);
    assert_ne!(internal_distinct, shared);
}

// ---------------------------------------------------------------------------
// 2. Daemon lock: shared path is mutually exclusive; distinct paths coexist
// ---------------------------------------------------------------------------

#[test]
fn test_internal_dir_shared_daemon_lock_is_mutually_exclusive() {
    // COLLISION proof: a single internal dir resolves to one daemon.lock path
    // that is mutually exclusive across independent file handles -- the model for
    // two separate daemon processes. (The genuine cross-process collision, where a
    // live daemon process holds the lock, is proven in
    // `test_internal_dir_end_to_end_shared_dir_second_daemon_cannot_start`.) Here
    // we validate the flock/share-mode semantics: a second acquisition on the same
    // path fails deterministically (advisory flock on a distinct fd / exclusive
    // share-mode open on Windows).
    let shared = unique_internal_dir("lock-shared");
    let config = DaemonConfig::from_internal_dir_for_test(&shared);
    let lock_path = config.lock_path.clone();

    let first = DaemonLock::acquire(&lock_path).expect("first daemon should acquire the lock");

    let second = DaemonLock::acquire(&lock_path);
    assert!(
        second.is_err(),
        "a second daemon on the SAME internal dir must NOT be able to acquire the shared lock"
    );

    // After the first holder is dropped, the lock is reacquirable.
    drop(first);
    let reacquired = DaemonLock::acquire(&lock_path);
    assert!(
        reacquired.is_ok(),
        "lock should be reacquirable once the holder is dropped"
    );
}

#[test]
fn test_internal_dir_distinct_daemon_locks_coexist() {
    // ISOLATION proof: two daemons with distinct internal dirs resolve distinct
    // daemon.lock paths and can BOTH hold their locks simultaneously.
    let machine_a = unique_internal_dir("lock-a");
    let machine_b = unique_internal_dir("lock-b");

    let lock_a = DaemonConfig::from_internal_dir_for_test(&machine_a).lock_path;
    let lock_b = DaemonConfig::from_internal_dir_for_test(&machine_b).lock_path;
    assert_ne!(
        lock_a, lock_b,
        "distinct internal dirs must use distinct locks"
    );

    let held_a = DaemonLock::acquire(&lock_a).expect("machine A should acquire its lock");
    let held_b = DaemonLock::acquire(&lock_b).expect("machine B should acquire its lock");

    // Both locks are held at the same time -- no collision.
    drop(held_a);
    drop(held_b);
}

// ---------------------------------------------------------------------------
// 2. DB isolation: a write to one internal dir's DB is invisible to another
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_distinct_db_writes_are_isolated() {
    // ISOLATION at the filesystem level, grounded in PRODUCTION resolution: with
    // GIT_AI_INTERNAL_DIR set to machine A's dir, the authorship DB resolves
    // (through the real resolver internal_dir_path()) under A's dir; likewise for
    // B. The two resolved DB files differ, so a write under A's is invisible under
    // B's. Asserting each resolved path equals `<machine>/db` fails if the env var
    // is ignored, so this is grounded in production behavior, not a bare fs check.
    let machine_a = unique_internal_dir("dbwrite-a");
    let machine_b = unique_internal_dir("dbwrite-b");

    // Resolve each machine's authorship DB path through the PRODUCTION resolver
    // (database_path()), hermetic against any ambient per-DB test override.
    let [db_a, ..] = production_db_paths(&machine_a);
    let [db_b, ..] = production_db_paths(&machine_b);
    // Production resolution honored each machine's env value (fails if ignored).
    assert_eq!(db_a, machine_a.join("db"));
    assert_eq!(db_b, machine_b.join("db"));
    assert_ne!(db_a, db_b);

    std::fs::create_dir_all(&machine_a).unwrap();
    std::fs::create_dir_all(&machine_b).unwrap();
    std::fs::write(&db_a, b"machine-a-authorship-bytes").unwrap();
    assert!(db_a.exists(), "machine A DB should exist after the write");
    assert!(
        !db_b.exists(),
        "machine B DB must NOT be created by machine A's write -- the files are isolated"
    );
}

// ---------------------------------------------------------------------------
// 3. END-TO-END: two real daemons, each with its own GIT_AI_INTERNAL_DIR,
//    driving real concurrent commit/authorship flows.
// ---------------------------------------------------------------------------

/// Drives a real, multi-commit AI/human attribution flow against `repo`,
/// asserting line-level authorship after EVERY commit. Used by both the
/// single-machine and concurrent-two-machine end-to-end tests so the two
/// machines exercise an identical, nontrivial flow.
fn drive_attribution_flow(repo: &TestRepo, file_name: &str) {
    use std::fs;

    let file_path = repo.path().join(file_name);

    // Commit 1: an untracked (legacy human) base line.
    fs::write(&file_path, "base line\n").unwrap();
    repo.stage_all_and_commit("commit 1: base").unwrap();
    let mut file = repo.filename(file_name);
    file.assert_committed_lines(crate::lines!["base line".unattributed_human()]);

    // Commit 2: a known-human line added below the base.
    fs::write(&file_path, "base line\nhuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", file_name])
        .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.commit("commit 2: human").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "human line".human(),
    ]);

    // Commit 3: an AI line appended (pre/post checkpoint flow).
    fs::write(&file_path, "base line\nhuman line\n").unwrap();
    repo.git_ai(&["checkpoint", "human", file_name]).unwrap();
    fs::write(&file_path, "base line\nhuman line\nai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", file_name]).unwrap();
    repo.stage_all_and_commit("commit 3: ai").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "human line".human(),
        "ai line".ai(),
    ]);
}

#[test]
fn test_internal_dir_end_to_end_single_machine_authorship_flow() {
    // Sanity end-to-end: a single daemon driven purely by GIT_AI_INTERNAL_DIR
    // (no daemon-home / socket / test-DB overrides) produces correct
    // line-level authorship. This proves the client, git proxy, and daemon all
    // agree on socket/lock/DB paths derived from GIT_AI_INTERNAL_DIR.
    let internal_dir = unique_internal_dir("e2e-single");
    let repo = TestRepo::new_with_internal_dir(&internal_dir);

    drive_attribution_flow(&repo, "notes.md");

    // The transcripts DB is opened by the daemon's stream worker during the
    // commit flow, so it was created UNDER the internal dir -- proving the
    // daemon resolved its DB location from GIT_AI_INTERNAL_DIR (it previously
    // bypassed the helper, reading `config.internal_dir` which now honors the
    // var).
    assert!(
        internal_dir.join("transcripts-db").exists(),
        "transcripts DB should have been created under the internal dir: {}",
        internal_dir.join("transcripts-db").display()
    );

    // The transcripts-db check above proves the LIVE DAEMON resolves its DB under
    // the internal dir. Also prove a real CLIENT subprocess (carrying
    // GIT_AI_INTERNAL_DIR in its environment) routes metrics-db there: `git-ai
    // usage` opens MetricsDatabase (via the same database_path() the verbatim/unit
    // tests pin) before its no-data exit. The DETERMINISTIC routing proof lives in
    // those tests; this is the end-to-end subprocess-env-propagation witness, so a
    // regression dropping GIT_AI_INTERNAL_DIR from the client env is caught here.
    let _ = repo.git_ai(&["usage", "--json"]);
    assert!(
        internal_dir.join("metrics-db").exists(),
        "a client subprocess with GIT_AI_INTERNAL_DIR set must create metrics-db \
         under the internal dir: {}",
        internal_dir.join("metrics-db").display()
    );
}

#[test]
fn test_internal_dir_end_to_end_two_machines_isolated_and_concurrent() {
    // ISOLATION end-to-end: two simulated machines, each with its own distinct
    // GIT_AI_INTERNAL_DIR and its own daemon, drive real commit/authorship
    // flows CONCURRENTLY. Both must produce correct line-level authorship with
    // no lock/DB contention.
    let internal_dir_a = unique_internal_dir("e2e-a");
    let internal_dir_b = unique_internal_dir("e2e-b");
    assert_ne!(internal_dir_a, internal_dir_b);

    let repo_a = TestRepo::new_with_internal_dir(&internal_dir_a);
    let repo_b = TestRepo::new_with_internal_dir(&internal_dir_b);

    // Run both flows concurrently in separate threads, capturing the repos by
    // reference (not by move) so both daemons stay alive for the whole scope.
    let (tx, rx) = mpsc::channel();
    let repo_a_ref = &repo_a;
    let repo_b_ref = &repo_b;
    thread::scope(|scope| {
        let tx_a = tx.clone();
        scope.spawn(move || {
            drive_attribution_flow(repo_a_ref, "machine_a.md");
            tx_a.send("a").unwrap();
        });
        let tx_b = tx.clone();
        scope.spawn(move || {
            drive_attribution_flow(repo_b_ref, "machine_b.md");
            tx_b.send("b").unwrap();
        });
    });
    drop(tx);
    let completed: Vec<&str> = rx.iter().collect();
    assert_eq!(
        completed.len(),
        2,
        "both concurrent machine flows should complete"
    );

    // Each machine wrote its transcripts DB into ITS OWN internal dir; neither
    // leaked into the other. The daemon's stream worker creates transcripts-db
    // during the flow.
    for dir in [&internal_dir_a, &internal_dir_b] {
        assert!(
            dir.join("transcripts-db").exists(),
            "each machine should create its own transcripts DB under its internal dir: {}",
            dir.join("transcripts-db").display()
        );
    }
}

#[test]
fn test_internal_dir_end_to_end_shared_dir_second_daemon_cannot_start() {
    // COLLISION end-to-end: a first daemon takes the daemon.lock for a given
    // internal dir. A second daemon process pointed at the SAME internal dir
    // cannot acquire that lock -- proving two machines sharing one internal dir
    // collide, which is exactly what setting distinct GIT_AI_INTERNAL_DIR
    // values prevents.
    let shared = unique_internal_dir("e2e-collision");

    // Stand up a real daemon-backed repo on the shared internal dir. Driving a
    // commit confirms its daemon is live and holding the lock.
    let repo = TestRepo::new_with_internal_dir(&shared);
    drive_attribution_flow(&repo, "shared.md");

    // The first daemon holds daemon.lock for this internal dir. A second
    // attempt on the same path fails deterministically. We assert this directly
    // on the resolved lock path (the same path the second daemon process would
    // compute), avoiding a racy second-process spawn.
    let lock_path = DaemonConfig::from_internal_dir_for_test(&shared).lock_path;
    assert!(
        lock_path.exists(),
        "the live daemon should have created daemon.lock at {}",
        lock_path.display()
    );
    let contended = LockFile::try_acquire(&lock_path);
    assert!(
        contended.is_none(),
        "a second daemon on the SHARED internal dir must not be able to take the lock held by the first"
    );

    // Sanity: the same machine on a DISTINCT internal dir CAN take its own lock.
    let distinct = unique_internal_dir("e2e-collision-distinct");
    let distinct_lock = DaemonConfig::from_internal_dir_for_test(&distinct).lock_path;
    if let Some(parent) = distinct_lock.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let distinct_held = LockFile::try_acquire(&distinct_lock);
    assert!(
        distinct_held.is_some(),
        "a daemon on a distinct internal dir should freely acquire its own lock"
    );
    drop(distinct_held);

    // Keep the live daemon-backed repo alive until here so the lock stays held
    // for the contended assertion above.
    let _ = &repo;
    // Give a moment for any in-flight daemon work to settle before teardown.
    thread::sleep(Duration::from_millis(50));
}

// ---------------------------------------------------------------------------
// 4. ENV PROPAGATION ON DETACHED AUTO-SPAWN (the spec's "CRITICAL CORRECTNESS"
//    invariant): when a client/proxy spawns the daemon detached, the daemon
//    child MUST inherit GIT_AI_INTERNAL_DIR so client and daemon compute
//    identical runtime paths. The other end-to-end tests pre-spawn the daemon
//    with GIT_AI_INTERNAL_DIR set directly on the daemon's own spawn command, so
//    they never exercise the inherit-on-detached-spawn path. This test does:
//    it sets GIT_AI_INTERNAL_DIR ONLY on a `git-ai bg start` client invocation
//    and lets THAT process spawn the daemon detached. The daemon child receives
//    the var purely by environment inheritance (production
//    `spawn_*_detached`/`spawn_*_with_piped_stderr` only env_remove git vars +
//    GIT_AI; they never env_clear nor drop GIT_AI_INTERNAL_DIR). We then prove
//    the spawned daemon resolved its paths from the inherited var by asserting
//    its sockets and lock landed under the internal dir. A regression that
//    dropped GIT_AI_INTERNAL_DIR from the detached child env would leave these
//    files absent (the daemon would resolve the DEFAULT ~/.git-ai/internal under
//    the isolated test HOME instead) and fail this test.

/// Resolve a `git-ai bg ...` client command wired to a private test HOME and a
/// machine-local `GIT_AI_INTERNAL_DIR`, with NONE of the daemon-home / socket /
/// test-DB overrides set. This is exactly how a real machine that sets only
/// `GIT_AI_INTERNAL_DIR` would invoke git-ai, so the daemon it spawns inherits
/// the var by ordinary environment propagation.
fn internal_dir_only_client(test_home: &Path, internal_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(get_binary_path());
    command.arg("bg");
    for arg in args {
        command.arg(arg);
    }
    // Isolate HOME/XDG/git config exactly like the harness does, so the DEFAULT
    // internal dir (if the var were dropped) would resolve under this throwaway
    // HOME and NOT under `internal_dir` -- making the inheritance assertion
    // unambiguous.
    configure_test_home_env(&mut command, test_home);
    // The ONLY path knob. No GIT_AI_DAEMON_HOME, no socket overrides, no test DB
    // path: the client and the daemon it spawns must agree on every runtime path
    // purely via GIT_AI_INTERNAL_DIR.
    command.env("GIT_AI_INTERNAL_DIR", internal_dir);
    // Run from the throwaway HOME (not a git repo) so commands like `bg status`
    // stay on the repo-agnostic daemon-health path and never try to resolve the
    // ambient repository (e.g. the git-ai checkout the test runs from).
    command.current_dir(test_home);
    command
}

#[test]
fn test_internal_dir_detached_autospawn_child_inherits_env() {
    let internal_dir = unique_internal_dir("autospawn");
    let test_home = unique_internal_dir("autospawn-home");
    std::fs::create_dir_all(&test_home).unwrap();
    assert_ne!(internal_dir, test_home);

    // The paths the CLIENT computes from GIT_AI_INTERNAL_DIR. The daemon it
    // spawns must compute the SAME paths -- which it can only do if it inherited
    // the var.
    let config = DaemonConfig::from_internal_dir_for_test(&internal_dir);

    // Sanity: nothing exists yet; no daemon is running on this internal dir.
    assert!(
        !config.lock_path.exists(),
        "no daemon should be running on a freshly-created internal dir"
    );

    // `git-ai bg start` is the production client path that spawns the daemon
    // detached (it is NOT gated off in test builds the way auto-spawn from a
    // git command is). The spawned daemon child inherits the parent env, which
    // carries GIT_AI_INTERNAL_DIR.
    let start = internal_dir_only_client(&test_home, &internal_dir, &["start"])
        .output()
        .expect("failed to run `git-ai bg start`");
    assert!(
        start.status.success(),
        "`git-ai bg start` should succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );

    // Ensure we always tear the daemon down, even on assertion failure, so a
    // stray daemon does not leak past the test.
    struct ShutdownGuard<'a> {
        test_home: &'a Path,
        internal_dir: &'a Path,
    }
    impl Drop for ShutdownGuard<'_> {
        fn drop(&mut self) {
            let _ =
                internal_dir_only_client(self.test_home, self.internal_dir, &["shutdown"]).output();
        }
    }
    let _guard = ShutdownGuard {
        test_home: &test_home,
        internal_dir: &internal_dir,
    };

    // PROOF OF INHERITANCE: the detached daemon created its lock + sockets under
    // the internal dir the CLIENT passed via the env. If the child had not
    // inherited GIT_AI_INTERNAL_DIR, the daemon would have resolved the default
    // ~/.git-ai/internal under the isolated test HOME, and these would never
    // appear under `internal_dir`.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !config.lock_path.exists() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        config.lock_path.exists(),
        "detached daemon must create its lock under the client's GIT_AI_INTERNAL_DIR \
         (proving env inheritance): {}",
        config.lock_path.display()
    );
    #[cfg(not(windows))]
    {
        assert!(
            config.control_socket_path.exists(),
            "detached daemon must create its control socket under the inherited internal dir: {}",
            config.control_socket_path.display()
        );
        assert!(
            config.trace_socket_path.exists(),
            "detached daemon must create its trace socket under the inherited internal dir: {}",
            config.trace_socket_path.display()
        );
    }

    // And the daemon is genuinely reachable on those inherited paths: `bg status`
    // (same client env) talks to it successfully. This proves the client and the
    // daemon AGREE on the control socket, which is the whole point.
    let status = internal_dir_only_client(&test_home, &internal_dir, &["status"])
        .output()
        .expect("failed to run `git-ai bg status`");
    assert!(
        status.status.success(),
        "`git-ai bg status` should reach the daemon the client spawned on the inherited \
         internal dir; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    // The default internal dir under the throwaway HOME must remain untouched:
    // nothing leaked to ~/.git-ai/internal, confirming the daemon used the
    // inherited var rather than falling back to the default.
    let default_daemon_dir = test_home.join(".git-ai").join("internal").join("daemon");
    assert!(
        !default_daemon_dir.exists(),
        "no daemon runtime files should appear under the default internal dir when \
         GIT_AI_INTERNAL_DIR is set: {}",
        default_daemon_dir.display()
    );
}

// ---------------------------------------------------------------------------
// 5. THREE machines (the user's literal scenario: three concurrent bg jobs on
//    three hosts sharing an NFS home, each with its own GIT_AI_INTERNAL_DIR).
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(git_ai_internal_dir_env)]
fn test_internal_dir_three_machines_resolve_globally_disjoint_paths() {
    // Generalize the disjointness proof from two machines to THREE: every runtime
    // path (4 DBs, control/trace sockets, daemon.lock, daemon.pid.json) must be
    // globally unique across all three hosts, so no pair of hosts ever collides on
    // any socket, lock, pid file, or database.
    let machines = [
        unique_internal_dir("trio-a"),
        unique_internal_dir("trio-b"),
        unique_internal_dir("trio-c"),
    ];
    // The three internal dirs themselves are distinct.
    for i in 0..machines.len() {
        for j in (i + 1)..machines.len() {
            assert_ne!(machines[i], machines[j]);
        }
    }

    // Gather every runtime path across all three machines and assert there are no
    // duplicates -- a duplicate would mean two of the three hosts share a file.
    let mut all_paths: Vec<PathBuf> = machines
        .iter()
        .flat_map(|m| production_runtime_paths(m))
        .collect();
    let total = all_paths.len();
    all_paths.sort();
    all_paths.dedup();
    assert_eq!(
        all_paths.len(),
        total,
        "three distinct internal dirs must yield globally unique runtime paths; \
         a duplicate means two of the three hosts would collide"
    );
}

#[test]
fn test_internal_dir_end_to_end_three_machines_isolated_and_concurrent() {
    // The user's exact scenario: THREE git-ai background daemons running
    // concurrently, each on its own machine-local GIT_AI_INTERNAL_DIR (as three
    // hosts sharing an NFS home would be configured). All three drive real
    // commit/authorship flows at once; each asserts correct line-level authorship
    // after every commit (inside drive_attribution_flow) and writes to fully
    // isolated databases -- no daemon lock or SQLite contention between hosts.
    let dirs = [
        unique_internal_dir("trio-e2e-a"),
        unique_internal_dir("trio-e2e-b"),
        unique_internal_dir("trio-e2e-c"),
    ];
    for i in 0..dirs.len() {
        for j in (i + 1)..dirs.len() {
            assert_ne!(dirs[i], dirs[j]);
        }
    }

    let repos: Vec<TestRepo> = dirs
        .iter()
        .map(|d| TestRepo::new_with_internal_dir(d))
        .collect();

    // Run all three attribution flows concurrently, one daemon/host per thread.
    let (tx, rx) = mpsc::channel();
    thread::scope(|scope| {
        for (idx, repo) in repos.iter().enumerate() {
            let tx = tx.clone();
            scope.spawn(move || {
                drive_attribution_flow(repo, &format!("machine_{idx}.md"));
                tx.send(idx).unwrap();
            });
        }
    });
    drop(tx);
    let completed: Vec<usize> = rx.iter().collect();
    assert_eq!(
        completed.len(),
        3,
        "all three concurrent machine flows should complete with correct authorship"
    );

    // Each of the three machines wrote its transcripts DB into ITS OWN internal
    // dir; none leaked into another.
    for dir in &dirs {
        assert!(
            dir.join("transcripts-db").exists(),
            "each of the three machines should create its own transcripts DB under its \
             internal dir: {}",
            dir.join("transcripts-db").display()
        );
    }
}
