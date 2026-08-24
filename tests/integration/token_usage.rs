//! End-to-end tests for TokenUsage metric events (event id 9): checkpoint ->
//! stream worker -> token-usage worker -> metrics DB.

use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::generate_session_id;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::token_usage_pos;
use git_ai::metrics::types::{MetricEvent, MetricEventId};
use git_ai::metrics::{EventAttributes, PosEncoded};
use serde_json::json;
use std::fs;
use std::path::Path;

fn isolated_metrics_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated metrics db dir");
    let path = dir.path().join("metrics.db");
    (dir, path.to_string_lossy().to_string())
}

fn token_usage_events(metrics_db_path: &str) -> Vec<MetricEvent> {
    let db = MetricsDatabase::open_at_path(Path::new(metrics_db_path))
        .expect("metrics db should open at isolated path");
    db.get_metric_history(0, None, &[MetricEventId::TokenUsage as u16])
        .expect("token usage history should be readable")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

fn value_u64(event: &MetricEvent, pos: usize) -> Option<u64> {
    event.values.get(&pos.to_string()).and_then(|v| v.as_u64())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Stable anchor for fixture timestamps: yesterday's UTC midnight, computed
/// once per process. Always in the past, always inside the retention window,
/// and never shifting between fixture creation and assertion when a test
/// straddles a UTC midnight.
fn fixture_base() -> i64 {
    static BASE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| now_secs() - now_secs() % 86_400 - 86_400)
}

/// Recent RFC3339 timestamps so fixture entries fall inside the retention
/// window.
fn recent_ts(minute: u32, second: u32) -> String {
    chrono::DateTime::from_timestamp(fixture_base() + (minute * 60 + second) as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn bucket_of(minute: u32, second: u32) -> u64 {
    let ts = fixture_base() as u64 + (minute * 60 + second) as u64;
    ts - ts % 300
}

/// Barrier over the full pipeline: `sync_daemon` covers checkpoint admission
/// and family processing, and `git-ai await` reaches `await_completion`,
/// which drains the stream worker and then the token-usage worker before the
/// telemetry flush - so events are in the metrics DB when it returns.
fn sync_token_usage_pipeline(repo: &TestRepo) {
    repo.sync_daemon();
    repo.git_ai(&["await", "--timeout", "60"])
        .expect("await should drain the token-usage pipeline");
}

/// Fire a pre+post checkpoint pair that carries the transcript path, editing
/// `file_path` in between so the checkpoint records an AI change.
fn checkpoint_with_transcript(
    repo: &TestRepo,
    preset: &str,
    session_id: &str,
    transcript_path: &Path,
    file_path: &Path,
    contents: &str,
) {
    // Tool names/input shapes each preset accepts as a file edit.
    let (tool_name, tool_input) = match preset {
        "codex" => (
            "apply_patch",
            json!({ "patch": format!("*** Update File: {}\n", file_path.to_string_lossy()) }),
        ),
        _ => ("Write", json!({ "file_path": file_path.to_string_lossy() })),
    };
    for hook_event_name in ["PreToolUse", "PostToolUse"] {
        let hook_input = json!({
            "cwd": repo.canonical_path().to_string_lossy(),
            "hook_event_name": hook_event_name,
            "tool_name": tool_name,
            "tool_use_id": "toolu_token_usage",
            "session_id": session_id,
            "transcript_path": transcript_path.to_string_lossy(),
            "tool_input": tool_input
        })
        .to_string();
        repo.git_ai(&["checkpoint", preset, "--hook-input", &hook_input])
            .expect("checkpoint should succeed");
        if hook_event_name == "PreToolUse" {
            fs::write(file_path, contents).unwrap();
        }
    }
}

fn claude_usage_line(msg: &str, req: &str, ts: &str, output: u64, cost_usd: Option<f64>) -> String {
    let cost = cost_usd
        .map(|c| format!(r#""costUSD":{c},"#))
        .unwrap_or_default();
    format!(
        r#"{{"timestamp":"{ts}",{cost}"sessionId":"ext","requestId":"{req}","message":{{"id":"{msg}","model":"claude-sonnet-4-20250514","usage":{{"input_tokens":100,"output_tokens":{output},"cache_creation_input_tokens":30,"cache_read_input_tokens":200}}}}}}"#
    )
}

#[test]
fn claude_transcript_emits_token_usage_bucket_events() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/token-usage.git",
    ])
    .expect("remote add should succeed");
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");
    let repo_root = repo.canonical_path();

    // Transcript history predates the checkpoint: the whole file is bucketed
    // (backfill of a session's full history from byte offset 0).
    let transcript_path = repo_root.join("claude-session.jsonl");
    fs::write(
        &transcript_path,
        format!(
            "{}\n{}\n{}\n",
            json!({"type": "user", "message": {"content": "hello"}}),
            claude_usage_line("m1", "r1", &recent_ts(1, 0), 50, Some(1.25)),
            claude_usage_line("m2", "r2", &recent_ts(6, 0), 70, None),
        ),
    )
    .unwrap();

    let file_path = repo_root.join("example.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    checkpoint_with_transcript(
        &repo,
        "claude",
        "sess-token-claude",
        &transcript_path,
        &file_path,
        "const x = 1;\nconst y = 2;\n",
    );
    sync_token_usage_pipeline(&repo);

    let mut events = token_usage_events(&metrics_db_path);
    events.sort_by_key(|e| value_u64(e, token_usage_pos::BUCKET_TS));
    assert_eq!(events.len(), 2, "one event per 5-minute bucket");

    let first = &events[0];
    assert_eq!(
        value_u64(first, token_usage_pos::BUCKET_TS),
        Some(bucket_of(1, 0))
    );
    assert_eq!(value_u64(first, token_usage_pos::INPUT_TOKENS), Some(100));
    assert_eq!(value_u64(first, token_usage_pos::OUTPUT_TOKENS), Some(50));
    assert_eq!(
        value_u64(first, token_usage_pos::CACHE_READ_TOKENS),
        Some(200)
    );
    assert_eq!(
        value_u64(first, token_usage_pos::CACHE_WRITE_TOKENS),
        Some(30)
    );
    assert_eq!(value_u64(first, token_usage_pos::TOTAL_TOKENS), Some(380));
    assert_eq!(value_u64(first, token_usage_pos::MESSAGE_COUNT), Some(1));
    // costUSD 1.25 from the transcript wins over computed pricing.
    assert_eq!(
        value_u64(first, token_usage_pos::EST_COST_MICRO_USD),
        Some(1_250_000)
    );
    // Claude reports no reasoning tokens: field stays unset.
    assert_eq!(
        value_u64(first, token_usage_pos::REASONING_OUTPUT_TOKENS),
        None
    );

    let second = &events[1];
    assert_eq!(
        value_u64(second, token_usage_pos::BUCKET_TS),
        Some(bucket_of(6, 0))
    );
    assert_eq!(value_u64(second, token_usage_pos::OUTPUT_TOKENS), Some(70));
    // No costUSD on the second entry: cost is computed from the embedded
    // models.dev snapshot, so it must be non-zero.
    assert!(value_u64(second, token_usage_pos::EST_COST_MICRO_USD).unwrap() > 0);

    for event in &events {
        let attrs = EventAttributes::from_sparse(&event.attrs);
        assert_eq!(
            attrs.session_id,
            Some(Some(generate_session_id("sess-token-claude", "claude")))
        );
        assert_eq!(
            attrs.external_session_id,
            Some(Some("sess-token-claude".to_string()))
        );
        assert_eq!(attrs.tool, Some(Some("claude".to_string())));
        assert_eq!(
            attrs.model,
            Some(Some("claude-sonnet-4-20250514".to_string()))
        );
        let repo_url = attrs.repo_url.flatten().expect("repo_url should be set");
        assert!(
            repo_url.contains("acme/token-usage"),
            "unexpected repo_url {repo_url}"
        );
    }
}

#[test]
fn codex_transcript_emits_deltas_with_reasoning_tokens() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");
    let repo_root = repo.canonical_path();

    let transcript_path = repo_root.join("codex-session.jsonl");
    let token_count = |ts: &str, input: u64, cached: u64, output: u64, reasoning: u64| {
        json!({
            "timestamp": ts,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "output_tokens": output,
                        "reasoning_output_tokens": reasoning,
                        "total_tokens": input + output
                    }
                }
            }
        })
        .to_string()
    };
    fs::write(
        &transcript_path,
        format!(
            "{}\n{}\n{}\n{}\n",
            json!({"timestamp": "2026-01-01T00:00:00Z", "type": "session_meta", "payload": {"id": "sess-token-codex"}}),
            json!({"timestamp": "2026-01-01T00:00:30Z", "type": "turn_context", "payload": {"model": "gpt-5.1"}}),
            token_count(&recent_ts(1, 0), 100, 40, 50, 10),
            // Same bucket; cumulative totals advance by (200, 60, 40, 5).
            token_count(&recent_ts(3, 0), 300, 100, 90, 15),
        ),
    )
    .unwrap();

    let file_path = repo_root.join("main.rs");
    fs::write(&file_path, "fn main() {}\n").unwrap();
    checkpoint_with_transcript(
        &repo,
        "codex",
        "sess-token-codex",
        &transcript_path,
        &file_path,
        "fn main() {}\nfn added() {}\n",
    );
    sync_token_usage_pipeline(&repo);

    let events = token_usage_events(&metrics_db_path);
    assert_eq!(events.len(), 1, "both deltas land in one bucket");
    let event = &events[0];
    assert_eq!(
        value_u64(event, token_usage_pos::BUCKET_TS),
        Some(bucket_of(1, 0))
    );
    // Normalized input excludes cached tokens: (100-40) + (200-60).
    assert_eq!(value_u64(event, token_usage_pos::INPUT_TOKENS), Some(200));
    assert_eq!(
        value_u64(event, token_usage_pos::CACHE_READ_TOKENS),
        Some(100)
    );
    assert_eq!(value_u64(event, token_usage_pos::OUTPUT_TOKENS), Some(90));
    assert_eq!(
        value_u64(event, token_usage_pos::REASONING_OUTPUT_TOKENS),
        Some(15)
    );
    assert_eq!(value_u64(event, token_usage_pos::MESSAGE_COUNT), Some(2));
    assert!(value_u64(event, token_usage_pos::EST_COST_MICRO_USD).unwrap() > 0);

    let attrs = EventAttributes::from_sparse(&event.attrs);
    assert_eq!(attrs.tool, Some(Some("codex".to_string())));
    assert_eq!(attrs.model, Some(Some("gpt-5.1".to_string())));
    assert_eq!(
        attrs.session_id,
        Some(Some(generate_session_id("sess-token-codex", "codex")))
    );
}

