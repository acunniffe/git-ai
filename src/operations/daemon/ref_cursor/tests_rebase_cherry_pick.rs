use super::tests_fixtures::*;
use super::*;
use crate::model::domain::FamilyKey;
use std::fs;

#[test]
fn failed_explicit_branch_rebase_consumes_noop_start_marker_before_continue() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "rebase (pick): stale rebase from another branch"),
            (C, C, "rebase (finish): returning to refs/heads/stale-topic"),
            (A, A, "rebase (start): checkout master"),
            (A, E, "rebase (continue): Topic"),
            (E, E, "rebase (finish): returning to refs/heads/topic"),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic",
        &[
            (A, D, "commit: Topic"),
            (D, E, "rebase (finish): refs/heads/topic onto main"),
        ],
    );

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut failed = command_with_worktree(
        &family,
        Some(worktree.clone()),
        &["rebase", "master", "topic"],
    );
    failed.exit_code = 1;

    cursor.enrich_command(&mut failed, &state).unwrap();

    assert_eq!(
        failed.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: A.to_string(),
                new: A.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic".to_string(),
                old: D.to_string(),
                new: D.to_string(),
            },
        ]
    );

    let mut continued = command_with_worktree(&family, Some(worktree), &["rebase", "--continue"]);

    cursor.enrich_command(&mut continued, &state).unwrap();

    assert_eq!(
        continued.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: A.to_string(),
                new: E.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic".to_string(),
                old: D.to_string(),
                new: E.to_string(),
            },
        ]
    );
}

#[test]
fn cold_rebase_late_ingress_offset_still_recovers_start_and_branch_finish() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let start_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n");
    let pick_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\trebase (pick): Local commit\n");
    let finish_line = format!(
        "{D} {D} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
    );
    fs::write(
        git_dir.join("logs/HEAD"),
        format!("{start_line}{pick_line}{finish_line}"),
    )
    .unwrap();
    let late_head_offset = start_line.len() as u64;

    let branch_line = format!(
        "{B} {D} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
    );
    fs::write(git_dir.join("logs/refs/heads/topic"), &branch_line).unwrap();
    let late_branch_offset = branch_line.len() as u64;

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/topic".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), late_head_offset);
    cmd.reflog_start_offsets
        .insert(common_key("refs/heads/topic"), late_branch_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic".to_string(),
                old: B.to_string(),
                new: D.to_string(),
            },
        ],
        "cold late rebase enrichment must preserve the non-fast-forward local-tip to rebased-tip pair"
    );
}

#[test]
fn cold_rebase_true_boundary_does_not_replay_older_rebase_span() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let old_start =
        format!("{A} {B} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n");
    let old_pick =
        format!("{B} {C} Test User <test@example.com> 0 +0000\trebase (pick): Old commit\n");
    let old_finish = format!(
        "{C} {C} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
    );
    let current_start =
        format!("{D} {E} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n");
    let current_pick =
        format!("{E} {F} Test User <test@example.com> 0 +0000\trebase (pick): Current commit\n");
    let current_finish = format!(
        "{F} {F} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
    );
    let true_head_boundary = (old_start.len() + old_pick.len() + old_finish.len()) as u64;
    fs::write(
        git_dir.join("logs/HEAD"),
        format!("{old_start}{old_pick}{old_finish}{current_start}{current_pick}{current_finish}"),
    )
    .unwrap();

    let old_branch = format!(
        "{A} {C} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
    );
    let current_branch = format!(
        "{D} {F} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
    );
    let true_branch_boundary = old_branch.len() as u64;
    fs::write(
        git_dir.join("logs/refs/heads/topic"),
        format!("{old_branch}{current_branch}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), D.to_string());
    state
        .refs
        .insert("refs/heads/topic".to_string(), D.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), true_head_boundary);
    cmd.reflog_start_offsets
        .insert(common_key("refs/heads/topic"), true_branch_boundary);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: D.to_string(),
                new: E.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: E.to_string(),
                new: F.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic".to_string(),
                old: D.to_string(),
                new: F.to_string(),
            },
        ],
        "true command-start boundary must not rewind into an older rebase span"
    );
}

#[test]
fn rebase_span_stops_before_later_rebase_after_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "rebase (start): checkout topic-1"),
            (C, D, "rebase (pick): Topic 2"),
            (D, E, "checkout: moving from topic-2 to topic-3"),
            (E, F, "rebase (start): checkout topic-2"),
            (F, G, "rebase (pick): Topic 3"),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic-2",
        &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic-3",
        &[(E, G, "rebase (finish): refs/heads/topic-3 onto topic-2")],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic-2".to_string(),
                old: B.to_string(),
                new: D.to_string(),
            },
        ]
    );
}

#[test]
fn rebase_does_not_attach_unrelated_branch_with_same_new_tip() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "rebase (start): checkout topic-1"),
            (C, D, "rebase (pick): Topic 2"),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic-2",
        &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
    );
    append_reflog(
        &git_dir,
        "refs/heads/unrelated",
        &[(E, D, "rebase (finish): refs/heads/unrelated onto topic-1")],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic-2".to_string(),
                old: B.to_string(),
                new: D.to_string(),
            },
        ]
    );
}

#[test]
fn rebase_prefers_start_entry_when_expected_state_matches_pick() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "rebase (start): checkout topic-2"),
            (C, D, "rebase (pick): Topic 3"),
            (D, D, "rebase (finish): returning to refs/heads/topic-3"),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic-3",
        &[(B, D, "rebase (finish): refs/heads/topic-3 onto topic-2")],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/topic-2".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-2"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            },
            RefChange {
                reference: "refs/heads/topic-3".to_string(),
                old: B.to_string(),
                new: D.to_string(),
            },
        ]
    );
}

#[test]
fn cherry_pick_span_starts_at_first_pick_when_expected_state_matches_second_pick() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "cherry-pick: Pick one"),
            (C, D, "cherry-pick: Pick two"),
        ],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/intermediate".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor.pending_cherry_pick_source_oids = vec![A.to_string(), E.to_string()];
    let mut cmd = command_with_worktree(&family, Some(worktree), &["cherry-pick", "--continue"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            },
        ]
    );
}

#[test]
fn cherry_pick_source_named_like_command_is_not_dropped() {
    let family = FamilyKey::new("/repo/.git");
    let mut state = family_state(&family);
    state.refs.insert("cherry-pick".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["cherry-pick", "cherry-pick"]);
    cmd.invoked_args = vec!["cherry-pick".to_string()];

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(cmd.cherry_pick_source_oids, vec![B.to_string()]);
}

#[test]
fn revert_source_named_like_command_is_not_dropped() {
    let family = FamilyKey::new("/repo/.git");
    let mut state = family_state(&family);
    state.refs.insert("revert".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["revert", "revert"]);
    cmd.invoked_args = vec!["revert".to_string()];

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(cmd.revert_source_oids, vec![C.to_string()]);
}
