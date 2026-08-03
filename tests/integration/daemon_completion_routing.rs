//! Completion-log routing coverage that runs in the sharded Unix and Windows suites.

use crate::repos::test_repo::TestRepo;

#[test]
fn completion_logs_partition_families_and_preserve_append_order() {
    let repo_a = TestRepo::new();
    let repo_b = TestRepo::new();

    assert_ne!(repo_a.canonical_path(), repo_b.canonical_path());
    assert_eq!(repo_a.daemon_home_path(), repo_b.daemon_home_path());

    let baseline_a = repo_a.daemon_completion_entries().len();
    let baseline_b = repo_b.daemon_completion_entries().len();

    repo_a
        .git(&["commit", "--allow-empty", "-m", "family a first"])
        .expect("first family-a command should succeed");
    repo_b
        .git(&["commit", "--allow-empty", "-m", "family b first"])
        .expect("family-b command should succeed");
    repo_a
        .git(&["branch", "completion-log-second"])
        .expect("second family-a command should succeed");

    repo_a.sync_daemon_force();
    repo_b.sync_daemon_force();

    let entries_a = repo_a.daemon_completion_entries();
    let entries_b = repo_b.daemon_completion_entries();
    let appended_a = &entries_a[baseline_a..];
    let appended_b = &entries_b[baseline_b..];

    assert_eq!(appended_a.len(), 2);
    assert_eq!(appended_b.len(), 1);
    assert_eq!(
        appended_a
            .iter()
            .map(|entry| entry.primary_command.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("commit"), Some("branch")]
    );
    assert_eq!(
        appended_b
            .iter()
            .map(|entry| entry.primary_command.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("commit")]
    );
    assert!(appended_a.iter().chain(appended_b).all(|entry| {
        entry.sync_tracked
            && entry.status == "ok"
            && entry.error.is_none()
            && entry.test_sync_session.is_some()
    }));
}
