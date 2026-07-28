use git_ai::feature_flags::{DEFAULT_HOOKS_SYNC_INTERVAL_SECS, FeatureFlags};
use rand::seq::IndexedRandom;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::repos::test_repo::{BenchmarkResult, TestRepo};

fn setup() {
    git_ai::config::Config::clear_test_feature_flags();

    // Test that we can override feature flags
    let test_flags = FeatureFlags {
        auth_keyring: false,
        transcript_streaming: true,
        transcript_sweep: true,
        checkpoint_debug_log: false,
        bash_checkpoints_v2: false,
        daemon_log_upload: true,
        rewrite_metrics_events: false,
        hooks_sync_interval: DEFAULT_HOOKS_SYNC_INTERVAL_SECS,
    };

    git_ai::config::Config::set_test_feature_flags(test_flags.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::seq::IndexedRandom;
    use rstest::rstest;

    // Performance floor constant (270ms) - used to determine if overhead is acceptable
    const PERFORMANCE_FLOOR_MS: Duration = Duration::from_millis(270);

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_human_only_edits_then_commit(#[case] repo_name: &str) {
        use std::time::Instant;

        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));
        // Find random files for testing
        println!("Finding random files for {}", repo_name);
        let start = Instant::now();
        let random_files = find_random_files(test_repo).expect("Should find random files");
        let duration = start.elapsed();
        println!("Time taken to find random files: {:?}", duration);
        // Select 3 random files (not large ones)
        let files_to_edit: Vec<String> =
            random_files.random_files.iter().take(3).cloned().collect();

        assert!(
            files_to_edit.len() >= 3,
            "Should have at least 3 random files to edit"
        );

        // Create a sampler that runs 10 times
        let sampler = Sampler::new(10);

        // Sample the performance of human-only edits + commit
        let result = sampler.sample(test_repo, |repo| {
            // Append "# Human Line" to each file
            for file_path in &files_to_edit {
                let full_path = repo.path().join(file_path);

                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&full_path)
                    .unwrap_or_else(|_| panic!("Should be able to open file: {}", file_path));

                file.write_all(b"\n# Human Line\n")
                    .unwrap_or_else(|_| panic!("Should be able to write to file: {}", file_path));
            }

            // Stage the files (regular git, no benchmark)
            for file_path in &files_to_edit {
                repo.git(&["add", file_path])
                    .unwrap_or_else(|_| panic!("Should be able to stage file: {}", file_path));
            }

            // Benchmark the commit operation (where pre-commit hook runs)
            repo.benchmark_git(&["commit", "-m", "Human-only edits"])
                .expect("Commit should succeed")
        });

        // Print the results
        result.print_summary(&format!("Human-only edits + commit ({})", repo_name));

        let (percent_overhead, average_overhead) = result.average_overhead();

        assert!(
            percent_overhead < 10.0 || average_overhead < PERFORMANCE_FLOOR_MS,
            "Average overhead should be less than 10% or under 70ms"
        );
    }

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_ai_and_human_edits(#[case] repo_name: &str) {
        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));
        // Find random files for testing
        let random_files = find_random_files(test_repo).expect("Should find random files");

        // Select 3 random files (not large ones)
        let files_to_edit: Vec<String> =
            random_files.random_files.iter().take(3).cloned().collect();

        assert!(
            files_to_edit.len() >= 3,
            "Should have at least 3 random files to edit"
        );

        // Create a sampler that runs 10 times
        let sampler = Sampler::new(10);

        // Sample the performance of AI and human edits + commit
        let result = sampler.sample(test_repo, |repo| {
            for file_path in &files_to_edit {
                let full_path = repo.path().join(file_path);

                // Step 1: Append "# Human Line" to the file
                {
                    let mut file = OpenOptions::new()
                        .append(true)
                        .open(&full_path)
                        .unwrap_or_else(|_| panic!("Should be able to open file: {}", file_path));

                    file.write_all(b"\n# Human Line\n").unwrap_or_else(|_| {
                        panic!("Should be able to write to file: {}", file_path)
                    });
                }

                // Step 2: Run git-ai checkpoint
                repo.git_ai(&["checkpoint", file_path])
                    .unwrap_or_else(|_| panic!("Should be able to checkpoint file: {}", file_path));

                // Step 3: Insert "# AI Line" at the top of the file
                {
                    let content = std::fs::read_to_string(&full_path)
                        .unwrap_or_else(|_| panic!("Should be able to read file: {}", file_path));

                    let new_content = format!("# AI Line\n{}", content);

                    std::fs::write(&full_path, new_content).unwrap_or_else(|_| {
                        panic!("Should be able to write to file: {}", file_path)
                    });
                }

                // Step 4: Run git-ai mock_ai
                repo.git_ai(&["checkpoint", "mock_ai", file_path])
                    .unwrap_or_else(|_| panic!("Should be able to mock_ai file: {}", file_path));
            }

            // Benchmark the commit operation (where pre-commit hook runs)
            repo.benchmark_git(&["commit", "-a", "-m", "AI and human edits"])
                .expect("Commit should succeed")
        });

        // Print the results
        result.print_summary(&format!("AI and human edits + commit ({})", repo_name));

        let (percent_overhead, average_overhead) = result.average_overhead();

        assert!(
            percent_overhead < 10.0 || average_overhead < PERFORMANCE_FLOOR_MS,
            "Average overhead should be less than 10% or under 70ms"
        );
    }

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_git_reset_head_5_mixed(#[case] repo_name: &str) {
        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));

        // Create a sampler that runs 10 times
        let sampler = Sampler::new(10);

        // Sample the performance of git reset HEAD~5 --mixed
        let result = sampler.sample(test_repo, |repo| {
            // Benchmark the reset operation with explicit --mixed flag
            repo.benchmark_git(&["reset", "HEAD~5", "--mixed"])
                .expect("Reset should succeed")
        });

        // Print the results
        result.print_summary(&format!("git reset HEAD~5 --mixed ({})", repo_name));

        let (percent_overhead, _) = result.average_overhead();

        assert!(
            percent_overhead < 20.0,
            "Average overhead should be less than 20%"
        );
    }

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_human_only_edits_in_big_files_then_commit(#[case] repo_name: &str) {
        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));

        // Find random files for testing
        let random_files = find_random_files(test_repo).expect("Should find random files");

        // Use large files for testing
        let files_to_edit: Vec<String> = random_files.large_files.clone();

        assert!(
            !files_to_edit.is_empty(),
            "Should have at least 1 large file to edit"
        );

        // Create a sampler that runs 10 times
        let sampler = Sampler::new(10);

        // Sample the performance of human-only edits + commit on large files
        let result = sampler.sample(test_repo, |repo| {
            // Append "# Human Line" to each file
            for file_path in &files_to_edit {
                let full_path = repo.path().join(file_path);

                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&full_path)
                    .unwrap_or_else(|_| panic!("Should be able to open file: {}", file_path));

                file.write_all(b"\n# Human Line\n")
                    .unwrap_or_else(|_| panic!("Should be able to write to file: {}", file_path));
            }

            // Stage the files (regular git, no benchmark)
            for file_path in &files_to_edit {
                repo.git(&["add", file_path])
                    .unwrap_or_else(|_| panic!("Should be able to stage file: {}", file_path));
            }

            // Benchmark the commit operation (where pre-commit hook runs)
            repo.benchmark_git(&["commit", "-m", "Human-only edits in big files"])
                .expect("Commit should succeed")
        });

        // Print the results
        result.print_summary(&format!(
            "Human-only edits in big files + commit ({})",
            repo_name
        ));

        let (percent_overhead, average_overhead) = result.average_overhead();

        assert!(
            percent_overhead < 10.0 || average_overhead < PERFORMANCE_FLOOR_MS,
            "Average overhead should be less than 10% or under 70ms"
        );
    }

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_git_reset_head_5(#[case] repo_name: &str) {
        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));

        // Create a sampler that runs 10 times
        let sampler = Sampler::new(10);

        // Sample the performance of git reset HEAD~5
        let result = sampler.sample(test_repo, |repo| {
            // Benchmark the reset operation (--mixed is the default)
            repo.benchmark_git(&["reset", "HEAD~5"])
                .expect("Reset should succeed")
        });

        // Print the results
        result.print_summary(&format!("git reset HEAD~5 ({})", repo_name));

        let (percent_overhead, _) = result.average_overhead();

        assert!(
            percent_overhead < 20.0,
            "Average overhead should be less than 20%"
        );
    }

    #[rstest]
    #[case("chromium")]
    #[case("react")]
    #[case("node")]
    #[case("chakracore")]
    #[ignore]
    fn test_large_checkpoints(#[case] repo_name: &str) {
        use std::time::Instant;

        let repos = get_performance_repos();
        let test_repo = repos
            .get(repo_name)
            .unwrap_or_else(|| panic!("{} repo should be available", repo_name));

        // Find 1000 random files for testing
        println!("Finding 1000 random files for {}", repo_name);
        let start = Instant::now();
        let random_files = find_random_files_with_options(
            test_repo,
            FindRandomFilesOptions {
                random_file_count: 2200,
                large_file_count: 0,
            },
        )
        .expect("Should find random files");
        let duration = start.elapsed();
        println!("Time taken to find random files: {:?}", duration);

        let all_files: Vec<String> = random_files.random_files;
        println!("Found {} files to edit", all_files.len());

        // Create a sampler that runs 5 times (fewer due to the large number of files)
        let sampler = Sampler::new(5);

        // Sample the performance of large checkpoint operations
        let result = sampler.sample(test_repo, |repo| {
            // Step 1: Edit all 1000 files (simulating AI edits)
            println!("Editing {} files...", all_files.len());
            for file_path in &all_files {
                let full_path = repo.path().join(file_path);

                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&full_path)
                    .unwrap_or_else(|_| panic!("Should be able to open file: {}", file_path));

                file.write_all(b"\n# AI Generated Line\n")
                    .unwrap_or_else(|_| panic!("Should be able to write to file: {}", file_path));
            }

            // Step 2: Run git-ai checkpoint mock_ai -- <all pathspecs>
            println!("Running checkpoint mock_ai on {} files...", all_files.len());
            let mut checkpoint_args: Vec<&str> = vec!["checkpoint", "mock_ai", "--"];
            let all_files_refs: Vec<&str> = all_files.iter().map(|s| s.as_str()).collect();
            checkpoint_args.extend(all_files_refs.iter());

            repo.git_ai(&checkpoint_args)
                .expect("Checkpoint mock_ai should succeed");

            // Step 3: Select 100 random files from the 1000 and edit them (simulating human edits)
            let mut rng = rand::rng();
            let files_to_re_edit: Vec<String> = all_files
                .sample(&mut rng, 100.min(all_files.len()))
                .cloned()
                .collect();

            println!(
                "Re-editing {} files (human edits)...",
                files_to_re_edit.len()
            );
            for file_path in &files_to_re_edit {
                let full_path = repo.path().join(file_path);

                let mut file = OpenOptions::new()
                    .append(true)
                    .open(&full_path)
                    .unwrap_or_else(|_| panic!("Should be able to open file: {}", file_path));

                file.write_all(b"\n# Human Line\n")
                    .unwrap_or_else(|_| panic!("Should be able to write to file: {}", file_path));
            }

            // Step 4: Benchmark the checkpoint on the 100 human-edited files
            println!(
                "Benchmarking checkpoint on {} files...",
                files_to_re_edit.len()
            );
            let mut final_checkpoint_args: Vec<&str> = vec!["checkpoint", "--"];
            let files_to_re_edit_refs: Vec<&str> =
                files_to_re_edit.iter().map(|s| s.as_str()).collect();
            final_checkpoint_args.extend(files_to_re_edit_refs.iter());

            repo.benchmark_git_ai(&final_checkpoint_args)
                .expect("Checkpoint should succeed")
        });

        // Print the results
        result.print_summary(&format!("Large checkpoints ({})", repo_name));

        // For checkpoint operations, we measure time per file
        // The benchmark is on 100 files, so we calculate ms per file
        let files_benchmarked = 100;
        let avg_total_ms = result.average.total_duration.as_millis() as f64;
        let ms_per_file = avg_total_ms / files_benchmarked as f64;

        println!(
            "Average total time: {:.2}ms, Files: {}, Time per file: {:.2}ms",
            avg_total_ms, files_benchmarked, ms_per_file
        );

        // Assert that checkpoint takes less than 50ms per file on average
        assert!(
            ms_per_file < 50.0,
            "Checkpoint should take less than 50ms per file, got {:.2}ms per file",
            ms_per_file
        );
    }
}

