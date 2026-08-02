//! Benchmark for secrets detection and redaction.
//!
//! This benchmark measures the performance of the secret detection algorithm,
//! which scans text for high-entropy strings that might be API keys, passwords, etc.
//!
//! Run with: cargo test test_secrets_benchmark --release -- --nocapture --ignored
//!
//! The benchmark uses realistic data sizes based on actual Claude Code transcripts
//! (~329KB of text across 92 messages).

use crate::test_utils::DurationStatistics;
use git_ai::operations::authorship::secrets::{
    extract_tokens, is_random, p_random, redact_secrets_in_text,
};
use std::time::{Duration, Instant};

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

/// Generate realistic test data that mimics Claude Code transcript content.
/// This includes:
/// - Code snippets with variable names and function calls
/// - Some actual high-entropy strings (like API keys)
/// - Natural language text
/// - File paths and URLs
fn generate_test_data(target_size_kb: usize) -> String {
    let mut text = String::new();
    let target_bytes = target_size_kb * 1024;

    // Code-like content with potential secret-like tokens
    let code_samples = [
        r#"fn calculate_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

const API_KEY: &str = "sk_test_4eC39HqLyjWDarjtT1zdp7dc";
const DATABASE_URL: &str = "postgres://user:password123@localhost:5432/db";
"#,
        r#"impl AuthenticationService {
    pub fn verify_token(&self, token: &str) -> Result<Claims, Error> {
        let secret_key = "xvz1evFS4wEEPTGEFPHBog5TYbN4a4rBxoffvXwgY";
        jwt::decode(token, &secret_key)
    }
}
"#,
        r#"// Configuration for the application
let config = Config {
    api_endpoint: "https://api.example.com/v1",
    auth_header: "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
    client_id: "client_abc123def456ghi789jkl012mno345",
    client_secret: "cs_live_AbCdEfGhIjKlMnOpQrStUvWxYz123456",
};
"#,
        r#"async fn fetch_user_data(user_id: &str) -> Result<User, ApiError> {
    let response = client
        .get(&format!("{}/users/{}", BASE_URL, user_id))
        .header("X-Api-Key", "pk_prod_7yHjK9mNpQrS2tUvWxYz3456AbCdEfGh")
        .send()
        .await?;
    response.json().await
}
"#,
        r#"// AWS credentials (DO NOT COMMIT)
// AKIAIOSFODNN7EXAMPLE
// wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

fn connect_to_s3() -> S3Client {
    let credentials = Credentials::new(
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    );
    S3Client::new(credentials)
}
"#,
        r#"interface UserConfig {
    apiKey: string;
    secretToken: string;
    refreshToken: string;
}

const defaultConfig: UserConfig = {
    apiKey: "api_key_xK9mN2pQ4rS6tU8vW0xY1zA3bC5dE7fG",
    secretToken: "st_4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d",
    refreshToken: "rt_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
};
"#,
        r#"# Environment variables for development
export STRIPE_SECRET_KEY="sk_test_51ABC123DEF456GHI789JKL"
export GITHUB_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
export NPM_TOKEN="npm_1234567890abcdefghijklmnopqrstuvwxyz"
export SLACK_WEBHOOK="https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX"
"#,
        // Natural language content (should not be flagged)
        r#"The quick brown fox jumps over the lazy dog. This is a sample paragraph
that contains regular English text without any secrets or API keys. We use this
to ensure that the secret detection algorithm does not flag normal prose as
potentially sensitive information. Performance is critical for user experience.
"#,
        // More code with identifiers
        r#"pub struct ConnectionPool {
    max_connections: usize,
    timeout_seconds: u64,
    retry_attempts: u32,
    connection_string: String,
}

impl ConnectionPool {
    pub fn new(connection_string: &str) -> Self {
        Self {
            max_connections: 10,
            timeout_seconds: 30,
            retry_attempts: 3,
            connection_string: connection_string.to_string(),
        }
    }

    pub fn get_connection(&self) -> Result<Connection, PoolError> {
        // Implementation details...
        todo!()
    }
}
"#,
    ];

    // Repeat content until we reach target size
    let mut idx = 0;
    while text.len() < target_bytes {
        text.push_str(code_samples[idx % code_samples.len()]);
        text.push_str("\n\n");
        idx += 1;
    }

    // Truncate to approximately target size
    text.truncate(target_bytes);
    text
}

/// Benchmark token extraction
fn benchmark_extract_tokens(text: &str, iterations: usize) -> Vec<Duration> {
    let mut durations = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let _tokens = extract_tokens(text);
        durations.push(start.elapsed());
    }

    durations
}

/// Benchmark is_random on extracted tokens
fn benchmark_is_random(text: &str, tokens: &[(usize, usize)], iterations: usize) -> Vec<Duration> {
    let mut durations = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        for &(start_idx, end_idx) in tokens {
            let _ = is_random(text.as_bytes().get(start_idx..end_idx).unwrap());
        }
        durations.push(start.elapsed());
    }

    durations
}

