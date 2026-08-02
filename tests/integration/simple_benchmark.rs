//! Simple benchmark for measuring checkpoint and commit timing.
//!
//! This benchmark measures the time taken for:
//! 1. AI checkpoints (with real Claude transcript)
//! 2. Git commits
//!
//! Run with: cargo test test_simple_ai_checkpoint_and_commit --release -- --nocapture --ignored

use crate::repos::test_repo::TestRepo;
use crate::test_utils::{DurationStatistics, fixture_path};
use serde_json::json;
use std::fs;
use std::time::{Duration, Instant};

/// Timing data for a single benchmark iteration
#[derive(Debug, Clone)]
struct IterationTiming {
    checkpoint_duration: Duration,
    commit_duration: Duration,
}

impl IterationTiming {
    fn total(&self) -> Duration {
        self.checkpoint_duration + self.commit_duration
    }
}

/// Statistics for a set of duration measurements
#[derive(Debug)]
struct DurationStats {
    count: usize,
    average: Duration,
    min: Duration,
    max: Duration,
    std_dev_ms: f64,
}

impl DurationStats {
    fn from_durations(durations: &[Duration]) -> Self {
        let stats = DurationStatistics::from_durations(durations);

        Self {
            count: stats.count(),
            average: stats.average().unwrap_or(Duration::ZERO),
            min: stats.min().unwrap_or(Duration::ZERO),
            max: stats.max().unwrap_or(Duration::ZERO),
            std_dev_ms: stats.std_dev_ms().unwrap_or(0.0),
        }
    }

    fn print(&self, label: &str) {
        println!("\n=== {} ({} runs) ===", label, self.count);
        println!("  Average:  {:.2}ms", self.average.as_secs_f64() * 1000.0);
        println!("  Min:      {:.2}ms", self.min.as_secs_f64() * 1000.0);
        println!("  Max:      {:.2}ms", self.max.as_secs_f64() * 1000.0);
        println!("  Std Dev:  {:.2}ms", self.std_dev_ms);
    }
}

/// Run a single iteration of the benchmark
fn run_iteration(
    repo: &TestRepo,
    counter_file: &std::path::Path,
    transcript_path: &std::path::Path,
    iteration: u32,
) -> IterationTiming {
    // Read current counter value and increment
    let current_value: u32 = fs::read_to_string(counter_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let new_value = current_value + 1;

    // Write new value
    fs::write(counter_file, new_value.to_string()).unwrap();

    // Create hook input JSON
    let hook_input = json!({
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "transcript_path": transcript_path.to_string_lossy().to_string(),
        "tool_input": {
            "file_path": counter_file.to_string_lossy().to_string()
        }
    })
    .to_string();

    // Time checkpoint
    let checkpoint_start = Instant::now();
    repo.checkpoint_with_hook_input("claude", &hook_input)
        .expect("Checkpoint should succeed");
    let checkpoint_duration = checkpoint_start.elapsed();

    // Time commit
    let commit_start = Instant::now();
    repo.stage_all_and_commit(&format!("Update counter to {}", new_value))
        .expect("Commit should succeed");
    let commit_duration = commit_start.elapsed();

    // Print progress
    println!(
        "Iteration {}: checkpoint={:.2}ms, commit={:.2}ms, total={:.2}ms",
        iteration,
        checkpoint_duration.as_secs_f64() * 1000.0,
        commit_duration.as_secs_f64() * 1000.0,
        (checkpoint_duration + commit_duration).as_secs_f64() * 1000.0
    );

    IterationTiming {
        checkpoint_duration,
        commit_duration,
    }
}

#[test]
#[ignore] // Run with --ignored flag since this is a benchmark
fn test_simple_ai_checkpoint_and_commit() {
    const NUM_ITERATIONS: u32 = 10;

    println!("\n========================================");
    println!("Simple AI Checkpoint + Commit Benchmark");
    println!("========================================");
    println!("Iterations: {}", NUM_ITERATIONS);
    println!();

    // Create test repository
    let repo = TestRepo::new();
    let repo_path = repo.canonical_path();
    eprintln!("repo_path: {}", repo_path.to_str().unwrap());

    // Create counter file with initial value
    let counter_file = repo_path.join("counter.txt");
    fs::write(&counter_file, "0").unwrap();

    // Initial commit
    repo.stage_all_and_commit("Initial commit with counter")
        .expect("Initial commit should succeed");

    // Use Claude fixture directly (no need to copy)
    let transcript_path = fixture_path("example-claude-code.jsonl");

    println!("Setup complete. Starting benchmark...\n");

    // Collect timing data
    let mut timings: Vec<IterationTiming> = Vec::with_capacity(NUM_ITERATIONS as usize);

    for i in 1..=NUM_ITERATIONS {
        let timing = run_iteration(&repo, &counter_file, &transcript_path, i);
        timings.push(timing);
    }

    // Calculate and print statistics
    let checkpoint_durations: Vec<Duration> =
        timings.iter().map(|t| t.checkpoint_duration).collect();
    let commit_durations: Vec<Duration> = timings.iter().map(|t| t.commit_duration).collect();
    let total_durations: Vec<Duration> = timings.iter().map(|t| t.total()).collect();

    let checkpoint_stats = DurationStats::from_durations(&checkpoint_durations);
    let commit_stats = DurationStats::from_durations(&commit_durations);
    let total_stats = DurationStats::from_durations(&total_durations);

    println!("\n========================================");
    println!("BENCHMARK RESULTS");
    println!("========================================");

    checkpoint_stats.print("Checkpoint Statistics");
    commit_stats.print("Commit Statistics");
    total_stats.print("Total (Checkpoint + Commit)");

    println!("\n========================================\n");
}