const PERFORMANCE_REPOS: &[(&str, &str)] = &[
    ("chromium", "https://github.com/chromium/chromium.git"),
    ("react", "https://github.com/facebook/react.git"),
    ("node", "https://github.com/nodejs/node.git"),
    ("chakracore", "https://github.com/microsoft/ChakraCore.git"),
];

static PERFORMANCE_REPOS_MAP: OnceLock<HashMap<String, TestRepo>> = OnceLock::new();

fn clone_and_init_repos() -> HashMap<String, TestRepo> {
    // Determine the project root (where Cargo.toml is)
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let project_root = PathBuf::from(manifest_dir);
    let perf_repos_dir = project_root.join(".performance-repos");

    // Create .performance-repos directory if it doesn't exist
    if !perf_repos_dir.exists() {
        std::fs::create_dir_all(&perf_repos_dir)
            .expect("Failed to create .performance-repos directory");
    }

    let mut repos_map = HashMap::new();

    for (name, url) in PERFORMANCE_REPOS {
        let repo_path = perf_repos_dir.join(name);

        // Check if repository is already cloned
        if !(repo_path.exists() && repo_path.join(".git").exists()) {
            // Clone the repository with full history
            let output = Command::new("git")
                .args(["clone", url, name, "--depth=150000"])
                .current_dir(&perf_repos_dir)
                .output()
                .unwrap_or_else(|_| panic!("Failed to clone repository: {}", name));

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                panic!("Failed to clone {}: {}", name, stderr);
            }
        }

        // Create TestRepo wrapper for the cloned repository
        // Note: Branch creation and checkout is handled by the Sampler before each benchmark run
        let test_repo = TestRepo::new_at_path(&repo_path);
        repos_map.insert(name.to_string(), test_repo);
    }
    repos_map
}

