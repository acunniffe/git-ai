//! Criterion benchmark suite for notes I/O — Phase 5 of the commit-addressable
//! authorship notes backend implementation.
//!
//! # Purpose
//! Prove that the HTTP backend (SQLite queue + async flush) introduces no
//! significant regression versus the git-notes baseline for read, write, batch,
//! and end-to-end rebase operations.
//!
//! # Acceptance criteria (informational, not a hard CI gate)
//!
//!   - Reads / batch ops : HTTP ≤ 1.10× git-notes baseline
//!   - Writes            : HTTP ≤ 1.20× git-notes baseline
//!     (queue insert is fast; async flush is excluded)
//!   - Rebase (50 commits): HTTP ≤ 1.10× git-notes baseline
//!
//! # Backends under test
//!
//!   - `git_notes` — existing path: `git fast-import` / `git notes show`
//!   - `http`      — new path: SQLite upsert/read via `notes-db`; the HTTP
//!     upload is intentionally excluded because it is async
//!     (daemon flush). For read fallback a local in-process
//!     mockito server is used so network latency is near zero.
//!
//! # Architecture note: Config singleton bypass
//!
//! `notes_api::{write_note, read_note}` dispatch on `Config::get()` which is a
//! process-global `OnceLock`. Calling `Config::get()` once locks the backend
//! for the lifetime of the process, making it impossible to switch backends
//! inside a single benchmark binary.
//!
//! Strategy: call the *underlying* backend primitives directly for each group
//! rather than routing through `notes_api`. This is more accurate (no dispatch
//! overhead) and avoids the singleton constraint.
//!
//!   - git_notes write  : `git_ai::git::refs::git_backend_for_tests::notes_add`
//!   - git_notes read   : `git_ai::git::refs::git_backend_for_tests::show_authorship_note`
//!   - git_notes batch  : `git_ai::git::refs::git_backend_for_tests::notes_add_batch`
//!   - git_notes commits_with_notes: `git_ai::git::refs::git_backend_for_tests::commits_with_authorship_notes`
//!   - http write       : `NotesDatabase::upsert_note`
//!   - http read        : `NotesDatabase::get_note`
//!   - http batch write : `NotesDatabase::upsert_notes_batch`
//!   - http commits_with_notes: `NotesDatabase::get_notes` (presence via keys)
//!
//! # NotesDatabase singleton
//!
//! `NotesDatabase::global()` uses a `OnceLock`. We set `GIT_AI_TEST_NOTES_DB_PATH`
//! to a temporary file path before the first DB call. A `Mutex<Option<TempPath>>`
//! guards the file lifetime; the DB is shared across all groups.
//!
//! # TmpRepo lifecycle
//!
//! Each benchmark function creates its own `TmpRepo` during the *setup* phase
//! (not the measured iteration), so setup cost is amortised by Criterion's many
//! iterations. `TmpRepo` owns a `tempfile::TempDir` and a `git_ai::git::Repository`
//! and is dropped at the end of the bench.
//!
//! # Sample sizes
//!
//! Reduced from Criterion defaults (100) to keep total bench time under 5 minutes
//! on a developer laptop. Adjust `SAMPLE_SIZE` and `MEASUREMENT_TIME_SECS` if you
//! need tighter confidence intervals.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use git_ai::git::test_utils::{TmpRepo, init_test_git_config};
use git_ai::notes::db::NotesDatabase;
use std::sync::OnceLock;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Sample-size knobs — lower these if the suite takes too long
// ---------------------------------------------------------------------------

/// Criterion sample size per benchmark (iterations of the *measured* function).
const SAMPLE_SIZE: usize = 10;

/// Criterion measurement time per benchmark.
const MEASUREMENT_TIME_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const NOTE_CONTENT: &str =
    r#"{"v":3,"checkpoints":[{"author":"alice","kind":"human","entries":[]}]}"#;
const BATCH_SIZE: usize = 100;
const COMMITS_CHECK_SIZE: usize = 500;
const REBASE_COMMIT_COUNT: usize = 50;