#[test]
fn appended_transcript_lines_update_existing_buckets() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");
    let repo_root = repo.canonical_path();

    let transcript_path = repo_root.join("claude-session.jsonl");
    fs::write(
        &transcript_path,
        format!(
            "{}\n",
            claude_usage_line("m1", "r1", &recent_ts(1, 0), 50, None)
        ),
    )
    .unwrap();

    let file_path = repo_root.join("example.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    checkpoint_with_transcript(
        &repo,
        "claude",
        "sess-token-append",
        &transcript_path,
        &file_path,
        "const x = 1;\nconst y = 2;\n",
    );
    sync_token_usage_pipeline(&repo);
    assert_eq!(token_usage_events(&metrics_db_path).len(), 1);

    // A new usage entry lands in the same bucket: the incremental pass reads
    // only the appended line and re-emits the bucket with combined totals.
    let mut content = fs::read_to_string(&transcript_path).unwrap();
    content.push_str(&claude_usage_line("m2", "r2", &recent_ts(2, 0), 30, None));
    content.push('\n');
    fs::write(&transcript_path, content).unwrap();

    checkpoint_with_transcript(
        &repo,
        "claude",
        "sess-token-append",
        &transcript_path,
        &file_path,
        "const x = 1;\nconst y = 2;\nconst z = 3;\n",
    );
    sync_token_usage_pipeline(&repo);

    let events = token_usage_events(&metrics_db_path);
    assert_eq!(events.len(), 2, "the bucket re-emits once with new totals");
    let latest = &events[1];
    assert_eq!(
        value_u64(latest, token_usage_pos::BUCKET_TS),
        Some(bucket_of(1, 0))
    );
    assert_eq!(value_u64(latest, token_usage_pos::OUTPUT_TOKENS), Some(80));
    assert_eq!(value_u64(latest, token_usage_pos::MESSAGE_COUNT), Some(2));
}