/// Get the performance test repositories
/// This function ensures repositories are cloned and initialized only once
pub fn get_performance_repos() -> &'static HashMap<String, TestRepo> {
    setup();
    PERFORMANCE_REPOS_MAP.get_or_init(clone_and_init_repos)
}

/// Result of finding random files in a repository
#[derive(Debug)]
pub struct RandomFiles {
    /// Random files from the repository (default 10)
    pub random_files: Vec<String>,
    /// 2 random large files (5k-10k lines)
    pub large_files: Vec<String>,
}

/// Options for finding random files
pub struct FindRandomFilesOptions {
    /// Number of random files to find (default 10)
    pub random_file_count: usize,
    /// Number of large files to find (default 2)
    pub large_file_count: usize,
}

impl Default for FindRandomFilesOptions {
    fn default() -> Self {
        Self {
            random_file_count: 10,
            large_file_count: 2,
        }
    }
}

/// Find random files in a repository for performance testing
///
/// Returns:
/// - 10 random files from the repository
/// - 2 random large files (by byte size, as a proxy for line count)
///
/// This helper uses filesystem operations directly instead of git commands
/// for much faster performance on large repositories.
pub fn find_random_files(test_repo: &TestRepo) -> Result<RandomFiles, String> {
    find_random_files_with_options(test_repo, FindRandomFilesOptions::default())
}