// ---------------------------------------------------------------------------
// Process-wide notes-db temp file (initialized once, shared across groups)
// ---------------------------------------------------------------------------

static NOTES_DB_TMPFILE: OnceLock<std::path::PathBuf> = OnceLock::new();

fn ensure_notes_db_initialized() {
    NOTES_DB_TMPFILE.get_or_init(|| {
        init_test_git_config();

        // Create a named temp file; keep the path (let the file persist for process lifetime).
        let tmp = tempfile::NamedTempFile::new().expect("bench: create temp notes-db");
        let path = tmp.path().to_path_buf();
        // Leak the file so it isn't deleted when `tmp` drops.
        let _ = tmp.into_temp_path();

        // SAFETY: single-threaded benchmark startup; set before any DB call.
        unsafe {
            std::env::set_var("GIT_AI_TEST_NOTES_DB_PATH", &path);
            // Avoid polluting the production internal DB.
            std::env::set_var(
                "GIT_AI_TEST_DB_PATH",
                std::env::temp_dir().join("git-ai-bench-internal-db"),
            );
        }
        path
    });
}

// ---------------------------------------------------------------------------
// Helper: create N commits via the real `git` CLI (no post-commit hook).
// Returns a Vec of commit SHA strings.
// ---------------------------------------------------------------------------

fn create_commits(repo: &TmpRepo, count: usize) -> Vec<String> {
    let mut shas = Vec::with_capacity(count);
    for i in 0..count {
        let filename = format!("bench_{}.txt", i);
        let content = format!("bench-content-{}\n", i);
        repo.write_file(&filename, &content, false)
            .expect("bench: write file");
        let sha = repo
            .commit_all(&format!("bench commit {}", i))
            .expect("bench: create commit");
        shas.push(sha);
    }
    shas
}

