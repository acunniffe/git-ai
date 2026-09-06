use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{
    codex_checkpoint, committed_metric_for_commit, isolated_metrics_db_path, sparse_str, sparse_u64,
};
use git_ai::metrics::events::committed_pos;
use std::fs;

fn looks_like_patch_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn committed_metric_includes_git_author_commit_timestamps_and_patch_id() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);

    let file_path = repo.path().join("generated.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial commit")
        .expect("initial commit should succeed");

    codex_checkpoint(
        &repo,
        &file_path,
        "metric-metadata-session",
        "PreToolUse",
        "tool-use-metric-metadata",
    );
    fs::write(&file_path, "base\nai line\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "metric-metadata-session",
        "PostToolUse",
        "tool-use-metric-metadata",
    );

    let commit = repo
        .stage_all_and_commit_with_env(
            "AI commit with deterministic dates",
            &[
                ("GIT_AUTHOR_DATE", "2030-01-03T00:00:00Z"),
                ("GIT_COMMITTER_DATE", "2030-01-03T00:00:42Z"),
            ],
        )
        .expect("AI commit should succeed");

    let expected_times = repo
        .git(&["show", "-s", "--format=%at%x00%ct", &commit.commit_sha])
        .expect("commit timestamps should be readable");
    let mut parts = expected_times.trim().split('\0');
    let expected_author_ts = parts
        .next()
        .expect("author ts")
        .parse::<u64>()
        .expect("author ts should parse");
    let expected_commit_ts = parts
        .next()
        .expect("commit ts")
        .parse::<u64>()
        .expect("commit ts should parse");

    let event = committed_metric_for_commit(&metrics_db_path, &commit.commit_sha);
    assert_eq!(
        sparse_u64(&event.values, committed_pos::AUTHOR_TS),
        Some(expected_author_ts)
    );
    assert_eq!(
        sparse_u64(&event.values, committed_pos::COMMIT_TS),
        Some(expected_commit_ts)
    );
    let patch_id = sparse_str(&event.values, committed_pos::PATCH_ID).expect("patch id");
    assert!(looks_like_patch_id(patch_id), "patch_id={patch_id}");
    assert_eq!(
        event.values.get(&committed_pos::COMMIT_SOURCE.to_string()),
        Some(&serde_json::Value::Null),
        "traced commits record a null commit_source"
    );

    let mut file = repo.filename("generated.txt");
    file.assert_committed_lines(lines!["base".unattributed_human(), "ai line".ai()]);
}