/// Find random files in a repository with custom options
///
/// Returns:
/// - `random_file_count` random files from the repository
/// - `large_file_count` random large files (by byte size, as a proxy for line count)
///
/// This helper uses filesystem operations directly instead of git commands
/// for much faster performance on large repositories.
pub fn find_random_files_with_options(
    test_repo: &TestRepo,
    options: FindRandomFilesOptions,
) -> Result<RandomFiles, String> {
    use std::fs;

    let repo_path = test_repo.path();

    // Collect all files recursively, skipping .git directory
    let mut all_files: Vec<String> = Vec::new();
    let mut dirs_to_visit: Vec<std::path::PathBuf> = vec![repo_path.to_path_buf()];

    while let Some(dir) = dirs_to_visit.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| format!("Failed to read dir: {}", e))?;

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Skip .git directory
            if file_name == ".git" {
                continue;
            }

            if path.is_dir() {
                dirs_to_visit.push(path);
            } else if path.is_file() {
                // Get relative path from repo root
                if let Ok(relative) = path.strip_prefix(repo_path)
                    && let Some(rel_str) = relative.to_str()
                {
                    all_files.push(rel_str.to_string());
                }
            }
        }
    }

    if all_files.is_empty() {
        return Err("No files found in repository".to_string());
    }

    let mut rng = rand::rng();

    // Find large files using file size as a proxy (> 100KB considered large)
    // This is much faster than reading files to count lines
    const LARGE_FILE_THRESHOLD: u64 = 100 * 1024; // 100KB

    let mut file_sizes: Vec<(String, u64)> = Vec::new();
    for file_path in &all_files {
        let full_path = repo_path.join(file_path);
        if let Ok(metadata) = fs::metadata(&full_path) {
            let size = metadata.len();
            if size >= LARGE_FILE_THRESHOLD {
                file_sizes.push((file_path.clone(), size));
            }
        }
    }

    // Sort by size descending and take top N
    file_sizes.sort_by_key(|b| std::cmp::Reverse(b.1));
    let large_files: Vec<String> = file_sizes
        .into_iter()
        .take(options.large_file_count)
        .map(|(p, _)| p)
        .collect();

    // Select N random files, excluding large files
    let candidates: Vec<&String> = all_files
        .iter()
        .filter(|f| !large_files.contains(f))
        .collect();

    let random_files: Vec<String> = candidates
        .sample(&mut rng, options.random_file_count.min(candidates.len()))
        .map(|s| (*s).clone())
        .collect();

    Ok(RandomFiles {
        random_files,
        large_files,
    })
}