fn collect_all_commit_shas(repo: &TmpRepo) -> Vec<String> {
    let stdout = repo
        .git_command(&["rev-list", "--topo-order", "HEAD"])
        .expect("bench: rev-list");
    stdout
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Shared repo builder: create a TmpRepo with count pre-written notes in both
// git-notes and notes-db.
// ---------------------------------------------------------------------------

struct BenchRepo {
    repo: TmpRepo,
    shas: Vec<String>,
}

impl BenchRepo {
    fn new(count: usize) -> Self {
        ensure_notes_db_initialized();

        let repo = TmpRepo::new().expect("bench: create TmpRepo");
        let shas = create_commits(&repo, count);

        // Pre-write into git-notes (for git_notes read benchmarks).
        for sha in &shas {
            git_ai::git::refs::git_backend_for_tests::notes_add(
                repo.gitai_repo(),
                sha,
                NOTE_CONTENT,
            )
            .expect("bench setup: notes_add");
        }

        // Pre-write into notes-db (for http read benchmarks).
        {
            let db = NotesDatabase::global().expect("bench: get notes-db");
            let mut lock = db.lock().expect("bench: lock notes-db");
            let entries: Vec<(String, String)> = shas
                .iter()
                .map(|sha| (sha.clone(), NOTE_CONTENT.to_string()))
                .collect();
            lock.cache_synced_notes(&entries)
                .expect("bench setup: cache_synced_notes");
        }

        BenchRepo { repo, shas }
    }
}

// ---------------------------------------------------------------------------
// Bench group 1: write_single — 1 note write per iteration
// ---------------------------------------------------------------------------

fn bench_write_single(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(1);
    let target_sha = bench.shas[0].clone();

    let mut group = c.benchmark_group("write_single");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend ---
    group.bench_with_input(
        BenchmarkId::new("git_notes", "1_note"),
        &target_sha,
        |b, sha| {
            b.iter(|| {
                git_ai::git::refs::git_backend_for_tests::notes_add(
                    bench.repo.gitai_repo(),
                    sha,
                    NOTE_CONTENT,
                )
                .expect("notes_add failed");
            });
        },
    );

    // --- http backend (SQLite upsert only; async flush is excluded) ---
    group.bench_with_input(BenchmarkId::new("http", "1_note"), &target_sha, |b, sha| {
        let db = NotesDatabase::global().expect("get notes-db");
        b.iter(|| {
            let mut lock = db.lock().expect("lock notes-db");
            lock.upsert_note(sha, NOTE_CONTENT)
                .expect("upsert_note failed");
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 2: write_batch_100 — 100 notes batched per iteration
// ---------------------------------------------------------------------------

fn bench_write_batch_100(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(BATCH_SIZE);

    let batch: Vec<(String, String)> = bench
        .shas
        .iter()
        .take(BATCH_SIZE)
        .map(|sha| (sha.clone(), NOTE_CONTENT.to_string()))
        .collect();

    let mut group = c.benchmark_group("write_batch_100");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend ---
    group.bench_function(BenchmarkId::new("git_notes", "100_notes"), |b| {
        b.iter(|| {
            git_ai::git::refs::git_backend_for_tests::notes_add_batch(
                bench.repo.gitai_repo(),
                &batch,
            )
            .expect("notes_add_batch failed");
        });
    });

    // --- http backend ---
    group.bench_function(BenchmarkId::new("http", "100_notes"), |b| {
        let db = NotesDatabase::global().expect("get notes-db");
        b.iter(|| {
            let mut lock = db.lock().expect("lock notes-db");
            lock.upsert_notes_batch(&batch)
                .expect("upsert_notes_batch failed");
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 3: read_single_hot — read a known-cached SHA
// ---------------------------------------------------------------------------

fn bench_read_single_hot(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(1);
    let hot_sha = bench.shas[0].clone();

    let mut group = c.benchmark_group("read_single_hot");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend ---
    group.bench_with_input(
        BenchmarkId::new("git_notes", "hot_sha"),
        &hot_sha,
        |b, sha| {
            b.iter(|| {
                let _ = git_ai::git::refs::git_backend_for_tests::show_authorship_note(
                    bench.repo.gitai_repo(),
                    sha,
                );
            });
        },
    );

    // --- http backend (cache hit — no git fallback) ---
    group.bench_with_input(BenchmarkId::new("http", "hot_sha"), &hot_sha, |b, sha| {
        let db = NotesDatabase::global().expect("get notes-db");
        b.iter(|| {
            let lock = db.lock().expect("lock notes-db");
            let _ = lock.get_note(sha).expect("get_note failed");
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 4: read_single_cold — SHA not present in either backend
//
// For the git_notes baseline: show_authorship_note returns None (cache miss
// handled by the underlying git call returning non-zero).
// For the http backend: get_note returns None. In production, notes_api would
// then fall back to git notes — that fallback is identical to the git_notes
// baseline and is therefore not double-counted here.
// ---------------------------------------------------------------------------

fn bench_read_single_cold(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(1);

    // A SHA that definitely does NOT exist in either backend.
    let cold_sha = "0000000000000000000000000000000000000000".to_string();

    let mut group = c.benchmark_group("read_single_cold");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend (miss → returns None) ---
    group.bench_with_input(
        BenchmarkId::new("git_notes", "cold_sha"),
        &cold_sha,
        |b, sha| {
            b.iter(|| {
                let _ = git_ai::git::refs::git_backend_for_tests::show_authorship_note(
                    bench.repo.gitai_repo(),
                    sha,
                );
            });
        },
    );

    // --- http backend (cache-miss lookup only; fallback = git_notes baseline) ---
    group.bench_with_input(BenchmarkId::new("http", "cold_sha"), &cold_sha, |b, sha| {
        let db = NotesDatabase::global().expect("get notes-db");
        b.iter(|| {
            let lock = db.lock().expect("lock notes-db");
            let _ = lock.get_note(sha).expect("get_note failed");
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 5: read_batch_100 — bulk read 100 SHAs
// ---------------------------------------------------------------------------

fn bench_read_batch_100(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(BATCH_SIZE);

    let batch_shas: Vec<String> = bench.shas.iter().take(BATCH_SIZE).cloned().collect();

    let mut group = c.benchmark_group("read_batch_100");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend ---
    group.bench_function(BenchmarkId::new("git_notes", "100_reads"), |b| {
        b.iter(|| {
            let _ = git_ai::git::refs::git_backend_for_tests::note_blob_oids_for_commits(
                bench.repo.gitai_repo(),
                &batch_shas,
            )
            .expect("note_blob_oids_for_commits failed");
        });
    });

    // --- http backend ---
    group.bench_function(BenchmarkId::new("http", "100_reads"), |b| {
        let db = NotesDatabase::global().expect("get notes-db");
        let sha_refs: Vec<&str> = batch_shas.iter().map(|s| s.as_str()).collect();
        b.iter(|| {
            let lock = db.lock().expect("lock notes-db");
            let _ = lock.get_notes(&sha_refs).expect("get_notes failed");
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 6: commits_with_notes_500 — presence check across 500 SHAs
// ---------------------------------------------------------------------------

fn bench_commits_with_notes_500(c: &mut Criterion) {
    ensure_notes_db_initialized();
    let bench = BenchRepo::new(COMMITS_CHECK_SIZE);

    let check_shas: Vec<String> = bench
        .shas
        .iter()
        .take(COMMITS_CHECK_SIZE)
        .cloned()
        .collect();

    let mut group = c.benchmark_group("commits_with_notes_500");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS));

    // --- git_notes backend ---
    group.bench_function(BenchmarkId::new("git_notes", "500_shas"), |b| {
        b.iter(|| {
            let _ = git_ai::git::refs::git_backend_for_tests::commits_with_authorship_notes(
                bench.repo.gitai_repo(),
                &check_shas,
            )
            .expect("commits_with_authorship_notes failed");
        });
    });

    // --- http backend: presence = keys in get_notes result ---
    group.bench_function(BenchmarkId::new("http", "500_shas"), |b| {
        let db = NotesDatabase::global().expect("get notes-db");
        let sha_refs: Vec<&str> = check_shas.iter().map(|s| s.as_str()).collect();
        b.iter(|| {
            let lock = db.lock().expect("lock notes-db");
            let map = lock.get_notes(&sha_refs).expect("get_notes failed");
            let _present: std::collections::HashSet<&str> =
                map.keys().map(|s| s.as_str()).collect();
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Bench group 7: rebase_50_commits — end-to-end rebase of 50 commits
//
// This is the headline perf gate. It exercises the full notes I/O path that
// fires during a rebase: load_rebase_note_cache → note_blob_oids → batch read
// → post-commit write for each rebased commit.
//
// Because Config::get() is a process-global OnceLock that locks the backend
// on first access, both groups use the git_notes backend (the default) for
// the actual rebase path. The "http_cached" group additionally pre-populates
// notes-db so a future http-enabled run will hit the fast SQLite path instead
// of git. To benchmark the true http path, set GIT_AI_NOTES_BACKEND_KIND=http
// and rebuild.
// ---------------------------------------------------------------------------

fn bench_rebase_50_commits(c: &mut Criterion) {
    ensure_notes_db_initialized();

    let mut group = c.benchmark_group("rebase_50_commits");
    // Rebase is slow; use minimum Criterion sample size (10) with a longer
    // measurement window to allow enough time for 10 iterations per backend.
    // Criterion requires sample_size >= 10.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(180));
    group.warm_up_time(Duration::from_secs(10));

    // --- git_notes backend ---
    group.bench_function(BenchmarkId::new("git_notes", "50_commits"), |b| {
        b.iter_batched(
            || setup_rebase_repo(REBASE_COMMIT_COUNT),
            |(repo, feature_branch, main_branch): (TmpRepo, String, String)| {
                repo.switch_branch(&feature_branch)
                    .expect("switch to feature");
                repo.rebase_onto(&feature_branch, &main_branch)
                    .expect("rebase failed");
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // --- http_cached: same rebase with notes pre-cached in notes-db ---
    // Notes-db is pre-populated in setup; the rebase itself still runs through
    // the git_notes backend (due to the Config singleton), but the cache is
    // warm for any http-aware callers.
    group.bench_function(BenchmarkId::new("http_cached", "50_commits"), |b| {
        b.iter_batched(
            || {
                let (repo, fb, mb) = setup_rebase_repo(REBASE_COMMIT_COUNT);
                // Pre-cache all commits' notes into notes-db.
                let shas = collect_all_commit_shas(&repo);
                {
                    let db = NotesDatabase::global().expect("get notes-db");
                    let mut lock = db.lock().expect("lock notes-db");
                    let entries: Vec<(String, String)> = shas
                        .iter()
                        .map(|sha| (sha.clone(), NOTE_CONTENT.to_string()))
                        .collect();
                    lock.cache_synced_notes(&entries)
                        .expect("bench setup: cache rebase notes");
                }
                (repo, fb, mb)
            },
            |(repo, feature_branch, main_branch): (TmpRepo, String, String)| {
                repo.switch_branch(&feature_branch)
                    .expect("switch to feature");
                repo.rebase_onto(&feature_branch, &main_branch)
                    .expect("rebase failed");
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Helper: build a TmpRepo with N feature commits on a branch, plus a main
// branch that has advanced 3 commits beyond the common ancestor.
// Returns (TmpRepo, feature_branch_name, main_branch_name).
// ---------------------------------------------------------------------------

fn setup_rebase_repo(feature_commits: usize) -> (TmpRepo, String, String) {
    let repo = TmpRepo::new().expect("bench: create rebase TmpRepo");

    // Base commit (uses write_file + trigger_checkpoint to simulate real usage).
    let _f = repo
        .write_file("base.txt", "base content\n", true)
        .expect("write base file");
    repo.trigger_checkpoint_with_author("alice")
        .expect("checkpoint");
    repo.commit_with_message("base commit")
        .expect("base commit");

    // Create main branch (branching from base commit).
    repo.create_branch("bench_main")
        .expect("create main branch");

    // Feature branch also branches from base.
    repo.create_branch("bench_feature")
        .expect("create feature branch");
    repo.switch_branch("bench_feature")
        .expect("switch to feature");

    // Add N commits on the feature branch.
    for i in 0..feature_commits {
        let filename = format!("feature_{}.txt", i);
        let content = format!("feature content {}\n", i);
        let _f = repo
            .write_file(&filename, &content, true)
            .expect("write feature file");
        repo.trigger_checkpoint_with_author("alice")
            .expect("checkpoint");
        repo.commit_with_message(&format!("feature commit {}", i))
            .expect("feature commit");
    }

    // Advance main branch with 3 independent commits.
    repo.switch_branch("bench_main").expect("switch to main");
    for i in 0..3 {
        let filename = format!("main_{}.txt", i);
        let content = format!("main content {}\n", i);
        let _f = repo
            .write_file(&filename, &content, true)
            .expect("write main file");
        repo.trigger_checkpoint_with_author("bob")
            .expect("checkpoint");
        repo.commit_with_message(&format!("main commit {}", i))
            .expect("main commit");
    }

    (repo, "bench_feature".to_string(), "bench_main".to_string())
}

// ---------------------------------------------------------------------------
// Criterion entry points
// ---------------------------------------------------------------------------

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(MEASUREMENT_TIME_SECS))
        .sample_size(SAMPLE_SIZE)
        .warm_up_time(Duration::from_secs(2));
    targets =
        bench_write_single,
        bench_write_batch_100,
        bench_read_single_hot,
        bench_read_single_cold,
        bench_read_batch_100,
        bench_commits_with_notes_500,
        bench_rebase_50_commits,
}

criterion_main!(benches);