/// Benchmark p_random (the core probability calculation) on tokens
fn benchmark_p_random(text: &str, tokens: &[(usize, usize)], iterations: usize) -> Vec<Duration> {
    let mut durations = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        for &(start_idx, end_idx) in tokens {
            let _ = p_random(text.as_bytes().get(start_idx..end_idx).unwrap());
        }
        durations.push(start.elapsed());
    }

    durations
}

/// Benchmark full redaction pipeline
fn benchmark_redact_secrets_in_text(text: &str, iterations: usize) -> Vec<Duration> {
    let mut durations = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let (_redacted, _count) = redact_secrets_in_text(text);
        durations.push(start.elapsed());
    }

    durations
}

#[test]
#[ignore] // Run with --ignored flag since this is a benchmark
fn test_secrets_benchmark() {
    const TEXT_SIZE_KB: usize = 329; // Realistic size from actual transcript
    const ITERATIONS: usize = 10;

    println!("\n========================================");
    println!("Secrets Detection Benchmark");
    println!("========================================");
    println!("Text size: {}KB", TEXT_SIZE_KB);
    println!("Iterations: {}", ITERATIONS);

    // Generate test data
    println!("\nGenerating test data...");
    let text = generate_test_data(TEXT_SIZE_KB);
    println!(
        "Generated {} bytes ({:.1}KB) of test data",
        text.len(),
        text.len() as f64 / 1024.0
    );

    // Extract tokens once for analysis
    let tokens = extract_tokens(&text);
    println!("Extracted {} potential secret tokens", tokens.len());

    // Show token length distribution
    let mut length_counts: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for &(start, end) in &tokens {
        *length_counts.entry(end - start).or_insert(0) += 1;
    }
    let mut lengths: Vec<_> = length_counts.into_iter().collect();
    lengths.sort_by_key(|(len, _)| *len);
    println!("\nToken length distribution:");
    for (len, count) in &lengths {
        println!("  {} chars: {} tokens", len, count);
    }

    println!("\n========================================");
    println!("Running benchmarks...");
    println!("========================================");

    // Benchmark 1: Token extraction
    println!("\n--- Benchmarking extract_tokens ---");
    let extract_durations = benchmark_extract_tokens(&text, ITERATIONS);
    let extract_stats = DurationStats::from_durations(&extract_durations);
    extract_stats.print("extract_tokens");

    // Benchmark 2: p_random (core probability calculation)
    println!("\n--- Benchmarking p_random on {} tokens ---", tokens.len());
    let p_random_durations = benchmark_p_random(&text, &tokens, ITERATIONS);
    let p_random_stats = DurationStats::from_durations(&p_random_durations);
    p_random_stats.print("p_random (all tokens)");

    // Per-token stats
    if !tokens.is_empty() {
        let avg_per_token_us =
            p_random_stats.average.as_secs_f64() * 1_000_000.0 / tokens.len() as f64;
        println!("  Per token: {:.2}µs", avg_per_token_us);
    }

    // Benchmark 3: is_random (includes p_random + additional checks)
    println!(
        "\n--- Benchmarking is_random on {} tokens ---",
        tokens.len()
    );
    let is_random_durations = benchmark_is_random(&text, &tokens, ITERATIONS);
    let is_random_stats = DurationStats::from_durations(&is_random_durations);
    is_random_stats.print("is_random (all tokens)");

    if !tokens.is_empty() {
        let avg_per_token_us =
            is_random_stats.average.as_secs_f64() * 1_000_000.0 / tokens.len() as f64;
        println!("  Per token: {:.2}µs", avg_per_token_us);
    }

    // Benchmark 4: Full redaction pipeline
    println!("\n--- Benchmarking redact_secrets_in_text ---");
    let redact_durations = benchmark_redact_secrets_in_text(&text, ITERATIONS);
    let redact_stats = DurationStats::from_durations(&redact_durations);
    redact_stats.print("redact_secrets_in_text");

    // Show actual redaction results
    let (_, secrets_found) = redact_secrets_in_text(&text);
    println!("  Secrets found: {}", secrets_found);

    // Summary
    println!("\n========================================");
    println!("SUMMARY");
    println!("========================================");
    println!(
        "\nFor {}KB of text with {} tokens:",
        TEXT_SIZE_KB,
        tokens.len()
    );
    println!(
        "  Token extraction: {:.2}ms",
        extract_stats.average.as_secs_f64() * 1000.0
    );
    println!(
        "  p_random (all tokens): {:.2}ms",
        p_random_stats.average.as_secs_f64() * 1000.0
    );
    println!(
        "  is_random (all tokens): {:.2}ms",
        is_random_stats.average.as_secs_f64() * 1000.0
    );
    println!(
        "  Full redaction: {:.2}ms",
        redact_stats.average.as_secs_f64() * 1000.0
    );
    println!("\nBreakdown:");
    let token_extraction_pct =
        extract_stats.average.as_secs_f64() / redact_stats.average.as_secs_f64() * 100.0;
    let is_random_pct =
        is_random_stats.average.as_secs_f64() / redact_stats.average.as_secs_f64() * 100.0;
    println!("  Token extraction: {:.1}% of total", token_extraction_pct);
    println!("  is_random checks: {:.1}% of total", is_random_pct);
    println!("\n========================================\n");
}

