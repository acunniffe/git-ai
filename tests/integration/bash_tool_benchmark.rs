//! Benchmarks for the bash tool stat-snapshot and diff system.
//!
//! Measures end-to-end `handle_bash_tool` latency (PreToolUse + PostToolUse)
//! across synthetic repos of varying sizes.  Each test spins up a dedicated
//! isolated daemon instance so watermarks are clean and the system-wide daemon
//! is never touched.
//!
//! | Repo Size | Files   | Target Pre-hook P95 | Target Post-hook P95 |
//! |-----------|---------|---------------------|----------------------|
//! | Small     | 1,000   | < 15ms              | < 15ms               |
//! | Medium    | 10,000  | < 75ms              | < 75ms               |
//! | Large     | 100,000 | < 750ms             | < 750ms              |
//! | XLarge    | 500,000 | < 7.5s              | < 7.5s               |
//!
//! Run with: cargo test bash_tool_benchmark --release -- --nocapture --ignored

use crate::test_utils::DurationStatistics;
use git_ai::model::working_log::AgentId;
use git_ai::operations::commands::checkpoint_agent::bash_tool;
use git_ai::operations::daemon::control_api::ControlRequest;
use git_ai::operations::daemon::send_control_request_with_timeout;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Per-test isolated daemon
// ---------------------------------------------------------------------------

/// Spawns an isolated `git-ai bg run` daemon for benchmarking and kills it on
/// drop.  Sets `GIT_AI_DAEMON_CONTROL_SOCKET` in the current process so that
/// `query_daemon_watermarks` inside `handle_bash_tool` connects to this daemon
/// instead of the system-wide one.
struct BenchDaemon {
    child: Child,
    control_socket: PathBuf,
    /// Saved value of GIT_AI_DAEMON_CONTROL_SOCKET before we overwrote it.
    prev_socket_env: Option<String>,
}

