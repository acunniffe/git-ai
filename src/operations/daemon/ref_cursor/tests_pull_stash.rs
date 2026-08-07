use super::tests_fixtures::*;
use super::*;
use crate::model::domain::FamilyKey;
use std::fs;

#[test]
fn revert_span_starts_at_first_revert_when_expected_state_matches_second_revert() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[(B, C, "revert: Revert one"), (C, D, "revert: Revert two")],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/intermediate".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["revert", A, E]);
    let expected = ExpectedTransition::from_state_and_working_logs(&cmd, &state);

    cursor
        .consume_head_span_for_command_limited(&mut cmd, &state, &["revert:"], expected, 2)
        .unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![ref_change("HEAD", B, C), ref_change("HEAD", C, D)]
    );
}

#[test]
fn pull_reflog_action_uses_expanded_command_for_zero_arg_alias() {
    let family = FamilyKey::new("/repo/.git".to_string());
    let mut cmd = command_with_worktree(&family, None, &["up"]);
    cmd.primary_command = Some("pull".to_string());
    cmd.invoked_command = Some("pull".to_string());
    cmd.invoked_args.clear();

    assert_eq!(pull_reflog_action(&cmd), "pull");
}

#[test]
fn pull_rebase_span_starts_at_start_entry_when_expected_state_matches_pick() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (
                B,
                C,
                "pull --rebase origin main (start): checkout origin/main",
            ),
            (C, D, "pull --rebase origin main (pick): Local commit"),
            (
                D,
                D,
                "pull --rebase origin main (finish): returning to refs/heads/main",
            ),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/main",
        &[(
            B,
            D,
            "pull --rebase origin main (finish): refs/heads/main onto origin/main",
        )],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/main".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(
        &family,
        Some(worktree),
        &["pull", "--rebase", "origin", "main"],
    );

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            ref_change("HEAD", B, C),
            ref_change("HEAD", C, D),
            ref_change("refs/heads/main", B, D),
        ]
    );
}

#[test]
fn cold_pull_rebase_late_ingress_offset_still_recovers_start_and_branch_finish() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let start_line = format!(
        "{B} {C} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout {C}\n"
    );
    let pick_line = format!(
        "{C} {D} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Local commit\n"
    );
    let finish_line = format!(
        "{D} {D} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
    );
    fs::write(
        git_dir.join("logs/HEAD"),
        format!("{start_line}{pick_line}{finish_line}"),
    )
    .unwrap();
    let late_head_offset = start_line.len() as u64;

    let branch_line = format!(
        "{B} {D} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto {C}\n"
    );
    fs::write(git_dir.join("logs/refs/heads/main"), &branch_line).unwrap();
    let late_branch_offset = branch_line.len() as u64;

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/main".to_string(), C.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["pull", "--rebase"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), late_head_offset);
    cmd.reflog_start_offsets
        .insert(common_key("refs/heads/main"), late_branch_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            ref_change("HEAD", B, C),
            ref_change("HEAD", C, D),
            ref_change("refs/heads/main", B, D),
        ],
        "cold late pull-rebase enrichment must preserve the non-fast-forward local-tip to rebased-tip pair"
    );
}

#[test]
fn cold_pull_rebase_true_boundary_does_not_replay_older_pull_span() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let old_start = format!(
        "{A} {B} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout main\n"
    );
    let old_pick =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Old commit\n");
    let old_finish = format!(
        "{C} {C} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
    );
    let current_start = format!(
        "{D} {E} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout main\n"
    );
    let current_pick = format!(
        "{E} {F} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Current commit\n"
    );
    let current_finish = format!(
        "{F} {F} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
    );
    let true_head_boundary = (old_start.len() + old_pick.len() + old_finish.len()) as u64;
    fs::write(
        git_dir.join("logs/HEAD"),
        format!("{old_start}{old_pick}{old_finish}{current_start}{current_pick}{current_finish}"),
    )
    .unwrap();

    let old_branch = format!(
        "{A} {C} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto main\n"
    );
    let current_branch = format!(
        "{D} {F} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto main\n"
    );
    let true_branch_boundary = old_branch.len() as u64;
    fs::write(
        git_dir.join("logs/refs/heads/main"),
        format!("{old_branch}{current_branch}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), D.to_string());
    state
        .refs
        .insert("refs/heads/main".to_string(), D.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["pull", "--rebase"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), true_head_boundary);
    cmd.reflog_start_offsets
        .insert(common_key("refs/heads/main"), true_branch_boundary);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            ref_change("HEAD", D, E),
            ref_change("HEAD", E, F),
            ref_change("refs/heads/main", D, F),
        ],
        "true command-start boundary must not rewind into an older pull-rebase span"
    );
}

#[test]
fn cold_stash_push_uses_message_to_skip_raw_stash_history() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(
        temp.path(),
        "refs/stash",
        &[
            (A, B, "On main: old raw stash"),
            (B, C, "On main: current ai stash"),
        ],
    );
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["stash", "push", "-m", "current ai stash"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(cmd.ref_changes, vec![ref_change("refs/stash", B, C)]);
    assert_eq!(cursor.stash_stack, vec![C.to_string()]);
}

#[test]
fn cold_stash_push_uses_command_reflog_boundary_without_message() {
    let temp = tempfile::tempdir().unwrap();
    let old_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tWIP on main\n");
    let old_history_len = old_line.len() as u64;
    let current_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tWIP on main\n");
    let path = temp.path().join("logs/refs/stash");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{old_line}{current_line}")).unwrap();
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["stash", "push"]);
    cmd.reflog_start_offsets
        .insert(common_key("refs/stash"), old_history_len);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(cmd.ref_changes, vec![ref_change("refs/stash", B, C)]);
    assert_eq!(cursor.stash_stack, vec![C.to_string()]);
}

#[test]
fn cold_stash_save_uses_message_to_skip_raw_stash_history() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(
        temp.path(),
        "refs/stash",
        &[
            (A, B, "On main: old raw stash"),
            (B, C, "On main: current save stash"),
        ],
    );
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["stash", "save", "current", "save", "stash"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(cmd.ref_changes, vec![ref_change("refs/stash", B, C)]);
    assert_eq!(cursor.stash_stack, vec![C.to_string()]);
}