/// Result of sampling a benchmark operation over multiple runs
#[derive(Debug, Clone)]
pub struct BenchmarkSampleResult {
    /// Number of runs performed
    pub num_runs: usize,
    /// Average benchmark result across all runs
    pub average: BenchmarkResult,
    /// Minimum benchmark result
    pub min: BenchmarkResult,
    /// Maximum benchmark result
    pub max: BenchmarkResult,
    /// All individual benchmark results
    pub results: Vec<BenchmarkResult>,
}

impl BenchmarkSampleResult {
    pub fn average_overhead(&self) -> (f64, Duration) {
        // Calculate overhead statistics (where total > git)
        let overhead_results: Vec<_> = self
            .results
            .iter()
            .filter(|r| r.total_duration > r.git_duration)
            .collect();

        if overhead_results.is_empty() {
            return (0.0, Duration::ZERO);
        }

        // Calculate average absolute overhead
        let total_overhead: Duration = overhead_results
            .iter()
            .map(|r| r.total_duration - r.git_duration)
            .sum();
        let avg_absolute_overhead = total_overhead / overhead_results.len() as u32;

        // Calculate average percentage overhead
        let total_percentage_overhead: f64 = overhead_results
            .iter()
            .map(|r| {
                let overhead = r.total_duration.as_secs_f64() - r.git_duration.as_secs_f64();
                let git_time = r.git_duration.as_secs_f64();
                if git_time > 0.0 {
                    (overhead / git_time) * 100.0
                } else {
                    0.0
                }
            })
            .sum();
        let avg_percentage_overhead = total_percentage_overhead / overhead_results.len() as f64;

        (avg_percentage_overhead, avg_absolute_overhead)
    }
    /// Print a formatted summary of the benchmark sample results
    pub fn print_summary(&self, operation_name: &str) {
        println!("\n=== Benchmark Summary: {} ===", operation_name);
        println!("  Runs:       {}", self.num_runs);
        println!(
            "  Average Total Duration:    {:?}",
            self.average.total_duration
        );
        println!(
            "  Average Git Duration:     {:?}",
            self.average.git_duration
        );
        println!(
            "  Average Pre-command:      {:?}",
            self.average.pre_command_duration
        );
        println!(
            "  Average Post-command:     {:?}",
            self.average.post_command_duration
        );
        println!("  Min Total Duration:       {:?}", self.min.total_duration);
        println!("  Max Total Duration:       {:?}", self.max.total_duration);

        // Calculate overhead statistics (where total > git)
        let overhead_results: Vec<_> = self
            .results
            .iter()
            .filter(|r| r.total_duration > r.git_duration)
            .collect();

        if !overhead_results.is_empty() {
            // Calculate average absolute overhead
            let total_overhead: Duration = overhead_results
                .iter()
                .map(|r| r.total_duration - r.git_duration)
                .sum();
            let avg_absolute_overhead = total_overhead / overhead_results.len() as u32;

            // Calculate average percentage overhead
            let total_percentage_overhead: f64 = overhead_results
                .iter()
                .map(|r| {
                    let overhead = r.total_duration.as_secs_f64() - r.git_duration.as_secs_f64();
                    let git_time = r.git_duration.as_secs_f64();
                    if git_time > 0.0 {
                        (overhead / git_time) * 100.0
                    } else {
                        0.0
                    }
                })
                .sum();
            let avg_percentage_overhead = total_percentage_overhead / overhead_results.len() as f64;

            println!(
                "  Overhead Cases:           {} (out of {})",
                overhead_results.len(),
                self.num_runs
            );
            println!("  Average Absolute Overhead: {:?}", avg_absolute_overhead);
            println!(
                "  Average % Overhead:       {:.2}%",
                avg_percentage_overhead
            );
        } else {
            println!("  Overhead Cases:           0 (out of {})", self.num_runs);
            println!("  Average Absolute Overhead: N/A (no overhead cases)");
            println!("  Average % Overhead:       N/A (no overhead cases)");
        }
    }
}

