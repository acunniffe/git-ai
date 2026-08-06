use super::tests_fixtures::*;
use super::*;
use crate::model::domain::FamilyKey;
use std::fs;

#[test]
fn commit_subject_matches_git_reflog_trailing_whitespace_cleanup() {
    assert_eq!(commit_subject("subject \t"), Some("subject".to_string()));
    assert_eq!(
        commit_subject("\nsubject \t\nbody"),
        Some("subject".to_string())
    );
    assert_eq!(
        commit_subject("subject\u{00a0}"),
        Some("subject\u{00a0}".to_string())
    );

    let args = vec![
        "commit".to_string(),
        "-m".to_string(),
        "subject \t".to_string(),
    ];
    assert!(commit_reflog_messages(&args, false).contains("commit: subject"));
    assert!(commit_reflog_messages(&args, true).contains("commit (amend): subject"));
}

#[test]
fn cold_start_late_ingress_offset_does_not_skip_commit_on_uninitialized_head_cursor() {
    // Regression for the concurrent-burst / rebase-patch-stack flake. Unlike
    // the initialized-cursor case below, here the worktree HEAD cursor is
    // COLD (first traced commit on a fresh linked worktree — the worktree's
    // own HEAD reflog was never seeded by a prior command in this family).
    //
    // The async ingress offset is captured LATE: after git appended this
    // commit's HEAD entry, with a trailing entry after it (so the offset is
    // NOT at EOF). Cold-start seeding used `command_start_offset_is_authoritative`
    // -> records-exist-after-offset == true -> seed the cursor at the late
    // offset, positioning it PAST the commit's own entry. find_entry_in_log
    // then reads from the late cursor, never sees the commit -> empty
    // ref_changes -> head_change None -> no CommitCreated -> AI attribution
    // silently lost.
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    // A→B: the worktree's creation checkout (untraced by this family's cursor).
    // B→C: this command's commit. C→D: a trailing entry (e.g. a later op) so
    // the late offset lands mid-reflog rather than at EOF.
    let create_line =
        format!("{A} {B} Test User <test@example.com> 0 +0000\tcheckout: moving to wt\n");
    let commit_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: burst commit\n");
    let trailing_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcheckout: moving back\n");
    let late_offset = (create_line.len() + commit_line.len()) as u64;
    fs::write(
        &head_log,
        format!("{create_line}{commit_line}{trailing_line}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    // Cold: cursor NOT initialized.
    let mut cursor = RefCursor::new(family.clone());

    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "burst commit"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), late_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: B.to_string(),
            new: C.to_string(),
        }],
        "cold-start late ingress offset must not seed the cursor past the commit's own entry"
    );
}

#[test]
fn cold_start_late_ingress_offset_does_not_skip_commit_on_uninitialized_common_ref() {
    // Variant of the above for a `common:` branch ref (e.g. the branch a
    // linked worktree commits on). command_start_offset_is_authoritative
    // returns true UNCONDITIONALLY for cold `common:` keys, so a late offset
    // (captured after git appended the branch's commit entry) seeds the
    // branch-ref cursor at EOF, past the commit's branch entry. The commit's
    // branch transition is then never found.
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let reference = "refs/heads/feature";
    let head_log = git_dir.join("logs/HEAD");
    let branch_log = git_dir.join("logs").join(reference);
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    fs::create_dir_all(branch_log.parent().unwrap()).unwrap();

    // HEAD records the commit normally (B→C), found from offset 0.
    let head_commit_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: feature commit\n");
    fs::write(&head_log, &head_commit_line).unwrap();

    // The branch ref also records B→C for the commit. The late ingress offset
    // points at EOF (after the commit's branch entry).
    let branch_commit_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: feature commit\n");
    fs::write(&branch_log, &branch_commit_line).unwrap();
    let late_branch_offset = branch_commit_line.len() as u64;

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    state.refs.insert(reference.to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());

    let mut cmd =
        command_with_worktree(&family, Some(worktree), &["commit", "-m", "feature commit"]);
    cmd.reflog_start_offsets
        .insert(common_key(reference), late_branch_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    // The branch ref change must be present (not skipped by the late cold seed).
    assert!(
        cmd.ref_changes
            .iter()
            .any(|change| change.reference == reference && change.old == B && change.new == C),
        "cold-start late ingress offset on a common branch ref must not skip the commit's branch entry; got {:?}",
        cmd.ref_changes
    );
}

#[test]
fn late_ingress_offset_does_not_advance_in_order_cursor_past_own_commit() {
    // Regression for the graphite/gt-create flake. The family actor keeps one
    // RefCursor across commands. A prior command (e.g. a `switch`) advanced the
    // in-order HEAD cursor to exactly before this command's commit entry. The
    // async daemon-ingress offset capture then races and reads the reflog AFTER
    // git appended both the commit entry and a following switch entry, so the
    // `reflog_start_offsets` hint points PAST this commit's own entry.
    //
    // The in-order cursor is the authoritative floor; the late hint finds no
    // matching entry at/after it, so selection falls back to the cursor's first
    // match — this commit's own entry. The commit keeps its attribution.
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    // A→B: prior switch onto the new branch (already consumed; cursor sits at
    // end of this line). B→C: this command's commit. C→D: the switch-back gt
    // issues right after committing.
    let switch_line =
        format!("{A} {B} Test User <test@example.com> 0 +0000\tcheckout: moving to branch\n");
    let commit_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: gt create\n");
    let switch_back_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcheckout: moving back\n");
    let in_order_offset = switch_line.len() as u64;
    // Ingress captured the reflog late: after the commit entry was written but
    // before the switch-back, so the hint points just PAST this commit's entry
    // while a later record (the switch-back) still exists.
    let late_offset = (switch_line.len() + commit_line.len()) as u64;
    fs::write(
        &head_log,
        format!("{switch_line}{commit_line}{switch_back_line}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor
        .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
        .unwrap();

    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "gt create"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), late_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: B.to_string(),
            new: C.to_string(),
        }],
        "late ingress offset must not skip the commit's own entry"
    );
}