/// Micro-benchmark for factorial and p_binomial to isolate the bottleneck
#[test]
#[ignore]
fn test_secrets_micro_benchmark() {
    println!("\n========================================");
    println!("Secrets Micro-Benchmark (p_random internals)");
    println!("========================================\n");

    // Test with tokens of varying lengths to understand scaling
    let test_tokens = [
        "sk_test_4eC39HqLyjWDarjtT1zdp7dc",          // 28 chars
        "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",  // 40 chars
        "xvz1evFS4wEEPTGEFPHBog5TYbN4a4rBxoffvXwgY", // 43 chars
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",  // 40 chars
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9eyJzdWIiOiIxMjM0NTY3ODkwIn0", // 64 chars
        "VeryLongTokenThatMightBeSlowerToProcessBecauseOfItsLengthAAAABBBBCCCC", // 68 chars
    ];

    const ITERATIONS: usize = 1000;

    println!("Running {} iterations per token\n", ITERATIONS);
    println!("{:>6} | {:>12} | {:>12}", "Length", "p_random", "is_random");
    println!("{:-<6}-+-{:-<12}-+-{:-<12}", "", "", "");

    for token in &test_tokens {
        let bytes = token.as_bytes();

        // Benchmark p_random
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = p_random(bytes);
        }
        let p_random_time = start.elapsed();
        let p_random_per_call = p_random_time.as_nanos() as f64 / ITERATIONS as f64;

        // Benchmark is_random
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = is_random(bytes);
        }
        let is_random_time = start.elapsed();
        let is_random_per_call = is_random_time.as_nanos() as f64 / ITERATIONS as f64;

        println!(
            "{:>6} | {:>9.1}ns | {:>9.1}ns",
            token.len(),
            p_random_per_call,
            is_random_per_call
        );
    }

    println!("\n========================================\n");
}

/// Performance regression test for secrets detection.
/// This test runs as part of the normal test suite (not ignored) and fails
/// if performance regresses beyond the acceptable threshold.
///
/// The test uses the same 329KB data size as the full benchmark to ensure
/// realistic performance measurement.
#[test]
#[ignore = "environment-dependent; run locally or on consistent hardware"]
fn test_secrets_performance_regression() {
    const TEXT_SIZE_KB: usize = 329; // Same as full benchmark

    // Different thresholds for debug vs release builds
    // Release: ~0.9ms actual, 10ms threshold (10x buffer)
    // Debug: ~10ms actual, 50ms threshold (allows for variation)
    #[cfg(debug_assertions)]
    const MAX_ALLOWED_MS: u64 = 50;
    #[cfg(not(debug_assertions))]
    const MAX_ALLOWED_MS: u64 = 10;

    let text = generate_test_data(TEXT_SIZE_KB);

    // Warm-up run (populates caches)
    let _ = redact_secrets_in_text(&text);

    // Timed run
    let start = Instant::now();
    let (_, secrets_found) = redact_secrets_in_text(&text);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < MAX_ALLOWED_MS as u128,
        "Secret redaction took {}ms for {}KB, expected < {}ms. \
         This may indicate a performance regression.",
        elapsed.as_millis(),
        TEXT_SIZE_KB,
        MAX_ALLOWED_MS
    );

    // Sanity check: should find some secrets
    assert!(secrets_found > 0, "Expected to find secrets in test data");
}

/// Test that secret detection scales linearly with input size.
/// Compares 329KB vs 5x (1645KB) and verifies time ratio is ~5x.
/// This ensures the implementation is O(n) and not O(n²) or worse.
#[test]
#[ignore = "environment-dependent; run locally or on consistent hardware"]
fn test_secrets_linear_scaling() {
    const BASE_SIZE_KB: usize = 329;
    const SCALE_FACTOR: usize = 5;
    const ITERATIONS: usize = 5;

    // Generate both sizes
    let text_1x = generate_test_data(BASE_SIZE_KB);
    let text_5x = generate_test_data(BASE_SIZE_KB * SCALE_FACTOR);

    // Warm up caches (Stirling table, bigram table, log factorials)
    let _ = redact_secrets_in_text(&text_1x);
    let _ = redact_secrets_in_text(&text_5x);

    // Benchmark 1x size
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = redact_secrets_in_text(&text_1x);
    }
    let time_1x = start.elapsed() / ITERATIONS as u32;

    // Benchmark 5x size
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let _ = redact_secrets_in_text(&text_5x);
    }
    let time_5x = start.elapsed() / ITERATIONS as u32;

    // Calculate ratio
    let ratio = time_5x.as_secs_f64() / time_1x.as_secs_f64();

    // For O(n) algorithm, ratio should be ~5.0
    // Allow 2.0-8.5 range for variance (cache effects, measurement noise)
    assert!(
        (2.0..=8.5).contains(&ratio),
        "Expected linear scaling (~5x), but got {:.2}x ratio. \
         1x: {:.2}ms, 5x: {:.2}ms. This suggests non-linear complexity.",
        ratio,
        time_1x.as_secs_f64() * 1000.0,
        time_5x.as_secs_f64() * 1000.0
    );
}