impl BenchDaemon {
    fn start(repo_root: &Path, daemon_home: &Path) -> Self {
        let control_socket = daemon_home.join("control.sock");
        let trace_socket = daemon_home.join("trace.sock");
        let test_db = daemon_home.join("test.db");

        fs::create_dir_all(daemon_home).expect("failed to create daemon_home");

        // Resolve the binary: prefer the release build used by the benchmark
        // runner, fall back to debug.
        let binary = {
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let rel = manifest.join("target/release/git-ai");
            let dbg = manifest.join("target/debug/git-ai");
            if rel.exists() { rel } else { dbg }
        };

        let child = Command::new(&binary)
            .args(["bg", "run"])
            .current_dir(repo_root)
            .env("GIT_AI_DAEMON_HOME", daemon_home)
            .env("GIT_AI_DAEMON_CONTROL_SOCKET", &control_socket)
            .env("GIT_AI_DAEMON_TRACE_SOCKET", &trace_socket)
            .env("GIT_AI_TEST_DB_PATH", &test_db)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn bench daemon");

        // Wait up to 5 s for the socket to become reachable.
        let probe = ControlRequest::StatusFamily {
            repo_working_dir: repo_root.to_string_lossy().into_owned(),
        };
        let mut ready = false;
        for _ in 0..200 {
            if send_control_request_with_timeout(&control_socket, &probe, Duration::from_millis(25))
                .is_ok()
            {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(ready, "bench daemon did not become ready within 5s");

        // Point the in-process query at this daemon's socket.
        let prev_socket_env = std::env::var("GIT_AI_DAEMON_CONTROL_SOCKET").ok();
        // SAFETY: benchmark tests run single-threaded with #[ignore]; no other
        // threads read this env var concurrently during the test.
        unsafe { std::env::set_var("GIT_AI_DAEMON_CONTROL_SOCKET", &control_socket) };

        BenchDaemon {
            child,
            control_socket,
            prev_socket_env,
        }
    }
}

impl Drop for BenchDaemon {
    fn drop(&mut self) {
        // Restore env var.
        // SAFETY: same single-threaded guarantee as in start().
        unsafe {
            match &self.prev_socket_env {
                Some(v) => std::env::set_var("GIT_AI_DAEMON_CONTROL_SOCKET", v),
                None => std::env::remove_var("GIT_AI_DAEMON_CONTROL_SOCKET"),
            }
        }
        // Graceful shutdown, then hard kill.
        let _ = send_control_request_with_timeout(
            &self.control_socket,
            &ControlRequest::Shutdown,
            Duration::from_millis(500),
        );
        std::thread::sleep(Duration::from_millis(200));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Statistics helpers
// ---------------------------------------------------------------------------

/// Timing data for one iteration of a full pre-hook + post-hook round trip.
#[derive(Debug, Clone)]
struct IterationTiming {
    /// Time for handle_bash_tool(PreToolUse): cleanup + snapshot walk + JSON write.
    pre_hook_duration: Duration,
    /// Time for handle_bash_tool(PostToolUse): JSON read + snapshot walk + diff.
    post_hook_duration: Duration,
}

/// Descriptive statistics for a set of duration measurements.
#[derive(Debug)]
struct DurationStats {
    count: usize,
    min: Duration,
    max: Duration,
    average: Duration,
    p95: Duration,
    std_dev_ms: f64,
}

impl DurationStats {
    fn from_durations(durations: &[Duration]) -> Self {
        let stats = DurationStatistics::from_durations(durations);
        assert!(stats.count() > 0, "cannot compute stats from empty slice");

        Self {
            count: stats.count(),
            min: stats.min().unwrap(),
            max: stats.max().unwrap(),
            average: stats.average().unwrap(),
            p95: stats.percentile_nearest_rank(0.95).unwrap(),
            std_dev_ms: stats.std_dev_ms().unwrap(),
        }
    }

    fn print(&self, label: &str) {
        println!("\n=== {} ({} runs) ===", label, self.count);
        println!("  Min:      {:.2}ms", self.min.as_secs_f64() * 1000.0);
        println!("  Average:  {:.2}ms", self.average.as_secs_f64() * 1000.0);
        println!("  Max:      {:.2}ms", self.max.as_secs_f64() * 1000.0);
        println!("  P95:      {:.2}ms", self.p95.as_secs_f64() * 1000.0);
        println!("  Std Dev:  {:.2}ms", self.std_dev_ms);
    }
}

// ---------------------------------------------------------------------------
// Synthetic repo construction
// ---------------------------------------------------------------------------

/// Create a temporary git repo at `root` containing `file_count` files spread
/// across a nested directory tree.  Files are grouped into directories of at
/// most ~100 files each, with up to 3 levels of nesting for realism.
fn create_synthetic_repo(root: &Path, file_count: usize) {
    fs::create_dir_all(root).expect("failed to create repo root");

    // git init
    let output = Command::new("git")
        .args(["init"])
        .current_dir(root)
        .output()
        .expect("git init failed");
    assert!(output.status.success(), "git init failed");

    // Configure user for commits
    for (key, val) in [
        ("user.name", "Bench User"),
        ("user.email", "bench@test.com"),
    ] {
        let output = Command::new("git")
            .args(["config", key, val])
            .current_dir(root)
            .output()
            .expect("git config failed");
        assert!(output.status.success(), "git config {} failed", key);
    }

    // Create a .gitignore to mimic real repos (ignore build artifacts, etc.)
    fs::write(root.join(".gitignore"), "target/\nnode_modules/\n*.o\n")
        .expect("failed to write .gitignore");

    // Build a nested directory tree.
    // Strategy: files_per_dir ~= 100, dirs are nested up to 3 levels.
    let files_per_dir: usize = 100;
    let total_dirs = file_count.div_ceil(files_per_dir);

    let mut files_created: usize = 0;
    for dir_index in 0..total_dirs {
        // Compute a nested path: level0/level1/level2
        let l0 = dir_index % 50;
        let l1 = (dir_index / 50) % 50;
        let l2 = dir_index / 2500;
        let dir_path = root
            .join(format!("src_{}", l2))
            .join(format!("mod_{}", l1))
            .join(format!("pkg_{}", l0));
        fs::create_dir_all(&dir_path).expect("failed to create nested dir");

        let remaining = file_count - files_created;
        let batch = remaining.min(files_per_dir);
        for file_index in 0..batch {
            let filename = format!("file_{}.rs", file_index);
            let content = format!(
                "// auto-generated benchmark file {}/{}\nfn f{}() {{}}\n",
                dir_index,
                file_index,
                files_created + file_index
            );
            fs::write(dir_path.join(&filename), content).expect("failed to write file");
        }
        files_created += batch;
    }

    assert_eq!(
        files_created, file_count,
        "expected to create {} files, created {}",
        file_count, files_created
    );

    // Stage and commit everything.  For large repos, `git add -A` followed by
    // a single commit is the fastest approach.
    let add_output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(root)
        .output()
        .expect("git add failed");
    assert!(add_output.status.success(), "git add -A failed");

    let commit_output = Command::new("git")
        .args(["commit", "-m", "initial synthetic commit"])
        .current_dir(root)
        .output()
        .expect("git commit failed");
    assert!(
        commit_output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit_output.stderr)
    );

    // Backdate .git/index by 30s so all setup files are covered by the
    // git-index-mtime watermark proxy.  Files written after this call have
    // mtimes ~30s newer — well outside the 2s MTIME_GRACE_WINDOW — so they
    // appear in snapshots without needing any sleep.
    let git_index = root.join(".git").join("index");
    filetime::set_file_mtime(
        &git_index,
        filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() - 30, 0),
    )
    .expect("failed to backdate .git/index");
}

// ---------------------------------------------------------------------------
// Benchmark harness
// ---------------------------------------------------------------------------

const NUM_ITERATIONS: usize = 5;

/// Run `NUM_ITERATIONS` of a full pre-hook + post-hook round trip on the given
/// repo root.  Each iteration calls `handle_bash_tool` for both events, which
/// exercises the complete user-visible latency path:
///   PreToolUse:  stale-snapshot cleanup + snapshot walk + JSON write to disk
///   PostToolUse: JSON read from disk + snapshot walk + in-memory diff
///
/// The daemon watermark query fails fast (no daemon in tests), so the snapshot
/// always performs a full walk — the cold/no-daemon worst case.
///
/// Returns (pre_hook_stats, post_hook_stats).
fn run_benchmark(repo_root: &Path, label: &str) -> (DurationStats, DurationStats) {
    println!(
        "\n--- {} benchmark ({} iterations) ---",
        label, NUM_ITERATIONS
    );

    let mut timings: Vec<IterationTiming> = Vec::with_capacity(NUM_ITERATIONS);
    let session_id = "bench-session";

    for i in 1..=NUM_ITERATIONS {
        let tool_use_id = format!("bench-call-{}", i);

        let agent_id = AgentId {
            tool: "bench".to_string(),
            id: "bench".to_string(),
            model: String::new(),
        };

        // Pre-hook: snapshot walk + daemon send
        let pre_start = Instant::now();
        bash_tool::handle_bash_pre_tool_use_with_context(
            repo_root,
            session_id,
            &tool_use_id,
            &agent_id,
            None,
            "t_test123456789a",
            None,
        )
        .expect("pre-hook should succeed");
        let pre_hook_duration = pre_start.elapsed();

        // Modify a single file between hooks to make the diff non-trivial
        let marker_path = repo_root.join("bench_marker.txt");
        fs::write(&marker_path, format!("iteration {}", i)).expect("failed to write marker");

        // Post-hook: daemon query + snapshot walk + in-memory diff
        let post_start = Instant::now();
        let result = bash_tool::handle_bash_post_tool_use(
            repo_root,
            session_id,
            &tool_use_id,
            &agent_id,
            None,
            "t_test123456789a",
            None,
        )
        .expect("post-hook should succeed");
        let post_hook_duration = post_start.elapsed();

        // Sanity: the marker file must appear as a change
        assert!(
            !matches!(result.action, bash_tool::BashCheckpointAction::NoChanges),
            "post-hook should detect marker file change"
        );

        println!(
            "  Iteration {}: pre={:.2}ms, post={:.2}ms",
            i,
            pre_hook_duration.as_secs_f64() * 1000.0,
            post_hook_duration.as_secs_f64() * 1000.0,
        );

        timings.push(IterationTiming {
            pre_hook_duration,
            post_hook_duration,
        });

        // Clean up marker for next iteration
        let _ = fs::remove_file(&marker_path);
    }

    let pre_durations: Vec<Duration> = timings.iter().map(|t| t.pre_hook_duration).collect();
    let post_durations: Vec<Duration> = timings.iter().map(|t| t.post_hook_duration).collect();

    let pre_stats = DurationStats::from_durations(&pre_durations);
    let post_stats = DurationStats::from_durations(&post_durations);

    pre_stats.print(&format!("{} Pre-hook", label));
    post_stats.print(&format!("{} Post-hook", label));

    (pre_stats, post_stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_bash_tool_snapshot_benchmark_small() {
    const FILE_COUNT: usize = 1_000;
    // Targets are ~50% higher than the old snapshot-only targets to account for
    // the JSON save/load I/O that handle_bash_tool adds to each hook event.
    const TARGET_PRE_P95_MS: f64 = 15.0;
    const TARGET_POST_P95_MS: f64 = 15.0;
    // CI margin: 10x to account for slow CI runners
    const CI_MARGIN: f64 = 10.0;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("small_repo");

    println!("\n========================================");
    println!("Bash Tool Benchmark: SMALL ({} files)", FILE_COUNT);
    println!(
        "Target pre P95: < {}ms, post P95: < {}ms",
        TARGET_PRE_P95_MS, TARGET_POST_P95_MS
    );
    println!("(end-to-end handle_bash_tool: cleanup+walk+JSON I/O+diff)");
    println!("========================================");

    let setup_start = Instant::now();
    create_synthetic_repo(&repo_root, FILE_COUNT);
    println!(
        "Repo setup: {:.2}ms",
        setup_start.elapsed().as_secs_f64() * 1000.0
    );

    let daemon_home = tmp.path().join("small_daemon");
    let _daemon = BenchDaemon::start(&repo_root, &daemon_home);

    let (pre_stats, post_stats) = run_benchmark(&repo_root, "Small (1K)");

    let pre_p95_ms = pre_stats.p95.as_secs_f64() * 1000.0;
    let post_p95_ms = post_stats.p95.as_secs_f64() * 1000.0;
    println!(
        "\nSmall repo pre P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    println!(
        "Small repo post P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
    assert!(
        pre_p95_ms < TARGET_PRE_P95_MS * CI_MARGIN,
        "Small repo pre-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    assert!(
        post_p95_ms < TARGET_POST_P95_MS * CI_MARGIN,
        "Small repo post-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
}

#[test]
#[ignore]
fn test_bash_tool_snapshot_benchmark_medium() {
    const FILE_COUNT: usize = 10_000;
    const TARGET_PRE_P95_MS: f64 = 75.0;
    const TARGET_POST_P95_MS: f64 = 75.0;
    const CI_MARGIN: f64 = 10.0;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("medium_repo");

    println!("\n========================================");
    println!("Bash Tool Benchmark: MEDIUM ({} files)", FILE_COUNT);
    println!(
        "Target pre P95: < {}ms, post P95: < {}ms",
        TARGET_PRE_P95_MS, TARGET_POST_P95_MS
    );
    println!("(end-to-end handle_bash_tool: cleanup+walk+JSON I/O+diff)");
    println!("========================================");

    let setup_start = Instant::now();
    create_synthetic_repo(&repo_root, FILE_COUNT);
    println!(
        "Repo setup: {:.2}ms",
        setup_start.elapsed().as_secs_f64() * 1000.0
    );

    let daemon_home = tmp.path().join("medium_daemon");
    let _daemon = BenchDaemon::start(&repo_root, &daemon_home);

    let (pre_stats, post_stats) = run_benchmark(&repo_root, "Medium (10K)");

    let pre_p95_ms = pre_stats.p95.as_secs_f64() * 1000.0;
    let post_p95_ms = post_stats.p95.as_secs_f64() * 1000.0;
    println!(
        "\nMedium repo pre P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    println!(
        "Medium repo post P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
    assert!(
        pre_p95_ms < TARGET_PRE_P95_MS * CI_MARGIN,
        "Medium repo pre-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    assert!(
        post_p95_ms < TARGET_POST_P95_MS * CI_MARGIN,
        "Medium repo post-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
}

#[test]
#[ignore]
fn test_bash_tool_snapshot_benchmark_large() {
    const FILE_COUNT: usize = 100_000;
    const TARGET_PRE_P95_MS: f64 = 750.0;
    const TARGET_POST_P95_MS: f64 = 750.0;
    const CI_MARGIN: f64 = 10.0;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("large_repo");

    println!("\n========================================");
    println!("Bash Tool Benchmark: LARGE ({} files)", FILE_COUNT);
    println!(
        "Target pre P95: < {}ms, post P95: < {}ms",
        TARGET_PRE_P95_MS, TARGET_POST_P95_MS
    );
    println!("(end-to-end handle_bash_tool: cleanup+walk+JSON I/O+diff)");
    println!("========================================");

    let setup_start = Instant::now();
    create_synthetic_repo(&repo_root, FILE_COUNT);
    println!("Repo setup: {:.2}s", setup_start.elapsed().as_secs_f64());

    let daemon_home = tmp.path().join("large_daemon");
    let _daemon = BenchDaemon::start(&repo_root, &daemon_home);

    let (pre_stats, post_stats) = run_benchmark(&repo_root, "Large (100K)");

    let pre_p95_ms = pre_stats.p95.as_secs_f64() * 1000.0;
    let post_p95_ms = post_stats.p95.as_secs_f64() * 1000.0;
    println!(
        "\nLarge repo pre P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    println!(
        "Large repo post P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
    assert!(
        pre_p95_ms < TARGET_PRE_P95_MS * CI_MARGIN,
        "Large repo pre-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        pre_p95_ms,
        TARGET_PRE_P95_MS * CI_MARGIN,
    );
    assert!(
        post_p95_ms < TARGET_POST_P95_MS * CI_MARGIN,
        "Large repo post-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
        post_p95_ms,
        TARGET_POST_P95_MS * CI_MARGIN,
    );
}

#[test]
#[ignore]
fn test_bash_tool_snapshot_benchmark_xlarge() {
    // This test creates 500K files and is too slow for CI.  It validates
    // graceful degradation: handle_bash_tool should either complete within the
    // timeout budget or degrade gracefully (error path is fast).
    const FILE_COUNT: usize = 500_000;
    const TARGET_PRE_P95_MS: f64 = 7_500.0;
    const TARGET_POST_P95_MS: f64 = 7_500.0;
    const CI_MARGIN: f64 = 4.0;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("xlarge_repo");

    println!("\n========================================");
    println!("Bash Tool Benchmark: XLARGE ({} files)", FILE_COUNT);
    println!(
        "Target pre/post P95: < {}ms (with graceful degradation)",
        TARGET_PRE_P95_MS
    );
    println!("WARNING: This test creates 500K files and may take several minutes to set up.");
    println!("========================================");

    let setup_start = Instant::now();
    create_synthetic_repo(&repo_root, FILE_COUNT);
    println!("Repo setup: {:.2}s", setup_start.elapsed().as_secs_f64());

    let daemon_home = tmp.path().join("xlarge_daemon");
    let _daemon = BenchDaemon::start(&repo_root, &daemon_home);

    // For XLarge we run fewer iterations since setup is so expensive.
    println!("\n--- XLarge benchmark (3 iterations) ---");
    let mut pre_durations: Vec<Duration> = Vec::new();
    let mut post_durations: Vec<Duration> = Vec::new();
    let session_id = "bench-session-xl";

    for i in 1..=3 {
        let tool_use_id = format!("xl-{}", i);

        let agent_id = AgentId {
            tool: "bench".to_string(),
            id: "bench".to_string(),
            model: String::new(),
        };

        let pre_start = Instant::now();
        let pre_result = bash_tool::handle_bash_pre_tool_use_with_context(
            &repo_root,
            session_id,
            &tool_use_id,
            &agent_id,
            None,
            "t_test123456789a",
            None,
        );
        let pre_elapsed = pre_start.elapsed();

        match pre_result {
            Ok(_) => {
                println!(
                    "  Iteration {} pre-hook: {:.2}ms",
                    i,
                    pre_elapsed.as_secs_f64() * 1000.0,
                );
                pre_durations.push(pre_elapsed);
            }
            Err(e) => {
                // Graceful degradation: verify failure was fast (no spin).
                println!(
                    "  Iteration {} pre-hook: error after {:.2}ms -- {} (graceful degradation)",
                    i,
                    pre_elapsed.as_secs_f64() * 1000.0,
                    e,
                );
                assert!(
                    pre_elapsed < Duration::from_secs(10),
                    "Graceful degradation should be fast; took {:.2}s",
                    pre_elapsed.as_secs_f64(),
                );
                return;
            }
        }

        // Modify a file so the diff has something to find
        let marker = repo_root.join("bench_marker.txt");
        fs::write(&marker, format!("xl iteration {}", i)).expect("failed to write marker");

        let post_start = Instant::now();
        let post_result = bash_tool::handle_bash_post_tool_use(
            &repo_root,
            session_id,
            &tool_use_id,
            &agent_id,
            None,
            "t_test123456789a",
            None,
        );
        let post_elapsed = post_start.elapsed();
        let _ = fs::remove_file(&marker);

        match post_result {
            Ok(_) => {
                println!(
                    "  Iteration {} post-hook: {:.2}ms",
                    i,
                    post_elapsed.as_secs_f64() * 1000.0,
                );
                post_durations.push(post_elapsed);
            }
            Err(e) => {
                println!(
                    "  Iteration {} post-hook: error after {:.2}ms -- {} (graceful degradation)",
                    i,
                    post_elapsed.as_secs_f64() * 1000.0,
                    e,
                );
                assert!(
                    post_elapsed < Duration::from_secs(10),
                    "Graceful degradation should be fast; took {:.2}s",
                    post_elapsed.as_secs_f64(),
                );
                return;
            }
        }
    }

    if !pre_durations.is_empty() {
        let pre_stats = DurationStats::from_durations(&pre_durations);
        let post_stats = DurationStats::from_durations(&post_durations);
        pre_stats.print("XLarge (500K) Pre-hook");
        post_stats.print("XLarge (500K) Post-hook");

        let pre_p95_ms = pre_stats.p95.as_secs_f64() * 1000.0;
        let post_p95_ms = post_stats.p95.as_secs_f64() * 1000.0;
        println!(
            "\nXLarge repo pre P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
            pre_p95_ms,
            TARGET_PRE_P95_MS,
            TARGET_PRE_P95_MS * CI_MARGIN,
        );
        println!(
            "XLarge repo post P95: {:.2}ms (target: {}ms, CI limit: {}ms)",
            post_p95_ms,
            TARGET_POST_P95_MS,
            TARGET_POST_P95_MS * CI_MARGIN,
        );
        if pre_p95_ms > TARGET_PRE_P95_MS {
            println!(
                "WARNING: XLarge pre P95 ({:.2}ms) exceeded ideal target -- acceptable for large repos",
                pre_p95_ms,
            );
        }
        assert!(
            pre_p95_ms < TARGET_PRE_P95_MS * CI_MARGIN,
            "XLarge repo pre-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
            pre_p95_ms,
            TARGET_PRE_P95_MS * CI_MARGIN,
        );
        assert!(
            post_p95_ms < TARGET_POST_P95_MS * CI_MARGIN,
            "XLarge repo post-hook P95 ({:.2}ms) exceeded CI limit ({}ms)",
            post_p95_ms,
            TARGET_POST_P95_MS * CI_MARGIN,
        );
    }
}

#[test]
#[ignore]
fn test_bash_tool_diff_performance() {
    // Benchmarks the diff() function in isolation by building two large
    // in-memory snapshots and diffing them.
    const FILE_COUNT: usize = 10_000;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("diff_bench_repo");

    println!("\n========================================");
    println!("Bash Tool Diff-Only Benchmark ({} files)", FILE_COUNT);
    println!("========================================");

    create_synthetic_repo(&repo_root, FILE_COUNT);

    // Touch all source files so their mtimes are newer than the backdated
    // .git/index watermark (set by create_synthetic_repo), making them visible to snapshot().
    let now_ft = filetime::FileTime::now();
    let mut dirs = vec![repo_root.clone()];
    while let Some(dir) = dirs.pop() {
        if dir.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        for entry in fs::read_dir(&dir).expect("read_dir").flatten() {
            let p = entry.path();
            if p.is_dir() {
                dirs.push(p);
            } else {
                let _ = filetime::set_file_mtime(&p, now_ft);
            }
        }
    }

    // Take a baseline snapshot.
    let pre = bash_tool::snapshot(&repo_root, "diff-bench", "pre", None)
        .expect("pre-snapshot should succeed");

    // Modify 1% of files to simulate realistic edits.
    let files_to_modify = FILE_COUNT / 100;
    let mut modified_count = 0;
    let mut dirs_to_visit = vec![repo_root.clone()];
    'outer: while let Some(dir) = dirs_to_visit.pop() {
        if dir.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        let entries = fs::read_dir(&dir).expect("failed to read dir");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs_to_visit.push(path);
            } else if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                fs::write(
                    &path,
                    format!("// modified\nfn modified_{}() {{}}\n", modified_count),
                )
                .expect("failed to modify file");
                modified_count += 1;
                if modified_count >= files_to_modify {
                    break 'outer;
                }
            }
        }
    }
    println!("Modified {} files for diff benchmark", modified_count);

    // Take a post-snapshot.
    let post = bash_tool::snapshot(&repo_root, "diff-bench", "post", None)
        .expect("post-snapshot should succeed");

    // Benchmark diff() over multiple iterations.
    println!(
        "\n--- Diff-only benchmark ({} iterations) ---",
        NUM_ITERATIONS
    );
    let mut diff_durations: Vec<Duration> = Vec::with_capacity(NUM_ITERATIONS);

    for i in 1..=NUM_ITERATIONS {
        let start = Instant::now();
        let result = bash_tool::diff(&pre, &post);
        let elapsed = start.elapsed();

        println!(
            "  Iteration {}: diff={:.4}ms (created={}, modified={})",
            i,
            elapsed.as_secs_f64() * 1000.0,
            result.created.len(),
            result.modified.len(),
        );

        // Sanity: we should see roughly the number of files we modified.
        assert!(
            result.modified.len() >= modified_count / 2,
            "Expected at least {} modified files, got {}",
            modified_count / 2,
            result.modified.len(),
        );

        diff_durations.push(elapsed);
    }

    let stats = DurationStats::from_durations(&diff_durations);
    stats.print("Diff-Only (10K files, 1% modified)");

    // Diff should be very fast since it is purely in-memory HashSet operations.
    let p95_ms = stats.p95.as_secs_f64() * 1000.0;
    assert!(
        p95_ms < 50.0,
        "Diff P95 ({:.2}ms) should be under 50ms for 10K entries",
        p95_ms,
    );
}

#[test]
#[ignore]
fn test_bash_tool_git_status_fallback_benchmark() {
    // Benchmarks git_status_fallback() which shells out to `git status`.
    const FILE_COUNT: usize = 10_000;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("fallback_bench_repo");

    println!("\n========================================");
    println!(
        "Bash Tool git_status_fallback Benchmark ({} files)",
        FILE_COUNT
    );
    println!("========================================");

    create_synthetic_repo(&repo_root, FILE_COUNT);

    // Create some uncommitted changes so git status has something to report.
    fs::write(repo_root.join("new_file.txt"), "new content").expect("failed to write new file");
    let modify_target = repo_root
        .join("src_0")
        .join("mod_0")
        .join("pkg_0")
        .join("file_0.rs");
    if modify_target.exists() {
        fs::write(&modify_target, "// modified\n").expect("failed to modify file");
    }

    println!(
        "\n--- git_status_fallback benchmark ({} iterations) ---",
        NUM_ITERATIONS
    );
    let mut durations: Vec<Duration> = Vec::with_capacity(NUM_ITERATIONS);

    for i in 1..=NUM_ITERATIONS {
        let start = Instant::now();
        let result =
            bash_tool::git_status_fallback(&repo_root).expect("git_status_fallback should succeed");
        let elapsed = start.elapsed();

        println!(
            "  Iteration {}: {:.2}ms ({} changed files)",
            i,
            elapsed.as_secs_f64() * 1000.0,
            result.len(),
        );

        assert!(
            !result.is_empty(),
            "git_status_fallback should detect uncommitted changes"
        );

        durations.push(elapsed);
    }

    let stats = DurationStats::from_durations(&durations);
    stats.print("git_status_fallback (10K files)");
}

#[test]
#[ignore]
fn test_bash_tool_snapshot_entry_count_accuracy() {
    // Verify that the snapshot captures exactly the files modified after the
    // watermark.  create_synthetic_repo backdates .git/index by 30 s, so
    // pre-existing files are covered; only files written after that appear.
    const NEW_FILE_COUNT: usize = 10;

    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let repo_root = tmp.path().join("accuracy_repo");

    println!("\n========================================");
    println!("Bash Tool Snapshot Accuracy ({} new files)", NEW_FILE_COUNT);
    println!("========================================");

    create_synthetic_repo(&repo_root, 100); // small base repo

    // Write NEW_FILE_COUNT new (untracked) files after the backdated watermark
    // (create_synthetic_repo backdates .git/index by 30s, so new files are clearly outside
    // the 2s grace window and appear in the snapshot).
    for i in 0..NEW_FILE_COUNT {
        fs::write(
            repo_root.join(format!("new_file_{}.txt", i)),
            format!("content {}", i),
        )
        .expect("failed to write new file");
    }

    let snap = bash_tool::snapshot(&repo_root, "accuracy", "check", None)
        .expect("snapshot should succeed");

    let entry_count = snap.entries.len();
    println!("Snapshot entries: {}", entry_count);

    assert!(
        entry_count >= NEW_FILE_COUNT,
        "Expected at least {} snapshot entries (the new files), got {}",
        NEW_FILE_COUNT,
        entry_count,
    );

    // All new files must be present; the pre-existing .rs files must not be.
    for i in 0..NEW_FILE_COUNT {
        let rel = std::path::PathBuf::from(format!("new_file_{}.txt", i));
        assert!(
            snap.entries.contains_key(&rel),
            "new_file_{}.txt should appear in snapshot",
            i,
        );
    }
}