#[test]
fn ingress_offset_hint_skips_untraced_duplicate_message_commit() {
    // The dual of the graphite case: an UNTRACED commit sharing a message sits
    // between the in-order cursor and this command's own commit. Here the
    // ingress offset was captured at the command's true start — after the
    // untraced commit — so it correctly biases selection to the later (traced)
    // entry. The hint must be honored, not ignored.
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    // A→B: prior traced base (consumed; cursor at end). B→C: an untraced commit
    // the daemon never saw, sharing the message. C→D: this command's commit.
    let base_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
    let untraced_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let traced_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let in_order_offset = base_line.len() as u64;
    // Hint captured at the true command start: after the untraced commit.
    let hint_offset = (base_line.len() + untraced_line.len()) as u64;
    fs::write(
        &head_log,
        format!("{base_line}{untraced_line}{traced_line}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor
        .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
        .unwrap();

    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), hint_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: C.to_string(),
            new: D.to_string(),
        }],
        "ingress hint must skip the untraced duplicate-message commit"
    );
}

#[test]
fn late_ingress_offset_skips_untraced_duplicate_message_commit() {
    // Same duplicate-message shape as above, but the async ingress capture
    // raced behind git and observed the reflog after this command's commit
    // was already appended. With no candidate at/after the late hint, the
    // cursor must choose the latest matching entry before the hint rather
    // than the older untraced duplicate.
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let base_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
    let untraced_line =
        format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let traced_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let in_order_offset = base_line.len() as u64;
    let late_hint_offset = (base_line.len() + untraced_line.len() + traced_line.len()) as u64;
    fs::write(
        &head_log,
        format!("{base_line}{untraced_line}{traced_line}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor
        .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
        .unwrap();

    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), late_hint_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: C.to_string(),
            new: D.to_string(),
        }],
        "late ingress hint must select the traced duplicate-message commit"
    );
}

#[test]
fn amend_without_message_does_not_match_plain_commit_reflog_entry() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (A, B, "commit: older plain commit"),
            (B, C, "commit (amend): older plain commit"),
        ],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd =
        command_with_worktree(&family, Some(worktree), &["commit", "--amend", "--no-edit"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: B.to_string(),
            new: C.to_string(),
        }]
    );
}

#[test]
fn commit_with_exact_reflog_message_ignores_stale_daemon_head() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let initial_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: initial\n");
    let raw_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: raw unseen\n");
    let traced_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: traced after raw\n");
    fs::write(&head_log, format!("{initial_line}{raw_line}{traced_line}")).unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(
        &family,
        Some(worktree),
        &["commit", "-m", "traced after raw"],
    );

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: C.to_string(),
            new: D.to_string(),
        }]
    );
}

#[test]
fn commit_reflog_boundary_skips_untraced_duplicate_message() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let initial_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: initial\n");
    let raw_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let traced_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
    let start_offset = (initial_line.len() + raw_line.len()) as u64;
    fs::write(&head_log, format!("{initial_line}{raw_line}{traced_line}")).unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), start_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: C.to_string(),
            new: D.to_string(),
        }]
    );
}

#[test]
fn eng_216_commit_reflog_subject_rewrite_falls_back_to_hinted_entry() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);

    let base_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
    let untraced_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: untraced\n");
    let hooked_line =
        format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: hooked: raw subject\n");
    let in_order_offset = base_line.len() as u64;
    let hint_offset = (base_line.len() + untraced_line.len()) as u64;
    fs::write(
        &head_log,
        format!("{base_line}{untraced_line}{hooked_line}"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor
        .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
        .unwrap();

    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "raw subject"]);
    cmd.reflog_start_offsets
        .insert(head_key(&git_dir), hint_offset);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: C.to_string(),
            new: D.to_string(),
        }],
        "commit-msg hooks may rewrite the final subject used by Git's reflog"
    );
}

#[test]
fn eng_216_commit_reflog_subject_rewrite_fallback_does_not_guess_without_hint() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (A, B, "commit: untraced"),
            (B, C, "commit: hooked: raw subject"),
        ],
    );

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "raw subject"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert!(
        cmd.ref_changes.is_empty(),
        "multiple commit entries without a command-start hint remain ambiguous"
    );
}

#[test]
fn eng_216_commit_reflog_subject_rewrite_fallback_stays_in_command_time_window() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    fs::write(
        &head_log,
        format!("{A} {B} Test User <test@example.com> 10 +0000\tcommit: hooked: raw subject\n"),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["commit", "-m", "raw subject"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert!(
        cmd.ref_changes.is_empty(),
        "subject-rewrite fallback must reject entries outside the command's timestamp window"
    );
}
