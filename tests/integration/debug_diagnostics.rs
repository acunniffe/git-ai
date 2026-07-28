use crate::repos::test_repo::{TestRepo, real_git_executable};
use git_ai::{daemon::DaemonConfig, diagnostic_sentinels::DEBUG_SELF_CHECK_DIR_NAME};
use std::fs;

#[test]
fn attribution_self_checks_do_not_timeout() {
    let repo = TestRepo::new();
    let slow_git = repo.test_home_path().join(if cfg!(windows) {
        "slow-git.exe"
    } else {
        "slow-git"
    });
    fs::copy(env!("CARGO_BIN_EXE_git-ai-test-git-shim"), &slow_git).unwrap();
    let config_path = repo.test_home_path().join(".git-ai").join("config.json");
    fs::write(
        config_path,
        serde_json::json!({
            "git_path": slow_git,
            "prompt_storage": "notes",
            "exclude_prompts_in_repositories": [],
            "disable_version_checks": true
        })
        .to_string(),
    )
    .unwrap();
    let trace2_target =
        DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());

    let report = repo
        .git_ai_with_env(
            &["debug", "--skip-trace2-checks"],
            &[
                ("GIT_TRACE2_EVENT", trace2_target.as_str()),
                ("GIT_TRACE2_EVENT_NESTING", "0"),
                ("GIT_AI_TEST_GIT_SHIM_TARGET", real_git_executable()),
                (
                    "GIT_AI_TEST_GIT_SHIM_FALLBACK_TARGET",
                    real_git_executable(),
                ),
                // Each Git command stays well below the three-second timeout,
                // but their cumulative delay exceeds the old shared deadline.
                ("GIT_AI_TEST_GIT_SHIM_DELAY_MS", "550"),
            ],
        )
        .expect("git-ai debug should complete");

    assert!(
        report.contains(&format!("configured git (program: {})", slow_git.display())),
        "configured self-check should use the delayed Git shim:\n{report}"
    );
    let passed_checks = report
        .lines()
        .filter(|line| line.contains("Attribution self-check: passed"))
        .count();
    assert_eq!(
        passed_checks, 2,
        "configured and terminal git attribution checks should pass:\n{report}"
    );
    let daemon_self_check_root = repo
        .daemon_home_path()
        .join(".git-ai")
        .join("internal")
        .join(DEBUG_SELF_CHECK_DIR_NAME);
    let expected_repo_prefix = format!("    repo: {}", daemon_self_check_root.display());
    assert_eq!(
        report.matches(&expected_repo_prefix).count(),
        2,
        "both self-check repositories should use the active daemon home:\n{report}"
    );
    for expected_line in [
        "line 1: untracked (expected untracked",
        "line 2: known_human (expected known_human",
        "line 3: ai (expected ai",
    ] {
        assert_eq!(
            report.matches(expected_line).count(),
            2,
            "both attribution checks should validate {expected_line}:\n{report}"
        );
    }
}
