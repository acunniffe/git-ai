use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use crate::test_utils::DurationStatistics;
use git_ai::model::working_log::CheckpointKind;
use std::fs;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct DurationStats {
    count: usize,
    average: Duration,
    min: Duration,
    max: Duration,
    p50: Duration,
    p95: Duration,
}

impl DurationStats {
    fn from_durations(durations: &mut [Duration]) -> Self {
        let stats = DurationStatistics::from_durations(durations);
        assert!(stats.count() > 0);

        Self {
            count: stats.count(),
            average: stats.average().unwrap(),
            min: stats.min().unwrap(),
            max: stats.max().unwrap(),
            p50: stats.percentile_upper_index(0.5).unwrap(),
            p95: stats.percentile_upper_index(0.95).unwrap(),
        }
    }

    fn print(&self, label: &str) {
        println!(
            "{label} ({} runs): avg={:.1}ms min={:.1}ms p50={:.1}ms p95={:.1}ms max={:.1}ms",
            self.count,
            self.average.as_secs_f64() * 1000.0,
            self.min.as_secs_f64() * 1000.0,
            self.p50.as_secs_f64() * 1000.0,
            self.p95.as_secs_f64() * 1000.0,
            self.max.as_secs_f64() * 1000.0,
        );
    }
}

fn benchmark_checkpoint_daemon(iterations: usize) -> DurationStats {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let repo_path = repo.canonical_path();

    fs::write(repo_path.join("base.txt"), "initial\n").unwrap();
    repo.stage_all_and_commit("init").unwrap();

    // Warm-up
    fs::write(repo_path.join("warmup.txt"), "warmup\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "warmup.txt"])
        .unwrap();
    repo.sync_daemon();

    let mut durations = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let fname = format!("file_{i}.txt");
        fs::write(repo_path.join(&fname), format!("content {i}\n")).unwrap();
        let start = Instant::now();
        repo.git_ai(&["checkpoint", "mock_ai", &fname]).unwrap();
        durations.push(start.elapsed());
    }
    DurationStats::from_durations(&mut durations)
}

#[test]
#[ignore]
fn bench_checkpoint_single_file_daemon() {
    println!("\n=== Checkpoint Single-File Benchmark ===");
    let stats = benchmark_checkpoint_daemon(20);
    stats.print("Checkpoint");
    assert!(
        stats.p95 < Duration::from_millis(200),
        "p95 checkpoint latency too high: {:?}",
        stats.p95
    );
}

#[test]
#[ignore]
fn bench_checkpoint_multi_file_daemon() {
    println!("\n=== Checkpoint Multi-File Benchmark ===");
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let repo_path = repo.canonical_path();
    fs::write(repo_path.join("base.txt"), "initial\n").unwrap();
    repo.stage_all_and_commit("init").unwrap();

    // Warm-up
    fs::write(repo_path.join("w.txt"), "w\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "w.txt"]).unwrap();
    repo.sync_daemon();

    let file_counts = [1, 5, 10, 20];
    for &file_count in &file_counts {
        let mut durations = Vec::with_capacity(10);
        for iter in 0..10 {
            let mut files = Vec::with_capacity(file_count);
            for f in 0..file_count {
                let fname = format!("multi_{iter}_{f}.txt");
                fs::write(repo_path.join(&fname), format!("content {iter}_{f}\n")).unwrap();
                files.push(fname);
            }
            let mut args: Vec<&str> = vec!["checkpoint", "mock_ai"];
            args.extend(files.iter().map(|s| s.as_str()));
            let start = Instant::now();
            repo.git_ai(&args).unwrap();
            durations.push(start.elapsed());
        }
        let stats = DurationStats::from_durations(&mut durations);
        stats.print(&format!("  {file_count} files"));
    }
}

#[test]
#[ignore]
fn bench_checkpoint_correctness_after_optimization() {
    println!("\n=== Checkpoint Correctness Verification ===");
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);
    let repo_path = repo.canonical_path();

    fs::write(repo_path.join("verified.txt"), "initial\n").unwrap();
    repo.stage_all_and_commit("init").unwrap();

    fs::write(repo_path.join("verified.txt"), "modified by AI\n").unwrap();
    let start = Instant::now();
    repo.git_ai(&["checkpoint", "mock_ai", "verified.txt"])
        .unwrap();
    let checkpoint_dur = start.elapsed();

    repo.sync_daemon();

    let checkpoints = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("should read checkpoints");

    assert!(
        !checkpoints.is_empty(),
        "at least one checkpoint should exist"
    );

    let last = checkpoints.last().unwrap();
    assert!(
        last.kind == CheckpointKind::AiAgent,
        "checkpoint kind should be AiAgent"
    );
    assert!(
        !last.entries.is_empty(),
        "checkpoint should have file entries"
    );
    assert!(
        last.entries.iter().any(|e| e.file == "verified.txt"),
        "checkpoint should include verified.txt"
    );

    println!(
        "Correctness verified: checkpoint took {:.1}ms, {} entries",
        checkpoint_dur.as_secs_f64() * 1000.0,
        last.entries.len()
    );
}

#[test]
#[ignore]
fn bench_checkpoint_all_modes_summary() {
    println!("\n============================================");
    println!("  Checkpoint Performance Summary");
    println!("============================================\n");

    let daemon = benchmark_checkpoint_daemon(20);
    daemon.print("Checkpoint");

    println!("\n============================================\n");

    assert!(
        daemon.p95 < Duration::from_millis(200),
        "checkpoint p95 too high"
    );
}
