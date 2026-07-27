use crate::repos::test_repo::TestRepo;
use git_ai::{daemon::DaemonConfig, diagnostic_sentinels::DEBUG_SELF_CHECK_DIR_NAME};

#[test]
fn attribution_self_checks_do_not_timeout() {
    let repo = TestRepo::new();
    let trace2_target =
        DaemonConfig::trace2_event_target_for_path(&repo.daemon_trace_socket_path());

    let report = repo
        .git_ai_with_env(
            &["debug", "--skip-trace2-checks"],
            &[
                ("GIT_TRACE2_EVENT", trace2_target.as_str()),
                ("GIT_TRACE2_EVENT_NESTING", "0"),
            ],
        )
        .expect("git-ai debug should complete");

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