/// A sampler for measuring performance of operations on test repositories
pub struct Sampler {
    num_runs: usize,
}

impl Sampler {
    /// Create a new sampler that will run operations n times
    pub fn new(num_runs: usize) -> Self {
        assert!(num_runs > 0, "num_runs must be greater than 0");
        Self { num_runs }
    }

    /// Sample a benchmark operation over multiple runs
    ///
    /// Automatically resets the repository to a clean state before each run:
    /// - Resets with --hard to clean any changes
    /// - Checks out main or master branch
    /// - Creates a new timestamped branch for isolation
    ///
    /// # Arguments
    /// * `test_repo` - The test repository to pass to the operation
    /// * `operation` - A closure that takes a &TestRepo and returns a BenchmarkResult
    ///
    /// # Returns
    /// A `BenchmarkSampleResult` containing averaged statistics about the benchmark results
    ///
    /// # Example
    /// ```ignore
    /// let sampler = Sampler::new(5);
    /// let result = sampler.sample(test_repo, |repo| {
    ///     repo.benchmark_git(&["log", "--oneline", "-n", "100"])
    ///         .expect("log should succeed")
    /// });
    /// result.print_summary("git log (100 commits)");
    /// ```
    pub fn sample<F>(&self, test_repo: &TestRepo, operation: F) -> BenchmarkSampleResult
    where
        F: Fn(&TestRepo) -> BenchmarkResult,
    {
        self.sample_with_setup(
            test_repo,
            |repo| {
                // Optimized setup: Since each test commits its changes, the working tree
                // is clean after each run. We just need to get back to the default branch.

                let setup_start = Instant::now();

                // 1. Get the default branch (fast - just reading refs)
                let default_branch = repo
                    .git_og(&["symbolic-ref", "refs/remotes/origin/HEAD"])
                    .ok()
                    .and_then(|output| {
                        output
                            .trim()
                            .strip_prefix("refs/remotes/origin/")
                            .map(|b| b.to_string())
                    })
                    .unwrap_or_else(|| {
                        if repo.git_og(&["rev-parse", "--verify", "main"]).is_ok() {
                            "main".to_string()
                        } else {
                            "master".to_string()
                        }
                    });

                // 2. Get current branch name to delete it later (if it's a test branch)
                let current_branch = repo
                    .git_og(&["branch", "--show-current"])
                    .ok()
                    .map(|s| s.trim().to_string());

                // 3. Checkout default branch with force to discard any uncommitted changes
                repo.git_og(&["checkout", "-f", &default_branch])
                    .unwrap_or_else(|_| panic!("Checkout {} should succeed", default_branch));

                // 4. Delete the old test branch if it was a test-bench branch
                if let Some(branch) = current_branch
                    && branch.starts_with("test-bench/")
                {
                    let _ = repo.git_og(&["branch", "-D", &branch]);
                }

                // 5. Create a new branch with timestamp for isolation
                let timestamp_nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_nanos();
                let branch_name = format!("test-bench/{}", timestamp_nanos);
                repo.git_og(&["checkout", "-b", &branch_name])
                    .expect("Create branch should succeed");

                println!("Time taken to setup: {:?}", setup_start.elapsed());
            },
            operation,
        )
    }