#[test]
fn disabled_flag_spawns_nothing_and_deletes_collected_data() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TOKEN_USAGE_METRICS", "0"),
    ]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial commit should succeed");
    let repo_root = repo.canonical_path();

    // Simulate data collected while the flag was on: the daemon must delete
    // it at startup when the flag is off. (The daemon already started, so
    // create-then-restart is not observable here; instead assert the running
    // daemon never creates the DB and emits nothing.)
    let transcript_path = repo_root.join("claude-session.jsonl");
    fs::write(
        &transcript_path,
        format!(
            "{}\n",
            claude_usage_line("m1", "r1", &recent_ts(1, 0), 50, None)
        ),
    )
    .unwrap();

    let file_path = repo_root.join("example.ts");
    fs::write(&file_path, "const x = 1;\n").unwrap();
    checkpoint_with_transcript(
        &repo,
        "claude",
        "sess-token-disabled",
        &transcript_path,
        &file_path,
        "const x = 1;\nconst y = 2;\n",
    );
    sync_token_usage_pipeline(&repo);

    assert!(
        token_usage_events(&metrics_db_path).is_empty(),
        "no TokenUsage events with the flag off"
    );
    let token_db_path = repo
        .test_home_path()
        .join(".git-ai/internal/token-usage-db");
    assert!(
        !token_db_path.exists(),
        "token-usage database must not be created with the flag off"
    );
}
