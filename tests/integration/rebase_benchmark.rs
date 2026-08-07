use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;
use std::time::Instant;

// Deterministic xorshift64 PRNG keeps benchmark workloads reproducible without rand.
struct DeterministicRng(u64);

impl DeterministicRng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn gen_range(&mut self, max: usize) -> usize {
        (self.next() as usize) % max.max(1)
    }
}

#[test]
fn deterministic_rng_sequence_and_ranges_are_stable() {
    let mut rng = DeterministicRng(42);
    assert_eq!(rng.next(), 45_454_805_674);
    assert_eq!(rng.next(), 11_532_217_803_599_905_471);
    assert_eq!(rng.next(), 10_021_416_941_527_320_954);
    assert_eq!(rng.gen_range(0), 0);
    assert_eq!(rng.gen_range(1), 0);
    assert_eq!(rng.gen_range(10), 7);
}

/// Benchmark: large rebase with many AI-authored commits
/// This simulates the real-world scenario reported by users in large monorepos
/// where rebases with AI authorship notes become extremely slow.
///
/// The test creates:
/// - A main branch that advances with N commits
/// - A feature branch with M commits, each touching AI-authored files
/// - Rebases the feature branch onto the advanced main branch
///
/// Run with: cargo test --package git-ai --test integration rebase_benchmark -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_rebase_many_ai_commits() {
    let num_feature_commits: usize = std::env::var("REBASE_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let num_main_commits: usize = std::env::var("REBASE_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let num_ai_files: usize = std::env::var("REBASE_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let lines_per_file: usize = std::env::var("REBASE_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    println!("\n=== Rebase Benchmark Configuration ===");
    println!("Feature commits: {}", num_feature_commits);
    println!("Main commits: {}", num_main_commits);
    println!("AI files per commit: {}", num_ai_files);
    println!("Lines per file: {}", lines_per_file);
    println!("=========================================\n");

    let repo = TestRepo::new();

    // Create initial commit on default branch
    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    // Create feature branch with many AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let setup_start = Instant::now();

    for commit_idx in 0..num_feature_commits {
        // Each commit touches several AI-authored files
        for file_idx in 0..num_ai_files {
            let filename = format!("feature/module_{}/file_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);

            // Build content with AI-authored lines that change each commit
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            for line_idx in 0..lines_per_file {
                let line_content = format!(
                    "// AI code v{} module {} line {}",
                    commit_idx, file_idx, line_idx
                );
                lines.push(line_content.ai());
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit(&format!("AI feature commit {}", commit_idx))
            .unwrap();

        if (commit_idx + 1) % 10 == 0 {
            println!(
                "  Created feature commit {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                setup_start.elapsed().as_secs_f64()
            );
        }
    }

    let feature_setup_time = setup_start.elapsed();
    println!(
        "Feature branch setup: {:.1}s ({} commits)",
        feature_setup_time.as_secs_f64(),
        num_feature_commits
    );

    // Advance main branch with non-conflicting commits
    repo.git(&["checkout", &default_branch]).unwrap();
    let main_setup_start = Instant::now();

    for commit_idx in 0..num_main_commits {
        let filename = format!("main/change_{}.txt", commit_idx);
        let mut file = repo.filename(&filename);
        file.set_contents(crate::lines![format!("main content {}", commit_idx)]);
        repo.stage_all_and_commit(&format!("Main commit {}", commit_idx))
            .unwrap();
    }

    let main_setup_time = main_setup_start.elapsed();
    println!(
        "Main branch setup: {:.1}s ({} commits)",
        main_setup_time.as_secs_f64(),
        num_main_commits
    );

    // Now perform the rebase and measure time
    repo.git(&["checkout", "feature"]).unwrap();

    println!("\n--- Starting rebase ---");
    let rebase_start = Instant::now();
    let result = repo.git(&["rebase", &default_branch]);
    let rebase_duration = rebase_start.elapsed();

    match &result {
        Ok(output) => {
            println!("Rebase succeeded in {:.3}s", rebase_duration.as_secs_f64());
            println!("Output: {}", output);
        }
        Err(e) => {
            println!(
                "Rebase failed in {:.3}s: {}",
                rebase_duration.as_secs_f64(),
                e
            );
        }
    }
    result.unwrap();

    println!("\n=== BENCHMARK RESULTS ===");
    println!(
        "Total rebase time: {:.3}s ({:.0}ms)",
        rebase_duration.as_secs_f64(),
        rebase_duration.as_millis()
    );
    println!(
        "Per-commit average: {:.1}ms",
        rebase_duration.as_millis() as f64 / num_feature_commits as f64
    );
    println!("=========================\n");
}

/// Smaller benchmark for quick iteration during optimization
#[test]
#[ignore]
fn benchmark_rebase_small() {
    let num_commits = 10;
    let num_ai_files = 3;
    let lines_per_file = 20;

    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    for commit_idx in 0..num_commits {
        for file_idx in 0..num_ai_files {
            let filename = format!("feat/mod_{}/f_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            for line_idx in 0..lines_per_file {
                lines.push(format!("// AI v{} m{} l{}", commit_idx, file_idx, line_idx).ai());
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit(&format!("feat {}", commit_idx))
            .unwrap();
    }

    repo.git(&["checkout", &default_branch]).unwrap();
    for i in 0..5 {
        let mut f = repo.filename(&format!("main_{}.txt", i));
        f.set_contents(crate::lines![format!("main {}", i)]);
        repo.stage_all_and_commit(&format!("main {}", i)).unwrap();
    }

    repo.git(&["checkout", "feature"]).unwrap();

    let start = Instant::now();
    repo.git(&["rebase", &default_branch]).unwrap();
    let dur = start.elapsed();

    println!("\n=== SMALL REBASE BENCHMARK ===");
    println!(
        "Commits: {}, AI files: {}, Lines/file: {}",
        num_commits, num_ai_files, lines_per_file
    );
    println!(
        "Total: {:.3}s ({:.0}ms)",
        dur.as_secs_f64(),
        dur.as_millis()
    );
    println!(
        "Per-commit: {:.1}ms",
        dur.as_millis() as f64 / num_commits as f64
    );
    println!("===============================\n");
}

/// Benchmark with performance JSON output for precise phase timing
#[test]
#[ignore]
fn benchmark_rebase_with_perf_json() {
    let num_commits: usize = std::env::var("REBASE_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let num_ai_files: usize = std::env::var("REBASE_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["base content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    for commit_idx in 0..num_commits {
        for file_idx in 0..num_ai_files {
            let filename = format!("feat/mod_{}/f_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            for line_idx in 0..30 {
                lines.push(
                    format!(
                        "// AI code v{} mod{} line{}",
                        commit_idx, file_idx, line_idx
                    )
                    .ai(),
                );
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit(&format!("feat {}", commit_idx))
            .unwrap();
    }

    repo.git(&["checkout", &default_branch]).unwrap();
    for i in 0..10 {
        let mut f = repo.filename(&format!("main_{}.txt", i));
        f.set_contents(crate::lines![format!("main {}", i)]);
        repo.stage_all_and_commit(&format!("main {}", i)).unwrap();
    }

    repo.git(&["checkout", "feature"]).unwrap();

    // Use benchmark_git to get performance JSON
    println!("\n--- Starting instrumented rebase ---");
    let start = Instant::now();
    let result = repo.benchmark_git(&["rebase", &default_branch]);
    let dur = start.elapsed();

    match result {
        Ok(bench) => {
            println!("\n=== INSTRUMENTED REBASE BENCHMARK ===");
            println!("Commits: {}, AI files: {}", num_commits, num_ai_files);
            println!("Total wall time: {:.3}s", dur.as_secs_f64());
            println!("Git duration: {:.3}s", bench.git_duration.as_secs_f64());
            println!(
                "Pre-command: {:.3}s",
                bench.pre_command_duration.as_secs_f64()
            );
            println!(
                "Post-command: {:.3}s",
                bench.post_command_duration.as_secs_f64()
            );
            println!(
                "Overhead: {:.3}s ({:.1}%)",
                (bench.total_duration - bench.git_duration).as_secs_f64(),
                ((bench.total_duration - bench.git_duration).as_millis() as f64
                    / bench.git_duration.as_millis().max(1) as f64)
                    * 100.0
            );
            println!("======================================\n");
        }
        Err(e) => {
            println!(
                "Benchmark result: {} (wall time: {:.3}s)",
                e,
                dur.as_secs_f64()
            );
            // Still useful even without structured perf data
        }
    }
}

/// Benchmark diff-based attribution transfer with large files and content changes.
/// This tests the scenario where rebasing changes file content (main branch modifies
/// AI-tracked files), forcing the diff-based path instead of the fast-path note remap.
///
/// Scale: 50 commits × 10 files × 200 lines = significant AI-authored content.
/// The diff-based path should complete the per-commit processing loop in <10ms total.
#[test]
#[ignore]
fn benchmark_rebase_diff_based_large() {
    let num_feature_commits: usize = std::env::var("REBASE_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let num_ai_files: usize = std::env::var("REBASE_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let lines_per_file: usize = std::env::var("REBASE_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    println!("\n=== Diff-Based Large Rebase Benchmark ===");
    println!("Feature commits: {}", num_feature_commits);
    println!("AI files: {}", num_ai_files);
    println!("Lines per file: {}", lines_per_file);
    println!("==========================================\n");

    let repo = TestRepo::new();

    // Create initial commit with shared files (both branches will modify)
    {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            lines.push(format!("// Header for module {}", file_idx).into());
            lines.push("// Main branch will add lines above this marker".into());
            for line_idx in 0..lines_per_file {
                lines.push(format!("// Initial AI code mod{} line{}", file_idx, line_idx).ai());
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit("Initial shared files").unwrap();
    }

    let default_branch = repo.current_branch();

    // Create feature branch with AI commits
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let setup_start = Instant::now();
    for commit_idx in 0..num_feature_commits {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = format!(
                "{}\n// AI addition v{} mod{}",
                current, commit_idx, file_idx
            );
            fs::write(&path, &new_content).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &filename]).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("AI feature {}", commit_idx))
            .unwrap();

        if (commit_idx + 1) % 10 == 0 {
            println!(
                "  Feature commit {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                setup_start.elapsed().as_secs_f64()
            );
        }
    }
    println!("Feature setup: {:.1}s", setup_start.elapsed().as_secs_f64());

    // Advance main branch with modifications to AI-tracked files (forces content changes on rebase)
    repo.git(&["checkout", &default_branch]).unwrap();
    for main_idx in 0..5 {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = current.replacen(
                "// Main branch will add lines above this marker",
                &format!(
                    "// Main addition {} for mod{}\n// Main branch will add lines above this marker",
                    main_idx, file_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("Main change {}", main_idx))
            .unwrap();
    }

    // Unrelated main commits
    for i in 0..10 {
        let filename = format!("main_only/change_{}.txt", i);
        let mut file = repo.filename(&filename);
        file.set_contents(crate::lines![format!("main only {}", i)]);
        repo.stage_all_and_commit(&format!("Main unrelated {}", i))
            .unwrap();
    }

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    let timing_file = repo.path().join("..").join("rebase_timing_diff.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    println!("\n--- Starting diff-based rebase ---");
    let rebase_start = Instant::now();
    let result = repo.git_with_env(
        &["rebase", &default_branch],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "1"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    );
    let rebase_duration = rebase_start.elapsed();

    match &result {
        Ok(_) => println!("Rebase succeeded in {:.3}s", rebase_duration.as_secs_f64()),
        Err(e) => println!(
            "Rebase FAILED in {:.3}s: {}",
            rebase_duration.as_secs_f64(),
            e
        ),
    }
    result.unwrap();

    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING BREAKDOWN ===");
        print!("{}", timing_data);
        println!("===============================");
    }

    println!("\n=== DIFF-BASED LARGE BENCHMARK RESULTS ===");
    println!(
        "Total rebase time: {:.3}s ({:.0}ms)",
        rebase_duration.as_secs_f64(),
        rebase_duration.as_millis()
    );
    println!(
        "Per-commit average: {:.1}ms",
        rebase_duration.as_millis() as f64 / num_feature_commits as f64
    );
    println!("============================================\n");
}

/// Benchmark comparing the notes-based fast path vs blame-based slow path.
/// Runs the same rebase twice: once with notes (fast) and once without (blame fallback).
///
/// Run with: cargo test --test integration benchmark_blame_vs_diff -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_blame_vs_diff() {
    let num_feature_commits: usize = std::env::var("REBASE_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let num_ai_files: usize = std::env::var("REBASE_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let lines_per_file: usize = std::env::var("REBASE_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    println!("\n=== Blame vs Diff-Based Benchmark ===");
    println!("Feature commits: {}", num_feature_commits);
    println!("AI files: {}", num_ai_files);
    println!("Lines per file: {}", lines_per_file);
    println!("======================================\n");

    // Helper closure to create a test repo with the same setup
    let create_repo = |strip_notes: bool| -> (std::time::Duration, String) {
        let repo = TestRepo::new();
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            lines.push(format!("// Header for module {}", file_idx).into());
            lines.push("// Main branch marker".into());
            for line_idx in 0..lines_per_file {
                lines.push(format!("// AI code mod{} line{}", file_idx, line_idx).ai());
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit("Initial shared files").unwrap();
        let default_branch = repo.current_branch();

        repo.git(&["checkout", "-b", "feature"]).unwrap();
        for commit_idx in 0..num_feature_commits {
            for file_idx in 0..num_ai_files {
                let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
                let path = repo.path().join(&filename);
                let current = fs::read_to_string(&path).unwrap_or_default();
                let new_content = format!(
                    "{}\n// AI addition v{} mod{}",
                    current, commit_idx, file_idx
                );
                fs::write(&path, &new_content).unwrap();
                repo.git_ai(&["checkpoint", "mock_ai", &filename]).unwrap();
            }
            repo.git(&["add", "-A"]).unwrap();
            repo.stage_all_and_commit(&format!("AI feature {}", commit_idx))
                .unwrap();
        }

        if strip_notes {
            // Delete the authorship notes ref to force the blame-based fallback
            let _ = repo.git(&["update-ref", "-d", "refs/notes/git-ai-authorship"]);
        }

        repo.git(&["checkout", &default_branch]).unwrap();
        for main_idx in 0..5 {
            for file_idx in 0..num_ai_files {
                let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
                let path = repo.path().join(&filename);
                let current = fs::read_to_string(&path).unwrap_or_default();
                let new_content = current.replacen(
                    "// Main branch marker",
                    &format!(
                        "// Main addition {} mod{}\n// Main branch marker",
                        main_idx, file_idx
                    ),
                    1,
                );
                fs::write(&path, &new_content).unwrap();
            }
            repo.git(&["add", "-A"]).unwrap();
            repo.stage_all_and_commit(&format!("Main {}", main_idx))
                .unwrap();
        }

        repo.git(&["checkout", "feature"]).unwrap();
        let timing_file = repo.path().join("..").join(if strip_notes {
            "timing_no_notes.txt"
        } else {
            "timing_with_notes.txt"
        });
        let timing_path = timing_file.to_str().unwrap().to_string();

        let rebase_start = Instant::now();
        repo.git_with_env(
            &["rebase", &default_branch],
            &[
                ("GIT_AI_DEBUG_PERFORMANCE", "1"),
                ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
            ],
            None,
        )
        .unwrap();
        let duration = rebase_start.elapsed();

        let timing_data = fs::read_to_string(&timing_file).unwrap_or_default();
        (duration, timing_data)
    };

    // Run with notes (diff-based fast path)
    let (with_notes_dur, with_notes_timing) = create_repo(false);
    println!("--- WITH NOTES (diff-based path) ---");
    print!("{}", with_notes_timing);
    println!("Total rebase: {:.0}ms\n", with_notes_dur.as_millis());

    // Run without notes (blame-based slow path)
    let (no_notes_dur, no_notes_timing) = create_repo(true);
    println!("--- WITHOUT NOTES (blame-based fallback) ---");
    print!("{}", no_notes_timing);
    println!("Total rebase: {:.0}ms\n", no_notes_dur.as_millis());

    let authorship_with =
        extract_timing(&with_notes_timing, "TOTAL").unwrap_or(with_notes_dur.as_millis() as u64);
    let authorship_without =
        extract_timing(&no_notes_timing, "TOTAL").unwrap_or(no_notes_dur.as_millis() as u64);

    if authorship_without > 0 {
        let speedup = authorship_without as f64 / authorship_with.max(1) as f64;
        println!("=== COMPARISON ===");
        println!("Authorship rewrite with notes:    {}ms", authorship_with);
        println!("Authorship rewrite without notes: {}ms", authorship_without);
        println!("Speedup:                          {:.1}x", speedup);
        println!("==================\n");
    }
}

/// HEAVY benchmark designed to stress-test rebase performance at scale.
///
/// This creates a realistic monorepo-style scenario:
/// - 50 AI-tracked files across multiple modules (200-500 lines each)
/// - 200 feature commits, EVERY commit touches ALL AI files (no skipping)
/// - Every single change has AI attribution (checkpoint for each file in each commit)
/// - Main branch also modifies the same AI-tracked files (forces slow path)
/// - 20 main branch commits creating content conflicts that shift line ranges
///
/// This ensures:
/// 1. No fast-path shortcuts (blob OIDs differ due to main branch changes)
/// 2. Every commit must have its attribution rewritten (100% AI content)
/// 3. Line attribution transfer must handle shifting ranges
/// 4. Large note payloads (50 files × many line ranges per commit)
///
/// Run with: cargo test --package git-ai --test integration benchmark_rebase_heavy -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_rebase_heavy() {
    let num_ai_files: usize = std::env::var("HEAVY_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let lines_per_file: usize = std::env::var("HEAVY_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let num_feature_commits: usize = std::env::var("HEAVY_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let num_main_commits: usize = std::env::var("HEAVY_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let files_per_commit: usize = std::env::var("HEAVY_BENCH_FILES_PER_COMMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(num_ai_files); // default: touch ALL files every commit

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║             HEAVY REBASE BENCHMARK                      ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!(
        "║  AI files:            {:<10}                        ║",
        num_ai_files
    );
    println!(
        "║  Lines per file:      {:<10}                        ║",
        lines_per_file
    );
    println!(
        "║  Feature commits:     {:<10}                        ║",
        num_feature_commits
    );
    println!(
        "║  Main commits:        {:<10}                        ║",
        num_main_commits
    );
    println!(
        "║  Files per commit:    {:<10}                        ║",
        files_per_commit
    );
    println!(
        "║  Total initial lines: {:<10}                        ║",
        num_ai_files * lines_per_file
    );
    println!("╚══════════════════════════════════════════════════════════╝\n");

    let repo = TestRepo::new();
    let setup_start = Instant::now();

    // Step 1: Create initial commit with all AI-tracked files
    {
        for file_idx in 0..num_ai_files {
            let module = file_idx % 10;
            let filename = format!("src/modules/mod_{}/component_{}.rs", module, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            // Header region (will be modified by main branch)
            lines.push(
                format!(
                    "// Module {} Component {} - Auto-generated",
                    module, file_idx
                )
                .into(),
            );
            lines.push("// MAIN_INSERTION_POINT".into());
            lines.push(format!("pub mod component_{} {{", file_idx).into());
            // AI-generated body
            for line_idx in 0..lines_per_file {
                let line = format!(
                    "    pub fn func_{}_{}() -> i32 {{ {} }} // AI generated",
                    file_idx,
                    line_idx,
                    line_idx * file_idx + 1
                );
                lines.push(line.ai());
            }
            lines.push("} // end module".into());
            file.set_contents(lines);
        }
        repo.stage_all_and_commit("Initial: all AI-tracked files")
            .unwrap();
    }
    println!(
        "Initial commit: {:.1}s",
        setup_start.elapsed().as_secs_f64()
    );

    let default_branch = repo.current_branch();

    // Step 2: Create feature branch with many AI commits
    // EVERY commit touches files and EVERY change has AI attribution
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_start = Instant::now();

    for commit_idx in 0..num_feature_commits {
        let start_file = (commit_idx * 3) % num_ai_files;
        for i in 0..files_per_commit {
            let file_idx = (start_file + i) % num_ai_files;
            let module = file_idx % 10;
            let filename = format!("src/modules/mod_{}/component_{}.rs", module, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();

            // Append AI-authored code at end (before closing brace)
            let new_content = current.replacen(
                "} // end module",
                &format!(
                    "    pub fn feature_{}_in_comp_{}() -> String {{ String::from(\"v{}\") }} // AI commit {}\n}} // end module",
                    commit_idx, file_idx, commit_idx, commit_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
            // Checkpoint EVERY file as AI-authored
            repo.git_ai(&["checkpoint", "mock_ai_agent", &filename])
                .unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("AI feature commit {}", commit_idx))
            .unwrap();

        if (commit_idx + 1) % 25 == 0 {
            println!(
                "  Feature commit {}/{} ({:.1}s, {:.0}ms/commit)",
                commit_idx + 1,
                num_feature_commits,
                feature_start.elapsed().as_secs_f64(),
                feature_start.elapsed().as_millis() as f64 / (commit_idx + 1) as f64,
            );
        }
    }
    println!(
        "Feature branch setup: {:.1}s ({} commits, {:.0}ms/commit)",
        feature_start.elapsed().as_secs_f64(),
        num_feature_commits,
        feature_start.elapsed().as_millis() as f64 / num_feature_commits as f64,
    );

    // Step 3: Advance main branch - modify the SAME AI-tracked files
    // This forces the slow path because blob OIDs will differ after rebase
    repo.git(&["checkout", &default_branch]).unwrap();
    let main_start = Instant::now();

    for main_idx in 0..num_main_commits {
        // Each main commit modifies a rotating set of AI files at the header
        let files_per_main = (num_ai_files / 2).max(5);
        let start_file = (main_idx * 7) % num_ai_files;
        for i in 0..files_per_main {
            let file_idx = (start_file + i) % num_ai_files;
            let module = file_idx % 10;
            let filename = format!("src/modules/mod_{}/component_{}.rs", module, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            // Insert at the MAIN_INSERTION_POINT - this shifts ALL line numbers
            let new_content = current.replacen(
                "// MAIN_INSERTION_POINT",
                &format!(
                    "// Main branch change {} in component {}\n// Added config: SETTING_{}={}\n// MAIN_INSERTION_POINT",
                    main_idx, file_idx, main_idx, file_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
        }
        // Also add unrelated files for realism
        for i in 0..3 {
            let filename = format!("docs/main_change_{}_{}.md", main_idx, i);
            let mut file = repo.filename(&filename);
            file.set_contents(crate::lines![format!("Main doc {} {}", main_idx, i)]);
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("Main change {}", main_idx))
            .unwrap();
    }
    println!(
        "Main branch setup: {:.1}s ({} commits)",
        main_start.elapsed().as_secs_f64(),
        num_main_commits,
    );
    println!(
        "Total setup time: {:.1}s",
        setup_start.elapsed().as_secs_f64()
    );

    // Step 4: Rebase feature onto main with full instrumentation
    repo.git(&["checkout", "feature"]).unwrap();

    let timing_file = repo.path().join("..").join("heavy_rebase_timing.txt");

    println!(
        "\n━━━ Starting HEAVY rebase ({} commits onto {}) ━━━",
        num_feature_commits, default_branch
    );
    let wall_start = Instant::now();

    // Use benchmark_git for structured timing (captures pre/git/post breakdown)
    let bench_result = repo.benchmark_git(&["rebase", &default_branch]);
    let wall_duration = wall_start.elapsed();

    match &bench_result {
        Ok(bench) => {
            let git_ms = bench.git_duration.as_millis();
            let total_ms = bench.total_duration.as_millis();
            let pre_ms = bench.pre_command_duration.as_millis();
            let post_ms = bench.post_command_duration.as_millis();
            let overhead_ms = total_ms.saturating_sub(git_ms);
            let overhead_pct = if git_ms > 0 {
                overhead_ms as f64 / git_ms as f64 * 100.0
            } else {
                0.0
            };

            println!("\n╔══════════════════════════════════════════════════════════╗");
            println!("║            HEAVY BENCHMARK RESULTS                      ║");
            println!("╠══════════════════════════════════════════════════════════╣");
            println!("║  Configuration:                                         ║");
            println!(
                "║    AI files:          {}                            ",
                num_ai_files
            );
            println!(
                "║    Lines/file:        {}                           ",
                lines_per_file
            );
            println!(
                "║    Feature commits:   {}                           ",
                num_feature_commits
            );
            println!(
                "║    Main commits:      {}                           ",
                num_main_commits
            );
            println!(
                "║    Files/commit:      {}                           ",
                files_per_commit
            );
            println!("╠══════════════════════════════════════════════════════════╣");
            println!("║  Timing:                                                ║");
            println!(
                "║    Wall time:         {:.3}s                       ",
                wall_duration.as_secs_f64()
            );
            println!(
                "║    Total (wrapper):   {}ms                        ",
                total_ms
            );
            println!(
                "║    Git rebase:        {}ms                        ",
                git_ms
            );
            println!(
                "║    Pre-command:       {}ms                        ",
                pre_ms
            );
            println!(
                "║    Post-command:      {}ms                        ",
                post_ms
            );
            println!(
                "║    Overhead:          {}ms ({:.1}% of git)        ",
                overhead_ms, overhead_pct
            );
            println!("╠══════════════════════════════════════════════════════════╣");
            println!("║  Per-commit averages:                                   ║");
            println!(
                "║    Total:             {:.1}ms                     ",
                total_ms as f64 / num_feature_commits as f64
            );
            println!(
                "║    Git:               {:.1}ms                     ",
                git_ms as f64 / num_feature_commits as f64
            );
            println!(
                "║    Overhead:          {:.1}ms                     ",
                overhead_ms as f64 / num_feature_commits as f64
            );
            println!("╚══════════════════════════════════════════════════════════╝\n");
        }
        Err(e) => {
            println!(
                "Benchmark failed after {:.3}s: {}",
                wall_duration.as_secs_f64(),
                e
            );
            panic!("Heavy benchmark failed: {}", e);
        }
    }

    // Also read timing file if available
    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("=== PHASE TIMING BREAKDOWN ===");
        print!("{}", timing_data);
        println!("===============================\n");
    }
}

/// Same as heavy benchmark but with timing file output for phase analysis
#[test]
#[ignore]
fn benchmark_rebase_heavy_with_timing() {
    let num_ai_files: usize = std::env::var("HEAVY_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let lines_per_file: usize = std::env::var("HEAVY_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let num_feature_commits: usize = std::env::var("HEAVY_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let num_main_commits: usize = std::env::var("HEAVY_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);

    println!("\n=== Heavy Rebase Benchmark (with timing) ===");
    println!(
        "AI files: {}, Lines/file: {}, Feature commits: {}, Main commits: {}",
        num_ai_files, lines_per_file, num_feature_commits, num_main_commits
    );
    println!("=============================================\n");

    let repo = TestRepo::new();

    // Create initial files
    for file_idx in 0..num_ai_files {
        let module = file_idx % 8;
        let filename = format!("src/mod_{}/file_{}.rs", module, file_idx);
        let mut file = repo.filename(&filename);
        let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
        lines.push(format!("// File {} header", file_idx).into());
        lines.push("// MAIN_MARKER".into());
        for line_idx in 0..lines_per_file {
            lines.push(format!("fn f_{}_{}() {{ /* AI */ }}", file_idx, line_idx).ai());
        }
        lines.push("// EOF".into());
        file.set_contents(lines);
    }
    repo.stage_all_and_commit("Initial AI files").unwrap();
    let default_branch = repo.current_branch();

    // Feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_start = Instant::now();
    for commit_idx in 0..num_feature_commits {
        for file_idx in 0..num_ai_files {
            let module = file_idx % 8;
            let filename = format!("src/mod_{}/file_{}.rs", module, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = current.replacen(
                "// EOF",
                &format!(
                    "fn feat_{}_{}() {{ /* AI v{} */ }}\n// EOF",
                    commit_idx, file_idx, commit_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &filename]).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("feat {}", commit_idx))
            .unwrap();
        if (commit_idx + 1) % 20 == 0 {
            println!(
                "  Feature {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                feature_start.elapsed().as_secs_f64()
            );
        }
    }
    println!(
        "Feature setup: {:.1}s",
        feature_start.elapsed().as_secs_f64()
    );

    // Main branch modifications
    repo.git(&["checkout", &default_branch]).unwrap();
    for main_idx in 0..num_main_commits {
        for file_idx in 0..num_ai_files {
            let module = file_idx % 8;
            let filename = format!("src/mod_{}/file_{}.rs", module, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = current.replacen(
                "// MAIN_MARKER",
                &format!("// main change {} f{}\n// MAIN_MARKER", main_idx, file_idx),
                1,
            );
            fs::write(&path, &new_content).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("main {}", main_idx))
            .unwrap();
    }

    // Rebase with timing
    repo.git(&["checkout", "feature"]).unwrap();
    let timing_file = repo.path().join("..").join("heavy_timing.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    println!("\n--- Starting rebase ---");
    let start = Instant::now();
    let result = repo.git_with_env(
        &["rebase", &default_branch],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "2"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    );
    let dur = start.elapsed();

    match &result {
        Ok(_) => println!("Rebase succeeded in {:.3}s", dur.as_secs_f64()),
        Err(e) => println!("Rebase FAILED in {:.3}s: {}", dur.as_secs_f64(), e),
    }
    result.unwrap();

    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING ===");
        print!("{}", timing_data);
        println!("====================\n");
    }

    println!(
        "Total: {:.3}s, Per-commit: {:.1}ms",
        dur.as_secs_f64(),
        dur.as_millis() as f64 / num_feature_commits as f64
    );
}

/// Realistic monorepo benchmark addressing all feedback from principal eng review:
/// 1. Non-uniform change patterns (1-20 files per commit, varying edit types)
/// 2. File heterogeneity (10 to 2000+ lines, different structures)
/// 3. Multi-author attribution (3 different AI models + human lines)
/// 4. Renames/moves (directory restructuring mid-feature)
/// 5. Non-trivial hunks (20-100 line AI edits, deletions, replacements)
/// 6. Clean rebase still (conflicts are hard to automate reproducibly)
#[test]
#[ignore]
fn benchmark_rebase_realistic_monorepo() {
    let num_feature_commits: usize = std::env::var("REALISTIC_BENCH_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let num_main_commits: usize = std::env::var("REALISTIC_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);

    println!("\n=== Realistic Monorepo Rebase Benchmark ===");
    println!(
        "Feature commits: {}, Main commits: {}",
        num_feature_commits, num_main_commits
    );
    println!("============================================\n");

    let repo = TestRepo::new();
    let mut rng = DeterministicRng(42); // deterministic for reproducibility

    // All checkpoints use "mock_ai" — the recognized test preset

    // --- Step 1: Create heterogeneous initial files ---
    // Mix of small configs, medium modules, and large files
    struct FileSpec {
        path: String,
        initial_lines: usize,
        has_ai: bool,
    }

    let mut files: Vec<FileSpec> = Vec::new();

    // Small config files (10-30 lines)
    for i in 0..5 {
        files.push(FileSpec {
            path: format!("config/settings_{}.toml", i),
            initial_lines: 10 + (i * 4),
            has_ai: i % 2 == 0, // some have AI, some don't
        });
    }

    // Medium source files (100-400 lines) — the bulk
    for i in 0..30 {
        let module = i % 6;
        files.push(FileSpec {
            path: format!("src/mod_{}/component_{}.rs", module, i),
            initial_lines: 100 + (i * 10),
            has_ai: true,
        });
    }

    // Large files (800-2000 lines)
    for i in 0..5 {
        files.push(FileSpec {
            path: format!("src/core/engine_{}.rs", i),
            initial_lines: 800 + (i * 300),
            has_ai: true,
        });
    }

    // Test files (200-500 lines)
    for i in 0..10 {
        files.push(FileSpec {
            path: format!("tests/test_{}.rs", i),
            initial_lines: 200 + (i * 30),
            has_ai: true,
        });
    }

    println!(
        "Creating {} files ({} with AI attribution)...",
        files.len(),
        files.iter().filter(|f| f.has_ai).count()
    );

    // Create initial files with mixed attribution
    for (idx, spec) in files.iter().enumerate() {
        let mut file = repo.filename(&spec.path);
        let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
        lines.push(format!("// File: {} (auto-generated)", spec.path).into());
        lines.push("// MAIN_INSERTION_POINT".into());

        for line_idx in 0..spec.initial_lines {
            let line_text = if line_idx % 20 == 0 {
                format!("// Section {} of {}", line_idx / 20, spec.path)
            } else if line_idx % 5 == 0 {
                format!("pub fn func_{}_{}() -> Result<(), Error> {{", idx, line_idx)
            } else if line_idx % 5 == 4 {
                "}".to_string()
            } else {
                format!("    let val_{} = compute_{}({});", line_idx, idx, line_idx)
            };

            if spec.has_ai && line_idx % 3 != 0 {
                // 2/3 of lines are AI-authored, 1/3 human — creates fragmented attribution
                lines.push(line_text.ai());
            } else {
                lines.push(line_text.into());
            }
        }
        lines.push("// FEATURE_INSERTION_POINT".into());
        lines.push("// EOF".into());
        file.set_contents(lines);
    }
    repo.stage_all_and_commit("Initial heterogeneous files")
        .unwrap();
    let default_branch = repo.current_branch();

    // --- Step 2: Feature branch with non-uniform commits ---
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_start = Instant::now();

    for commit_idx in 0..num_feature_commits {
        // Vary how many files each commit touches (1 to 20)
        let num_files_to_touch = if commit_idx % 10 == 0 {
            // Every 10th commit is a big refactor touching many files
            15 + rng.gen_range(10) // 15-24 files
        } else if commit_idx % 3 == 0 {
            // Some commits touch just 1-2 files (focused edits)
            1 + rng.gen_range(2)
        } else {
            // Normal commits touch 3-8 files
            3 + rng.gen_range(6)
        };

        let ai_files: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.has_ai)
            .map(|(i, _)| i)
            .collect();

        // Pick random subset of files to touch
        let mut touched: Vec<usize> = Vec::new();
        for _ in 0..num_files_to_touch.min(ai_files.len()) {
            let pick = ai_files[rng.gen_range(ai_files.len())];
            if !touched.contains(&pick) {
                touched.push(pick);
            }
        }

        // Vary edit patterns per file
        for &file_idx in &touched {
            let spec = &files[file_idx];
            let path = repo.path().join(&spec.path);
            let current = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let edit_type = rng.gen_range(5);

            let new_content = match edit_type {
                0 => {
                    // Large append (20-80 lines) — typical AI code generation
                    let num_new_lines = 20 + rng.gen_range(60);
                    let mut addition = String::new();
                    addition.push_str(&format!(
                        "\n// Feature {} addition ({} lines)\n",
                        commit_idx, num_new_lines
                    ));
                    addition.push_str(&format!(
                        "pub fn feature_{}_impl_{} () {{\n",
                        commit_idx, file_idx
                    ));
                    for j in 0..num_new_lines {
                        addition.push_str(&format!(
                            "    let step_{} = process_{}({});\n",
                            j, commit_idx, j
                        ));
                    }
                    addition.push_str("}\n// FEATURE_INSERTION_POINT");
                    current.replacen("// FEATURE_INSERTION_POINT", &addition, 1)
                }
                1 => {
                    // Replacement edit — delete some lines, add different ones
                    let lines: Vec<&str> = current.lines().collect();
                    if lines.len() > 30 {
                        let start = 10 + rng.gen_range(lines.len() / 3);
                        let del_count = 5 + rng.gen_range(15);
                        let end = (start + del_count).min(lines.len() - 5);
                        let add_count = 10 + rng.gen_range(30);
                        let mut result: Vec<String> =
                            lines[..start].iter().map(|s| s.to_string()).collect();
                        result.push(format!("// Refactored section (commit {})", commit_idx));
                        for j in 0..add_count {
                            result.push(format!(
                                "    let refactored_{} = new_impl_{}_{}();",
                                j, commit_idx, file_idx
                            ));
                        }
                        result.extend(lines[end..].iter().map(|s| s.to_string()));
                        result.join("\n")
                    } else {
                        // File too small, just append
                        current.replacen(
                            "// FEATURE_INSERTION_POINT",
                            &format!(
                                "fn small_edit_{}() {{ }}\n// FEATURE_INSERTION_POINT",
                                commit_idx
                            ),
                            1,
                        )
                    }
                }
                2 => {
                    // Multi-site edit — insert at multiple locations
                    let lines: Vec<&str> = current.lines().collect();
                    let mut result: Vec<String> = Vec::with_capacity(lines.len() + 20);
                    let insert_every = lines.len() / 4;
                    for (i, line) in lines.iter().enumerate() {
                        result.push(line.to_string());
                        if insert_every > 0 && i > 0 && i % insert_every == 0 && i < lines.len() - 5
                        {
                            for j in 0..5 {
                                result.push(format!(
                                    "    // Injected at site {} by commit {} (line {})",
                                    i, commit_idx, j
                                ));
                            }
                        }
                    }
                    result.join("\n")
                }
                3 => {
                    // Pure deletion (remove 5-20 lines from middle)
                    let lines: Vec<&str> = current.lines().collect();
                    if lines.len() > 40 {
                        let start = 15 + rng.gen_range(lines.len() / 3);
                        let del_count = 5 + rng.gen_range(15);
                        let end = (start + del_count).min(lines.len() - 5);
                        let mut result: Vec<String> =
                            lines[..start].iter().map(|s| s.to_string()).collect();
                        result.push(format!(
                            "// Deleted {} lines (commit {})",
                            end - start,
                            commit_idx
                        ));
                        result.extend(lines[end..].iter().map(|s| s.to_string()));
                        result.join("\n")
                    } else {
                        current.clone()
                    }
                }
                _ => {
                    // Small append (2-5 lines) — quick fix style
                    current.replacen(
                        "// FEATURE_INSERTION_POINT",
                        &format!(
                            "fn fix_{}_{}() {{ todo!() }}\nfn helper_{}_{}() {{ }}\n// FEATURE_INSERTION_POINT",
                            commit_idx, file_idx, commit_idx, file_idx
                        ),
                        1,
                    )
                }
            };

            fs::write(&path, &new_content).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &spec.path]).unwrap();
        }

        // Every 50th commit: rename a file (directory restructuring)
        if commit_idx > 0 && commit_idx % 50 == 0 && commit_idx / 50 < 3 {
            let rename_idx = commit_idx / 50; // 1, 2
            let old_path = format!("src/mod_{}/component_{}.rs", rename_idx, rename_idx * 5);
            let new_path = format!("src/refactored/component_{}_v2.rs", rename_idx * 5);
            let old_full = repo.path().join(&old_path);
            if old_full.exists() {
                let new_dir = repo.path().join("src/refactored");
                fs::create_dir_all(&new_dir).ok();
                fs::rename(&old_full, repo.path().join(&new_path)).ok();
            }
        }

        repo.git(&["add", "-A"]).unwrap();
        let msg = if commit_idx % 10 == 0 {
            format!("refactor: large restructuring pass {}", commit_idx)
        } else if commit_idx % 3 == 0 {
            format!("fix: targeted bug fix {}", commit_idx)
        } else {
            format!("feat: implement feature component {}", commit_idx)
        };
        repo.stage_all_and_commit(&msg).unwrap();

        if (commit_idx + 1) % 30 == 0 {
            println!(
                "  Feature {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                feature_start.elapsed().as_secs_f64()
            );
        }
    }
    println!(
        "Feature setup: {:.1}s ({} commits)",
        feature_start.elapsed().as_secs_f64(),
        num_feature_commits
    );

    // --- Step 3: Main branch advances ---
    // Touch ALL AI files on every main commit to guarantee blob OID differences
    // after rebase, forcing the slow path (full attribution rewrite).
    // Insert at MAIN_INSERTION_POINT (near top), separate from feature changes
    // at FEATURE_INSERTION_POINT (near bottom) to avoid merge conflicts.
    repo.git(&["checkout", &default_branch]).unwrap();
    for main_idx in 0..num_main_commits {
        for (file_idx, spec) in files.iter().enumerate() {
            if !spec.has_ai {
                continue;
            }
            let path = repo.path().join(&spec.path);
            if let Ok(current) = fs::read_to_string(&path) {
                let new_content = current.replacen(
                    "// MAIN_INSERTION_POINT",
                    &format!(
                        "// main-infra-v{}: config for file {}\nconst MAIN_CFG_{}_{}: u32 = {};\n// MAIN_INSERTION_POINT",
                        main_idx, file_idx, main_idx, file_idx, main_idx * 100 + file_idx
                    ),
                    1,
                );
                fs::write(&path, &new_content).unwrap();
            }
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("infra: main branch update {}", main_idx))
            .unwrap();
    }

    // --- Step 4: Rebase with timing ---
    repo.git(&["checkout", "feature"]).unwrap();
    let timing_file = std::path::PathBuf::from("/tmp/realistic_timing.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    // Check notes BEFORE rebase
    let pre_notes = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let pre_count = pre_notes.lines().filter(|l| !l.is_empty()).count();
    println!("AI notes BEFORE rebase: {}", pre_count);
    // Show sample notes to verify content (first, middle, last)
    let note_lines: Vec<&str> = pre_notes.lines().filter(|l| !l.is_empty()).collect();
    for &idx in &[0, note_lines.len() / 2, note_lines.len().saturating_sub(1)] {
        if let Some(line) = note_lines.get(idx) {
            let commit_sha = line.split_whitespace().nth(1).unwrap_or("");
            if !commit_sha.is_empty() {
                let note_content = repo
                    .git(&["notes", "--ref=refs/notes/ai", "show", commit_sha])
                    .unwrap_or_else(|e| format!("ERROR: {}", e));
                let preview_len = 300.min(note_content.len());
                println!(
                    "Note[{}] for {}:\n{}\n",
                    idx,
                    &commit_sha[..8],
                    &note_content[..preview_len]
                );
            }
        }
    }

    println!(
        "\n--- Starting realistic rebase ({} commits onto {}) ---",
        num_feature_commits, default_branch
    );
    let start = Instant::now();
    let result = repo.git_with_env(
        &["rebase", &default_branch],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "2"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    );
    let dur = start.elapsed();

    match &result {
        Ok(_) => println!("Rebase succeeded in {:.3}s", dur.as_secs_f64()),
        Err(e) => println!("Rebase FAILED in {:.3}s: {}", dur.as_secs_f64(), e),
    }
    result.unwrap();

    // Check notes state and rebase details
    let notes_list = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let notes_count = notes_list.lines().filter(|l| !l.is_empty()).count();
    println!("AI notes after rebase: {}", notes_count);
    // Show first few notes to verify they exist
    for line in notes_list.lines().take(3) {
        println!("  note: {}", line.trim());
    }

    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING ===");
        print!("{}", timing_data);
        println!("====================\n");
    } else {
        println!("(No timing file written — fast path or no notes to rewrite)");
    }

    println!(
        "Total: {:.3}s, Per-commit: {:.1}ms",
        dur.as_secs_f64(),
        dur.as_millis() as f64 / num_feature_commits as f64
    );
}

/// Realistic large monorepo benchmark modeling the key use case:
/// A developer rebases a small feature branch onto a busy main branch.
///
/// Key characteristics that differ from prior benchmarks:
/// - **Large repo tree**: 2000+ files across deep directory structure (makes tree diffs realistic)
/// - **Small feature branch**: 10 commits, each touching 2-5 AI-tracked files
/// - **Busy main branch**: 100 commits since feature diverged (touching various areas)
/// - **Sparse AI overlap**: Only 8 AI-tracked files, main mostly touches OTHER areas
/// - **Main occasionally touches same files**: Forces diff-based path (not fast-path note remap)
///
/// This models: "I've been working on a feature for a few days, main has moved forward
/// significantly, and I need to rebase before merging."
///
/// ## Repo caching
///
/// Setup takes ~15-20 minutes. To reuse a cached repo across runs:
///
/// ```sh
/// # First run: creates and saves the repo
/// MONO_BENCH_CACHE_DIR=/tmp/monorepo-bench-cache cargo test ... benchmark_monorepo_rebase ...
///
/// # Subsequent runs: restores from cache (~2s instead of ~15min)
/// MONO_BENCH_CACHE_DIR=/tmp/monorepo-bench-cache cargo test ... benchmark_monorepo_rebase ...
/// ```
///
/// Run with: cargo test --test integration benchmark_monorepo_rebase -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_monorepo_rebase() {
    let num_background_files: usize = std::env::var("MONO_BENCH_BG_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let num_ai_files: usize = std::env::var("MONO_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let lines_per_ai_file: usize = std::env::var("MONO_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let num_feature_commits: usize = std::env::var("MONO_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let num_main_commits: usize = std::env::var("MONO_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let cache_dir = std::env::var("MONO_BENCH_CACHE_DIR").ok();

    println!("\n=== Monorepo Rebase Benchmark ===");
    println!(
        "Background files:  {} (repo tree size)",
        num_background_files
    );
    println!("AI-tracked files:  {}", num_ai_files);
    println!("Lines per AI file: {}", lines_per_ai_file);
    println!("Feature commits:   {}", num_feature_commits);
    println!("Main commits:      {}", num_main_commits);
    if let Some(ref cd) = cache_dir {
        println!("Cache dir:         {}", cd);
    }
    println!("=================================\n");

    let repo = TestRepo::new();

    // --- Try to restore from cache ---
    let restored_from_cache = if let Some(ref cd) = cache_dir {
        let cache_path = std::path::Path::new(cd);
        if cache_path.join(".git").exists() {
            println!("Restoring repo from cache: {}", cd);
            let restore_start = Instant::now();
            // Copy cached working tree + .git into the test repo
            let status = std::process::Command::new("rsync")
                .args([
                    "-a",
                    "--delete",
                    &format!("{}/", cd),
                    &format!("{}/", repo.path().display()),
                ])
                .status()
                .expect("rsync failed");
            assert!(status.success(), "rsync restore from cache failed");
            println!(
                "Restored from cache in {:.1}s",
                restore_start.elapsed().as_secs_f64()
            );
            true
        } else {
            println!(
                "Cache dir not found, will create fresh setup and save to: {}",
                cd
            );
            false
        }
    } else {
        false
    };

    let mut rng = DeterministicRng(12345);
    let setup_start = Instant::now();

    // AI file paths used throughout setup and rebase
    let ai_file_paths: Vec<String> = (0..num_ai_files)
        .map(|i| format!("services/payments/src/handlers/payment_handler_{}.rs", i))
        .collect();

    if !restored_from_cache {
        // --- Step 1: Create a large repo with deep directory structure ---
        // All files are 100% AI-authored (worst case for rebase logic).
        // Write all files directly to disk, then do ONE bulk checkpoint.
        let dir_prefixes = [
            "services/auth/src",
            "services/billing/src",
            "services/notifications/src",
            "services/search/src",
            "services/analytics/src",
            "libs/common/src",
            "libs/database/src",
            "libs/cache/src",
            "libs/logging/src",
            "tools/cli/src",
            "tools/admin/src",
            "docs/api",
            "docs/internal",
            "config/deploy",
            "config/monitoring",
            "tests/e2e",
            "tests/integration",
            "tests/unit",
            "scripts/ci",
            "scripts/migration",
        ];

        // Create background files directly on disk (no per-file checkpoint)
        for file_idx in 0..num_background_files {
            let dir = dir_prefixes[file_idx % dir_prefixes.len()];
            let subdir = file_idx / dir_prefixes.len();
            let filename = format!("{}/mod_{}/file_{}.rs", dir, subdir % 10, file_idx);
            let file_path = repo.path().join(&filename);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(
                &file_path,
                format!(
                    "// Background file {}\npub fn bg_func_{}() {{}}\n",
                    file_idx, file_idx
                ),
            )
            .unwrap();
        }

        // Create AI-tracked files directly on disk
        for (file_idx, ai_path) in ai_file_paths.iter().enumerate() {
            let file_path = repo.path().join(ai_path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut content = String::new();
            content.push_str(&format!("// Payment handler {}\n", file_idx));
            content.push_str("// MAIN_INSERTION_POINT\n");
            content.push_str(&format!("pub mod payment_handler_{} {{\n", file_idx));
            for line_idx in 0..lines_per_ai_file {
                content.push_str(&format!(
                    "    pub fn process_{}() -> Result<(), PaymentError> {{ Ok(()) }}\n",
                    line_idx
                ));
            }
            content.push_str("    // FEATURE_INSERTION_POINT\n");
            content.push_str("}\n");
            fs::write(&file_path, &content).unwrap();
        }

        // Stage everything and do ONE bulk AI checkpoint for all files
        repo.git(&["add", "-A"]).unwrap();
        let checkpoint_start = Instant::now();
        repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
        println!(
            "Bulk AI checkpoint ({} files): {:.1}s",
            num_background_files + num_ai_files,
            checkpoint_start.elapsed().as_secs_f64()
        );

        repo.stage_all_and_commit("Initial monorepo setup").unwrap();
        let initial_time = setup_start.elapsed();
        println!(
            "Initial commit ({} files): {:.1}s",
            num_background_files + num_ai_files,
            initial_time.as_secs_f64()
        );

        let default_branch = repo.current_branch();

        // --- Step 2: Feature branch (small, focused, 100% AI) ---
        // Each commit checkpoints per-file (realistic: one prompt per file edit)
        repo.git(&["checkout", "-b", "feature/payments-refactor"])
            .unwrap();
        let feature_start = Instant::now();

        for commit_idx in 0..num_feature_commits {
            // Each feature commit touches 2-5 of the AI files (not all of them)
            let files_this_commit = 2 + rng.gen_range(4); // 2-5 files
            let start = rng.gen_range(num_ai_files);

            for i in 0..files_this_commit.min(num_ai_files) {
                let file_idx = (start + i) % num_ai_files;
                let path = repo.path().join(&ai_file_paths[file_idx]);
                let current = fs::read_to_string(&path).unwrap_or_default();

                // Append AI-authored code at the feature insertion point
                let num_new_lines = 5 + rng.gen_range(20); // 5-24 lines per file
                let mut addition = format!(
                    "    pub fn feature_{}_handler_{}() -> Result<(), PaymentError> {{\n",
                    commit_idx, file_idx
                );
                for j in 0..num_new_lines {
                    addition.push_str(&format!(
                        "        let step_{} = validate_payment({}, {});\n",
                        j, commit_idx, j
                    ));
                }
                addition.push_str("        Ok(())\n    }\n    // FEATURE_INSERTION_POINT");

                let new_content = current.replacen("    // FEATURE_INSERTION_POINT", &addition, 1);
                fs::write(&path, &new_content).unwrap();
                // Per-file checkpoint (realistic: one AI prompt per file)
                repo.git_ai(&["checkpoint", "mock_ai", &ai_file_paths[file_idx]])
                    .unwrap();
            }

            repo.git(&["add", "-A"]).unwrap();
            repo.stage_all_and_commit(&format!(
                "feat(payments): implement step {} of refactor",
                commit_idx
            ))
            .unwrap();
        }
        println!(
            "Feature branch: {:.1}s ({} commits)",
            feature_start.elapsed().as_secs_f64(),
            num_feature_commits
        );

        // --- Step 3: Busy main branch (all 100% AI-authored — worst case) ---
        // Models: many other developers merging AI-written code into main
        repo.git(&["checkout", &default_branch]).unwrap();
        let main_start = Instant::now();

        for main_idx in 0..num_main_commits {
            // Most main commits touch OTHER areas (not payments)
            let touch_ai_files = main_idx % 4 == 0; // ~25% of main commits touch AI files

            // Touch 3-10 background files per commit (simulates other developers' work)
            let bg_files_touched = 3 + rng.gen_range(8);
            let bg_start = rng.gen_range(num_background_files);
            for i in 0..bg_files_touched {
                let file_idx = (bg_start + i) % num_background_files;
                let dir = dir_prefixes[file_idx % dir_prefixes.len()];
                let subdir = file_idx / dir_prefixes.len();
                let filename = format!("{}/mod_{}/file_{}.rs", dir, subdir % 10, file_idx);
                let path = repo.path().join(&filename);
                if let Ok(current) = fs::read_to_string(&path) {
                    let new_content = format!(
                        "{}\npub fn main_change_{}_{}() {{}}",
                        current, main_idx, file_idx
                    );
                    fs::write(&path, &new_content).unwrap();
                }
            }

            // Occasionally touch AI-tracked files (forces slow path on rebase)
            if touch_ai_files {
                for ai_path in &ai_file_paths {
                    let path = repo.path().join(ai_path);
                    if let Ok(current) = fs::read_to_string(&path) {
                        let new_content = current.replacen(
                        "// MAIN_INSERTION_POINT",
                        &format!(
                            "// infra: config update v{}\nconst PAYMENT_CFG_{}: u32 = {};\n// MAIN_INSERTION_POINT",
                            main_idx, main_idx, main_idx * 42
                        ),
                        1,
                    );
                        fs::write(&path, &new_content).unwrap();
                    }
                }
            }

            // Stage and checkpoint as AI (all changes are AI-authored)
            repo.git(&["add", "-A"]).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
            repo.stage_all_and_commit(&format!("main: update {} from other team", main_idx))
                .unwrap();

            if (main_idx + 1) % 25 == 0 {
                println!(
                    "  Main commit {}/{} ({:.1}s)",
                    main_idx + 1,
                    num_main_commits,
                    main_start.elapsed().as_secs_f64()
                );
            }
        }
        println!(
            "Main branch: {:.1}s ({} commits)",
            main_start.elapsed().as_secs_f64(),
            num_main_commits
        );
        println!("Total setup: {:.1}s", setup_start.elapsed().as_secs_f64());

        // Save to cache if requested
        if let Some(ref cd) = cache_dir {
            let cache_path = std::path::Path::new(cd);
            if !cache_path.join(".git").exists() {
                println!("Saving repo to cache: {}", cd);
                let save_start = Instant::now();
                fs::create_dir_all(cache_path).expect("create cache dir");
                let status = std::process::Command::new("rsync")
                    .args([
                        "-a",
                        "--delete",
                        &format!("{}/", repo.path().display()),
                        &format!("{}/", cd),
                    ])
                    .status()
                    .expect("rsync failed");
                assert!(status.success(), "rsync save to cache failed");
                println!(
                    "Saved to cache in {:.1}s",
                    save_start.elapsed().as_secs_f64()
                );
            }
        }
    } // end if !restored_from_cache

    // Detect default branch (works whether fresh setup or cache restore — we're on main either way)
    let default_branch = repo.current_branch();

    // --- Step 4: Rebase with full timing instrumentation ---
    repo.git(&["checkout", "feature/payments-refactor"])
        .unwrap();
    let timing_file = std::path::PathBuf::from("/tmp/monorepo_rebase_timing.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    // Count notes before rebase
    let pre_notes = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let pre_count = pre_notes.lines().filter(|l| !l.is_empty()).count();
    println!("\nAI notes before rebase: {}", pre_count);

    println!(
        "\n--- Starting monorepo rebase ({} feature commits onto {} main commits) ---",
        num_feature_commits, num_main_commits
    );
    let rebase_start = Instant::now();
    let result = repo.git_with_env(
        &["rebase", &default_branch],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "2"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    );
    let rebase_dur = rebase_start.elapsed();

    match &result {
        Ok(_) => println!("Rebase succeeded in {:.3}s", rebase_dur.as_secs_f64()),
        Err(e) => println!("Rebase FAILED in {:.3}s: {}", rebase_dur.as_secs_f64(), e),
    }
    result.unwrap();

    // Post-rebase note check
    let post_notes = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let post_count = post_notes.lines().filter(|l| !l.is_empty()).count();
    println!("AI notes after rebase:  {}", post_count);

    // Phase timing breakdown
    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING ===");
        print!("{}", timing_data);
        println!("====================\n");
    } else {
        println!("(No timing file — possibly fast-path or no notes to rewrite)");
    }

    println!("=== MONOREPO REBASE RESULTS ===");
    println!(
        "Repo: {} files, {} AI-tracked",
        num_background_files + num_ai_files,
        num_ai_files
    );
    println!(
        "Rebase: {} feature commits onto {} main commits",
        num_feature_commits, num_main_commits
    );
    println!(
        "Total: {:.3}s ({:.0}ms)",
        rebase_dur.as_secs_f64(),
        rebase_dur.as_millis()
    );
    println!(
        "Per feature commit: {:.1}ms",
        rebase_dur.as_millis() as f64 / num_feature_commits as f64
    );
    println!("================================\n");
}

/// Same monorepo scenario as `benchmark_monorepo_rebase`, but uses Graphite-style
/// plumbing commands (`git commit-tree` + `git update-ref`) instead of `git rebase`.
///
/// Each feature commit is replayed one at a time via:
///   1. `git merge-tree` to compute the rebased tree
///   2. `git commit-tree` to create the new commit object
///   3. `git update-ref` to advance the branch (triggers git-ai wrapper detection)
///
/// This models the actual Graphite CLI restack flow and tests whether git-ai's
/// wrapper-based plumbing detection is as fast as the standard post-rewrite hook path.
///
/// Run with: cargo test --test integration benchmark_monorepo_graphite_rebase -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_monorepo_graphite_rebase() {
    let num_background_files: usize = std::env::var("MONO_BENCH_BG_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let num_ai_files: usize = std::env::var("MONO_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let lines_per_ai_file: usize = std::env::var("MONO_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let num_feature_commits: usize = std::env::var("MONO_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let num_main_commits: usize = std::env::var("MONO_BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    // Use a separate cache dir so it doesn't conflict with the standard rebase cache
    let cache_dir = std::env::var("MONO_BENCH_CACHE_DIR")
        .ok()
        .map(|d| format!("{}-graphite", d));

    println!("\n=== Monorepo GRAPHITE-STYLE Rebase Benchmark ===");
    println!(
        "Background files:  {} (repo tree size)",
        num_background_files
    );
    println!("AI-tracked files:  {}", num_ai_files);
    println!("Lines per AI file: {}", lines_per_ai_file);
    println!("Feature commits:   {}", num_feature_commits);
    println!("Main commits:      {}", num_main_commits);
    if let Some(ref cd) = cache_dir {
        println!("Cache dir:         {}", cd);
    }
    println!("=================================================\n");

    let repo = TestRepo::new();

    // --- Try to restore from cache ---
    let restored_from_cache = if let Some(ref cd) = cache_dir {
        let cache_path = std::path::Path::new(cd);
        if cache_path.join(".git").exists() {
            println!("Restoring repo from cache: {}", cd);
            let restore_start = Instant::now();
            let status = std::process::Command::new("rsync")
                .args([
                    "-a",
                    "--delete",
                    &format!("{}/", cd),
                    &format!("{}/", repo.path().display()),
                ])
                .status()
                .expect("rsync failed");
            assert!(status.success(), "rsync restore from cache failed");
            println!(
                "Restored from cache in {:.1}s",
                restore_start.elapsed().as_secs_f64()
            );
            true
        } else {
            println!(
                "Cache dir not found, will create fresh setup and save to: {}",
                cd
            );
            false
        }
    } else {
        false
    };

    let mut rng = DeterministicRng(12345); // Same seed as standard benchmark for identical repo

    let ai_file_paths: Vec<String> = (0..num_ai_files)
        .map(|i| format!("services/payments/src/handlers/payment_handler_{}.rs", i))
        .collect();

    if !restored_from_cache {
        let setup_start = Instant::now();

        // --- Identical setup to benchmark_monorepo_rebase ---
        let dir_prefixes = [
            "services/auth/src",
            "services/billing/src",
            "services/notifications/src",
            "services/search/src",
            "services/analytics/src",
            "libs/common/src",
            "libs/database/src",
            "libs/cache/src",
            "libs/logging/src",
            "tools/cli/src",
            "tools/admin/src",
            "docs/api",
            "docs/internal",
            "config/deploy",
            "config/monitoring",
            "tests/e2e",
            "tests/integration",
            "tests/unit",
            "scripts/ci",
            "scripts/migration",
        ];

        for file_idx in 0..num_background_files {
            let dir = dir_prefixes[file_idx % dir_prefixes.len()];
            let subdir = file_idx / dir_prefixes.len();
            let filename = format!("{}/mod_{}/file_{}.rs", dir, subdir % 10, file_idx);
            let file_path = repo.path().join(&filename);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(
                &file_path,
                format!(
                    "// Background file {}\npub fn bg_func_{}() {{}}\n",
                    file_idx, file_idx
                ),
            )
            .unwrap();
        }

        for (file_idx, ai_path) in ai_file_paths.iter().enumerate() {
            let file_path = repo.path().join(ai_path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut content = String::new();
            content.push_str(&format!("// Payment handler {}\n", file_idx));
            content.push_str("// MAIN_INSERTION_POINT\n");
            content.push_str(&format!("pub mod payment_handler_{} {{\n", file_idx));
            for line_idx in 0..lines_per_ai_file {
                content.push_str(&format!(
                    "    pub fn process_{}() -> Result<(), PaymentError> {{ Ok(()) }}\n",
                    line_idx
                ));
            }
            content.push_str("    // FEATURE_INSERTION_POINT\n");
            content.push_str("}\n");
            fs::write(&file_path, &content).unwrap();
        }

        repo.git(&["add", "-A"]).unwrap();
        repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
        repo.stage_all_and_commit("Initial monorepo setup").unwrap();

        let default_branch = repo.current_branch();

        // Feature branch
        repo.git(&["checkout", "-b", "feature/payments-refactor"])
            .unwrap();

        for commit_idx in 0..num_feature_commits {
            let files_this_commit = 2 + rng.gen_range(4);
            let start = rng.gen_range(num_ai_files);
            for i in 0..files_this_commit.min(num_ai_files) {
                let file_idx = (start + i) % num_ai_files;
                let path = repo.path().join(&ai_file_paths[file_idx]);
                let current = fs::read_to_string(&path).unwrap_or_default();
                let num_new_lines = 5 + rng.gen_range(20);
                let mut addition = format!(
                    "    pub fn feature_{}_handler_{}() -> Result<(), PaymentError> {{\n",
                    commit_idx, file_idx
                );
                for j in 0..num_new_lines {
                    addition.push_str(&format!(
                        "        let step_{} = validate_payment({}, {});\n",
                        j, commit_idx, j
                    ));
                }
                addition.push_str("        Ok(())\n    }\n    // FEATURE_INSERTION_POINT");
                let new_content = current.replacen("    // FEATURE_INSERTION_POINT", &addition, 1);
                fs::write(&path, &new_content).unwrap();
                repo.git_ai(&["checkpoint", "mock_ai", &ai_file_paths[file_idx]])
                    .unwrap();
            }
            repo.git(&["add", "-A"]).unwrap();
            repo.stage_all_and_commit(&format!(
                "feat(payments): implement step {} of refactor",
                commit_idx
            ))
            .unwrap();
        }

        // Busy main branch
        repo.git(&["checkout", &default_branch]).unwrap();
        for main_idx in 0..num_main_commits {
            let touch_ai_files = main_idx % 4 == 0;
            let bg_files_touched = 3 + rng.gen_range(8);
            let bg_start = rng.gen_range(num_background_files);
            for i in 0..bg_files_touched {
                let file_idx = (bg_start + i) % num_background_files;
                let dir = dir_prefixes[file_idx % dir_prefixes.len()];
                let subdir = file_idx / dir_prefixes.len();
                let filename = format!("{}/mod_{}/file_{}.rs", dir, subdir % 10, file_idx);
                let path = repo.path().join(&filename);
                if let Ok(current) = fs::read_to_string(&path) {
                    fs::write(
                        &path,
                        format!(
                            "{}\npub fn main_change_{}_{}() {{}}",
                            current, main_idx, file_idx
                        ),
                    )
                    .unwrap();
                }
            }
            if touch_ai_files {
                for ai_path in &ai_file_paths {
                    let path = repo.path().join(ai_path);
                    if let Ok(current) = fs::read_to_string(&path) {
                        let new_content = current.replacen(
                            "// MAIN_INSERTION_POINT",
                            &format!(
                                "// infra: config update v{}\nconst PAYMENT_CFG_{}: u32 = {};\n// MAIN_INSERTION_POINT",
                                main_idx, main_idx, main_idx * 42
                            ),
                            1,
                        );
                        fs::write(&path, &new_content).unwrap();
                    }
                }
            }
            repo.git(&["add", "-A"]).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();
            repo.stage_all_and_commit(&format!("main: update {} from other team", main_idx))
                .unwrap();

            if (main_idx + 1) % 100 == 0 {
                println!("  Main commit {}/{}", main_idx + 1, num_main_commits);
            }
        }

        // Save to cache
        if let Some(ref cd) = cache_dir {
            let cache_path = std::path::Path::new(cd);
            if !cache_path.join(".git").exists() {
                println!("Saving repo to cache: {}", cd);
                fs::create_dir_all(cache_path).expect("create cache dir");
                let status = std::process::Command::new("rsync")
                    .args([
                        "-a",
                        "--delete",
                        &format!("{}/", repo.path().display()),
                        &format!("{}/", cd),
                    ])
                    .status()
                    .expect("rsync failed");
                assert!(status.success(), "rsync save to cache failed");
            }
        }

        println!("Total setup: {:.1}s", setup_start.elapsed().as_secs_f64());
    } // end if !restored_from_cache

    let default_branch = repo.current_branch();

    // --- Step 4: Graphite-style rebase using plumbing commands ---
    // Collect feature branch commits (oldest to newest)
    repo.git(&["checkout", "feature/payments-refactor"])
        .unwrap();
    let feature_commits_str = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .unwrap();
    let feature_commits: Vec<&str> = feature_commits_str
        .trim()
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    println!(
        "Feature commits to replay: {} (onto {} main commits)",
        feature_commits.len(),
        num_main_commits
    );

    // Get the main branch tip as our onto target
    let main_tip = repo
        .git(&["rev-parse", &default_branch])
        .unwrap()
        .trim()
        .to_string();

    let pre_notes = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let pre_count = pre_notes.lines().filter(|l| !l.is_empty()).count();
    println!("AI notes before rebase: {}", pre_count);

    let timing_file = std::path::PathBuf::from("/tmp/monorepo_graphite_timing.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    println!(
        "\n--- Starting GRAPHITE-STYLE rebase ({} commits via commit-tree + update-ref) ---",
        feature_commits.len()
    );
    let rebase_start = Instant::now();

    // Replay each feature commit onto the new base using plumbing commands.
    //
    // This matches actual Graphite CLI behavior: all commits are replayed via
    // commit-tree first, then ONE update-ref moves the branch from old tip to
    // new tip. git-ai's post_update_ref_hook sees the same N-commit rewrite
    // shape as a standard git rebase.
    let old_tip = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut new_parent = main_tip.clone();
    for (idx, &feature_sha) in feature_commits.iter().enumerate() {
        let commit_start = Instant::now();

        // Get the old parent of this feature commit
        let old_parent = repo
            .git(&["rev-parse", &format!("{}^", feature_sha)])
            .unwrap()
            .trim()
            .to_string();

        // Use merge-tree to compute the rebased tree:
        // merge-tree --write-tree --merge-base <old_parent> <new_parent> <feature_commit>
        // This 3-way merge cherry-picks the diff (old_parent→feature) onto new_parent
        let merged_tree_output = repo
            .git(&[
                "merge-tree",
                "--write-tree",
                "--merge-base",
                &old_parent,
                &new_parent,
                feature_sha,
            ])
            .unwrap();
        let merged_tree = merged_tree_output
            .trim()
            .lines()
            .next()
            .unwrap()
            .to_string();

        // Get the original commit message
        let message = repo
            .git(&["log", "-1", "--format=%s", feature_sha])
            .unwrap()
            .trim()
            .to_string();

        // Create the new commit with commit-tree (no update-ref yet — just like Graphite)
        let new_commit = repo
            .git(&[
                "commit-tree",
                &merged_tree,
                "-p",
                &new_parent,
                "-m",
                &message,
            ])
            .unwrap()
            .trim()
            .to_string();

        new_parent = new_commit;

        if (idx + 1) % 10 == 0 || idx == feature_commits.len() - 1 {
            println!(
                "  Replayed commit {}/{} ({:.0}ms this, {:.1}s total)",
                idx + 1,
                feature_commits.len(),
                commit_start.elapsed().as_millis(),
                rebase_start.elapsed().as_secs_f64()
            );
        }
    }

    // ONE atomic update-ref moves the branch from old tip to new tip.
    // This is the single point where git-ai's wrapper detects a rewrite.
    let new_tip = new_parent;
    println!(
        "\nRunning single update-ref: {} -> {}",
        &old_tip[..12],
        &new_tip[..12]
    );
    repo.git_with_env(
        &[
            "update-ref",
            "refs/heads/feature/payments-refactor",
            &new_tip,
            &old_tip,
        ],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "2"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    )
    .unwrap();

    // Update working tree to match (like Graphite's `reset --keep`)
    repo.git(&["reset", "--hard", &new_tip]).unwrap();
    let rebase_dur = rebase_start.elapsed();

    let post_notes = repo
        .git(&["notes", "--ref=refs/notes/ai", "list"])
        .unwrap_or_default();
    let post_count = post_notes.lines().filter(|l| !l.is_empty()).count();
    println!("AI notes after rebase:  {}", post_count);

    // Phase timing breakdown
    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING ===");
        print!("{}", timing_data);
        println!("====================\n");
    } else {
        println!("(No timing file — possibly fast-path or no notes to rewrite)");
    }

    println!("\n=== MONOREPO GRAPHITE REBASE RESULTS ===");
    println!(
        "Repo: {} files, {} AI-tracked",
        num_background_files + num_ai_files,
        num_ai_files
    );
    println!(
        "Rebase: {} feature commits onto {} main commits",
        feature_commits.len(),
        num_main_commits
    );
    println!(
        "Total: {:.3}s ({:.0}ms)",
        rebase_dur.as_secs_f64(),
        rebase_dur.as_millis()
    );
    println!(
        "Per feature commit: {:.1}ms",
        rebase_dur.as_millis() as f64 / feature_commits.len() as f64
    );
    println!("=========================================\n");
}

fn extract_timing(data: &str, key: &str) -> Option<u64> {
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(key)
            && let Some(val) = trimmed.split('=').nth(1)
        {
            return val.trim_end_matches("ms").parse().ok();
        }
    }
    None
}

/// Benchmark that forces the SLOW path (VirtualAttributions + blame) by having
/// main branch also modify AI-touched files. This causes blob differences
/// between original and rebased commits, making the fast-path note remap fail.
///
/// This is the worst-case scenario and what we need to optimize.
#[test]
#[ignore]
fn benchmark_rebase_slow_path() {
    let num_feature_commits: usize = std::env::var("REBASE_BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let num_ai_files: usize = std::env::var("REBASE_BENCH_AI_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let lines_per_file: usize = std::env::var("REBASE_BENCH_LINES_PER_FILE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    println!("\n=== Slow-Path Rebase Benchmark ===");
    println!("Feature commits: {}", num_feature_commits);
    println!("AI files: {}", num_ai_files);
    println!("Lines per file: {}", lines_per_file);
    println!("===================================\n");

    let repo = TestRepo::new();

    // Create initial commit with the shared files that both branches will modify
    // This ensures both branches touch the same AI-tracked files
    {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            // Initial content: a header that main will modify + body that feature will modify
            lines.push(format!("// Header for module {}", file_idx).into());
            lines.push("// Main branch will add lines above this marker".into());
            for line_idx in 0..lines_per_file {
                lines.push(format!("// Initial AI code mod{} line{}", file_idx, line_idx).ai());
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit("Initial shared files").unwrap();
    }

    let default_branch = repo.current_branch();

    // Create feature branch with AI commits that modify the shared files
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    let setup_start = Instant::now();
    for commit_idx in 0..num_feature_commits {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let path = repo.path().join(&filename);

            // Read current content and append AI lines at the bottom
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = format!(
                "{}\n// AI addition v{} mod{}",
                current, commit_idx, file_idx
            );
            fs::write(&path, &new_content).unwrap();

            // Checkpoint as AI
            repo.git_ai(&["checkpoint", "mock_ai", &filename]).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("AI feature {}", commit_idx))
            .unwrap();

        if (commit_idx + 1) % 10 == 0 {
            println!(
                "  Feature commit {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                setup_start.elapsed().as_secs_f64()
            );
        }
    }
    println!("Feature setup: {:.1}s", setup_start.elapsed().as_secs_f64());

    // Go back to main and modify the SAME AI-tracked files at the TOP
    // This creates non-conflicting changes (different regions) that still cause
    // different blob OIDs after rebase, forcing the slow path
    repo.git(&["checkout", &default_branch]).unwrap();

    for main_idx in 0..5 {
        for file_idx in 0..num_ai_files {
            let filename = format!("shared/mod_{}/f_{}.rs", file_idx, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            // Insert at the top (before the marker)
            let new_content = current.replacen(
                "// Main branch will add lines above this marker",
                &format!(
                    "// Main addition {} for mod{}\n// Main branch will add lines above this marker",
                    main_idx, file_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("Main change {}", main_idx))
            .unwrap();
    }

    // Also add some unrelated main commits for realism
    for i in 0..10 {
        let filename = format!("main_only/change_{}.txt", i);
        let mut file = repo.filename(&filename);
        file.set_contents(crate::lines![format!("main only {}", i)]);
        repo.stage_all_and_commit(&format!("Main unrelated {}", i))
            .unwrap();
    }

    // Now rebase feature onto main - this should trigger the slow path
    // because the AI-tracked files have different blobs after rebase
    repo.git(&["checkout", "feature"]).unwrap();

    let timing_file = repo.path().join("..").join("rebase_timing.txt");
    let timing_path = timing_file.to_str().unwrap().to_string();

    println!("\n--- Starting slow-path rebase ---");
    let rebase_start = Instant::now();
    let result = repo.git_with_env(
        &["rebase", &default_branch],
        &[
            ("GIT_AI_DEBUG_PERFORMANCE", "1"),
            ("GIT_AI_REBASE_TIMING_FILE", &timing_path),
        ],
        None,
    );
    let rebase_duration = rebase_start.elapsed();

    match &result {
        Ok(output) => {
            println!("Rebase succeeded in {:.3}s", rebase_duration.as_secs_f64());
            // Print only last few lines of output to avoid noise
            let lines: Vec<&str> = output.lines().collect();
            let start = lines.len().saturating_sub(10);
            for line in &lines[start..] {
                println!("  {}", line);
            }
        }
        Err(e) => {
            println!(
                "Rebase FAILED in {:.3}s: {}",
                rebase_duration.as_secs_f64(),
                e
            );
        }
    }
    result.unwrap();

    // Read and display detailed timing breakdown
    if let Ok(timing_data) = fs::read_to_string(&timing_file) {
        println!("\n=== PHASE TIMING BREAKDOWN ===");
        print!("{}", timing_data);
        println!("===============================");
    }

    println!("\n=== SLOW-PATH BENCHMARK RESULTS ===");
    println!(
        "Total rebase time: {:.3}s ({:.0}ms)",
        rebase_duration.as_secs_f64(),
        rebase_duration.as_millis()
    );
    println!(
        "Per-commit average: {:.1}ms",
        rebase_duration.as_millis() as f64 / num_feature_commits as f64
    );
    println!("====================================\n");
}

/// Large-scale benchmark with mixed file sizes for PR comparison.
///
/// Creates:
/// - 200 AI-tracked files (150 × 1000 lines, 50 × 5000 lines)
/// - 150 feature commits, each modifying all files (ensuring AI attribution on every commit)
/// - Main branch also modifies the same files (forces diff-based path, not blob-copy fast path)
///
/// Run with: cargo test --package git-ai --test integration benchmark_large_scale_mixed -- --ignored --nocapture
#[test]
#[ignore]
fn benchmark_large_scale_mixed() {
    let num_small_files: usize = std::env::var("BENCH_SMALL_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let num_large_files: usize = std::env::var("BENCH_LARGE_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let small_file_lines: usize = std::env::var("BENCH_SMALL_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let large_file_lines: usize = std::env::var("BENCH_LARGE_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000);
    let num_feature_commits: usize = std::env::var("BENCH_FEATURE_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let num_main_commits: usize = std::env::var("BENCH_MAIN_COMMITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let total_files = num_small_files + num_large_files;
    let total_initial_lines =
        num_small_files * small_file_lines + num_large_files * large_file_lines;

    println!("\n=== Large-Scale Mixed Benchmark ===");
    println!(
        "Small files: {} × {} lines",
        num_small_files, small_file_lines
    );
    println!(
        "Large files: {} × {} lines",
        num_large_files, large_file_lines
    );
    println!("Total files: {}", total_files);
    println!("Total initial lines: {}", total_initial_lines);
    println!("Feature commits: {}", num_feature_commits);
    println!("Main commits: {}", num_main_commits);
    println!("====================================\n");

    let repo = TestRepo::new();
    let setup_start = Instant::now();

    // Create initial commit with all files
    {
        for file_idx in 0..total_files {
            let lines_for_file = if file_idx < num_small_files {
                small_file_lines
            } else {
                large_file_lines
            };
            let filename = format!("src/mod_{}/file_{}.rs", file_idx % 20, file_idx);
            let mut file = repo.filename(&filename);
            let mut lines: Vec<crate::repos::test_file::ExpectedLine> = Vec::new();
            lines.push(format!("// Module {} header", file_idx).into());
            lines.push("// MAIN_MARKER".into());
            for line_idx in 0..lines_for_file {
                lines.push(
                    format!(
                        "fn func_{}_{}() {{ /* AI generated */ }}",
                        file_idx, line_idx
                    )
                    .ai(),
                );
            }
            file.set_contents(lines);
        }
        repo.stage_all_and_commit("Initial: all AI files").unwrap();
    }
    println!(
        "Initial commit setup: {:.1}s",
        setup_start.elapsed().as_secs_f64()
    );

    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_start = Instant::now();

    for commit_idx in 0..num_feature_commits {
        // Each commit modifies a subset of files (rotating window of ~20 files)
        // but touches enough to exercise the diff path
        let files_per_commit = 20.min(total_files);
        let start_file = (commit_idx * 7) % total_files; // rotating start to vary which files

        for i in 0..files_per_commit {
            let file_idx = (start_file + i) % total_files;
            let filename = format!("src/mod_{}/file_{}.rs", file_idx % 20, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            // Append AI-authored line at the end
            let new_content = format!(
                "{}\nfn feature_{}_in_{}() {{ /* AI commit {} */ }}",
                current, commit_idx, file_idx, commit_idx
            );
            fs::write(&path, &new_content).unwrap();
            repo.git_ai(&["checkpoint", "mock_ai", &filename]).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("AI feature {}", commit_idx))
            .unwrap();

        if (commit_idx + 1) % 25 == 0 {
            println!(
                "  Feature commit {}/{} ({:.1}s)",
                commit_idx + 1,
                num_feature_commits,
                feature_start.elapsed().as_secs_f64()
            );
        }
    }
    println!(
        "Feature branch setup: {:.1}s ({} commits)",
        feature_start.elapsed().as_secs_f64(),
        num_feature_commits
    );

    // Advance main branch — modify AI-tracked files to force diff-based path
    repo.git(&["checkout", &default_branch]).unwrap();
    let main_start = Instant::now();
    for main_idx in 0..num_main_commits {
        // Main modifies a different rotating set of files at the MARKER line
        let files_per_main = 30.min(total_files);
        let start_file = (main_idx * 13) % total_files;
        for i in 0..files_per_main {
            let file_idx = (start_file + i) % total_files;
            let filename = format!("src/mod_{}/file_{}.rs", file_idx % 20, file_idx);
            let path = repo.path().join(&filename);
            let current = fs::read_to_string(&path).unwrap_or_default();
            let new_content = current.replacen(
                "// MAIN_MARKER",
                &format!(
                    "// Main change {} in file {}\n// MAIN_MARKER",
                    main_idx, file_idx
                ),
                1,
            );
            fs::write(&path, &new_content).unwrap();
        }
        repo.git(&["add", "-A"]).unwrap();
        repo.stage_all_and_commit(&format!("Main {}", main_idx))
            .unwrap();
    }
    // Add unrelated main commits
    for i in 0..5 {
        let mut f = repo.filename(&format!("main_only/f_{}.txt", i));
        f.set_contents(crate::lines![format!("main only {}", i)]);
        repo.stage_all_and_commit(&format!("Main unrelated {}", i))
            .unwrap();
    }
    println!(
        "Main branch setup: {:.1}s",
        main_start.elapsed().as_secs_f64()
    );
    println!("Total setup: {:.1}s", setup_start.elapsed().as_secs_f64());

    // Rebase using benchmark_git for structured timing
    repo.git(&["checkout", "feature"]).unwrap();

    println!(
        "\n--- Starting rebase ({} commits onto {}) ---",
        num_feature_commits, default_branch
    );
    let wall_start = Instant::now();
    let bench_result = repo.benchmark_git(&["rebase", &default_branch]);
    let wall_duration = wall_start.elapsed();

    match &bench_result {
        Ok(bench) => {
            let git_ms = bench.git_duration.as_millis();
            let total_ms = bench.total_duration.as_millis();
            let pre_ms = bench.pre_command_duration.as_millis();
            let post_ms = bench.post_command_duration.as_millis();
            let overhead_ms = total_ms.saturating_sub(git_ms);
            let overhead_pct = if git_ms > 0 {
                overhead_ms as f64 / git_ms as f64 * 100.0
            } else {
                0.0
            };

            println!("\n╔══════════════════════════════════════════════════════════╗");
            println!("║          LARGE-SCALE BENCHMARK RESULTS                  ║");
            println!("╠══════════════════════════════════════════════════════════╣");
            println!(
                "║  Files:          {} ({} × {}L + {} × {}L)",
                total_files, num_small_files, small_file_lines, num_large_files, large_file_lines
            );
            println!("║  Initial lines:  {}", total_initial_lines);
            println!("║  Commits:        {}", num_feature_commits);
            println!("╠══════════════════════════════════════════════════════════╣");
            println!("║  Wall time:      {:.3}s", wall_duration.as_secs_f64());
            println!("║  Total (wrapper): {}ms", total_ms);
            println!("║  Git rebase:     {}ms", git_ms);
            println!("║  Pre-command:    {}ms", pre_ms);
            println!("║  Post-command:   {}ms", post_ms);
            println!(
                "║  Overhead:       {}ms ({:.1}% of git time)",
                overhead_ms, overhead_pct
            );
            println!(
                "║  Per-commit avg: {:.1}ms total, {:.1}ms git, {:.1}ms overhead",
                total_ms as f64 / num_feature_commits as f64,
                git_ms as f64 / num_feature_commits as f64,
                overhead_ms as f64 / num_feature_commits as f64,
            );
            println!("╚══════════════════════════════════════════════════════════╝\n");
        }
        Err(e) => {
            println!(
                "Benchmark failed after {:.3}s: {}",
                wall_duration.as_secs_f64(),
                e
            );
            println!(
                "Wall time: {:.3}s ({:.0}ms)",
                wall_duration.as_secs_f64(),
                wall_duration.as_millis()
            );
            panic!("Benchmark failed: {}", e);
        }
    }
}