    /// Sample a benchmark operation over multiple runs with a setup function
    /// that runs before each benchmark but is not included in timing
    ///
    /// # Arguments
    /// * `test_repo` - The test repository to pass to the operation
    /// * `setup` - A closure that runs before each benchmark (not timed)
    /// * `operation` - A closure that takes a &TestRepo and returns a BenchmarkResult
    ///
    /// # Returns
    /// A `BenchmarkSampleResult` containing averaged statistics about the benchmark results
    ///
    /// # Example
    /// ```ignore
    /// let sampler = Sampler::new(5);
    /// let result = sampler.sample_with_setup(
    ///     test_repo,
    ///     |repo| {
    ///         // Setup code that runs before each benchmark (not timed)
    ///         repo.git(&["reset", "--hard"]).expect("reset should succeed");
    ///     },
    ///     |repo| {
    ///         // The actual benchmark
    ///         repo.benchmark_git(&["log", "--oneline", "-n", "100"])
    ///             .expect("log should succeed")
    ///     }
    /// );
    /// result.print_summary("git log (100 commits)");
    /// ```
    pub fn sample_with_setup<S, F>(
        &self,
        test_repo: &TestRepo,
        setup: S,
        operation: F,
    ) -> BenchmarkSampleResult
    where
        S: Fn(&TestRepo),
        F: Fn(&TestRepo) -> BenchmarkResult,
    {
        let mut results = Vec::with_capacity(self.num_runs);

        for _i in 0..self.num_runs {
            // Run setup before each benchmark (not timed)
            setup(test_repo);

            // Run the actual benchmark
            let benchmark_result = operation(test_repo);
            results.push(benchmark_result);
        }

        // Calculate averages for each duration field
        let total_total: Duration = results.iter().map(|r| r.total_duration).sum();
        let total_git: Duration = results.iter().map(|r| r.git_duration).sum();
        let total_pre: Duration = results.iter().map(|r| r.pre_command_duration).sum();
        let total_post: Duration = results.iter().map(|r| r.post_command_duration).sum();

        let average = BenchmarkResult {
            total_duration: total_total / self.num_runs as u32,
            git_duration: total_git / self.num_runs as u32,
            pre_command_duration: total_pre / self.num_runs as u32,
            post_command_duration: total_post / self.num_runs as u32,
        };

        // Find min and max based on total_duration
        let min = results
            .iter()
            .min_by_key(|r| r.total_duration)
            .unwrap()
            .clone();
        let max = results
            .iter()
            .max_by_key(|r| r.total_duration)
            .unwrap()
            .clone();

        BenchmarkSampleResult {
            num_runs: self.num_runs,
            average,
            min,
            max,
            results,
        }
    }
}
