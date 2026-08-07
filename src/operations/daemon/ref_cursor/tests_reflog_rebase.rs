use super::tests_fixtures::*;
use super::*;
use crate::model::domain::FamilyKey;
use std::fs;

fn reflog_line(old: &str, new: &str, message: &str) -> String {
    format!("{old} {new} Test User <test@example.com> 0 +0000\t{message}")
}

#[test]
fn common_ref_discovery_excludes_worktree_head_log() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(temp.path(), "HEAD", &[(A, B, "")]);
    append_reflog(temp.path(), "refs/heads/main", &[(A, B, "")]);
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let cursor = RefCursor::new(family);

    assert_eq!(
        cursor.discover_common_refs().unwrap(),
        vec!["refs/heads/main".to_string()]
    );
}

#[test]
fn reflog_reader_ignores_trailing_unterminated_record() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("HEAD");
    let complete = format!("{}\n", reflog_line(A, B, "commit: complete"));
    let partial = reflog_line(B, C, "commit: partial");
    fs::write(&path, format!("{complete}{partial}")).unwrap();

    let records = read_reflog_records(&path, None).unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].old, A);
    assert_eq!(records[0].new, B);

    fs::write(&path, format!("{complete}{partial}\n")).unwrap();
    let records = read_reflog_records(&path, None).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].old, B);
    assert_eq!(records[1].new, C);
}

#[test]
fn reflog_anchor_rejects_non_newline_end_offset() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("HEAD");
    let complete = format!("{}\n", reflog_line(A, B, "commit: complete"));
    let partial = reflog_line(B, C, "commit: partial");
    fs::write(&path, format!("{complete}{partial}")).unwrap();

    let complete_record = read_reflog_record_ending_at(&path, complete.len() as u64)
        .unwrap()
        .expect("complete newline-terminated record should be readable");
    assert_eq!(complete_record.old, A);
    assert_eq!(complete_record.new, B);

    let partial_end = (complete.len() + partial.len()) as u64;
    assert!(
        read_reflog_record_ending_at(&path, partial_end)
            .unwrap()
            .is_none(),
        "an offset inside an unterminated reflog record must not become an anchor"
    );
}

#[test]
fn rebase_span_stops_at_new_rebase_start_before_finish() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (B, C, "rebase (start): checkout main"),
            (C, D, "rebase (pick): First rebase commit"),
            (D, E, "rebase (start): checkout other"),
            (E, F, "rebase (pick): Later rebase commit"),
        ],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![ref_change("HEAD", B, C), ref_change("HEAD", C, D)]
    );
}

#[test]
fn rebase_span_continuation_skips_stale_abort_before_selected_start() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    let branch_log = git_dir.join("logs/refs/heads/feature");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    fs::create_dir_all(branch_log.parent().unwrap()).unwrap();

    let failed_start =
        format!("{B} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n");
    let stale_abort = format!(
        "{C} {B} Test User <test@example.com> 0 +0000\trebase (abort): returning to refs/heads/stale-topic\n"
    );
    let checkout_feature = format!(
        "{B} {A} Test User <test@example.com> 0 +0000\tcheckout: moving from stale-topic to feature\n"
    );
    let feature_commit =
        format!("{A} {D} Test User <test@example.com> 0 +0000\tcommit: feature ai\n");
    let rebase_start =
        format!("{D} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n");
    let rebase_pick =
        format!("{C} {E} Test User <test@example.com> 0 +0000\trebase (pick): feature ai\n");
    let rebase_finish = format!(
        "{E} {E} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/feature\n"
    );
    let failed_start_offset = failed_start.len() as u64;
    let abort_offset = failed_start_offset + stale_abort.len() as u64;
    let checkout_offset = abort_offset + checkout_feature.len() as u64;
    let commit_offset = checkout_offset + feature_commit.len() as u64;
    fs::write(
        &head_log,
        format!(
            "{failed_start}{stale_abort}{checkout_feature}{feature_commit}{rebase_start}{rebase_pick}{rebase_finish}"
        ),
    )
    .unwrap();

    fs::write(
        &branch_log,
        format!(
            "{ZERO} {A} Test User <test@example.com> 0 +0000\tbranch: Created from {A}\n\
             {A} {D} Test User <test@example.com> 0 +0000\tcommit: feature ai\n\
             {D} {E} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/feature onto main\n",
            ZERO = zero_oid(),
        ),
    )
    .unwrap();

    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let mut state = family_state(&family);
    state.refs.insert("HEAD".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/main".to_string(), C.to_string());
    state
        .refs
        .insert("refs/heads/stale-topic".to_string(), B.to_string());
    let mut cursor = RefCursor::new(family.clone());
    cursor
        .initialize_reflog_cursor(&head_key(&git_dir), failed_start_offset)
        .unwrap();

    let mut checkout = command_with_worktree(
        &family,
        Some(worktree.clone()),
        &["checkout", "-b", "feature", A],
    );
    checkout
        .reflog_start_offsets
        .insert(head_key(&git_dir), abort_offset);
    cursor.enrich_command(&mut checkout, &state).unwrap();
    for change in &checkout.ref_changes {
        state
            .refs
            .insert(change.reference.clone(), change.new.clone());
    }

    let mut commit = command_with_worktree(
        &family,
        Some(worktree.clone()),
        &["commit", "-m", "feature ai"],
    );
    commit
        .reflog_start_offsets
        .insert(head_key(&git_dir), checkout_offset);
    cursor.enrich_command(&mut commit, &state).unwrap();
    for change in &commit.ref_changes {
        state
            .refs
            .insert(change.reference.clone(), change.new.clone());
    }

    let mut rebase = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
    rebase
        .reflog_start_offsets
        .insert(head_key(&git_dir), commit_offset);
    cursor.enrich_command(&mut rebase, &state).unwrap();

    assert_eq!(
        rebase.ref_changes,
        vec![
            ref_change("HEAD", D, C),
            ref_change("HEAD", C, E),
            ref_change("refs/heads/feature", D, E),
        ],
        "rebase continuation must follow the selected start, not a stale untraced abort row before it"
    );
}

#[test]
fn skipped_reflog_entry_remains_available_for_later_sequenced_command() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(
        temp.path(),
        "refs/heads/main",
        &[
            (A, B, "ordered second command"),
            (B, C, "ordered first command"),
        ],
    );
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());

    let mut first = command(&family, &["update-ref", "refs/heads/main", C, B]);
    cursor.enrich_command(&mut first, &state).unwrap();
    assert_eq!(first.ref_changes, vec![ref_change("refs/heads/main", B, C)]);

    let mut second = command(&family, &["update-ref", "refs/heads/main", B, A]);
    cursor.enrich_command(&mut second, &state).unwrap();
    assert_eq!(
        second.ref_changes,
        vec![ref_change("refs/heads/main", A, B)]
    );
}

#[test]
fn reflog_generation_reset_with_same_byte_length_clears_sparse_consumption() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(
        temp.path(),
        "refs/heads/main",
        &[
            (A, B, "ordered second command"),
            (B, C, "ordered first command"),
        ],
    );
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());

    let mut first = command(&family, &["update-ref", "refs/heads/main", C, B]);
    cursor.enrich_command(&mut first, &state).unwrap();
    assert_eq!(first.ref_changes, vec![ref_change("refs/heads/main", B, C)]);

    let old_len = fs::metadata(temp.path().join("logs/refs/heads/main"))
        .unwrap()
        .len();
    append_reflog(
        temp.path(),
        "refs/heads/main",
        &[
            (A, B, "ordered second command"),
            (B, C, "ordered third command"),
        ],
    );
    assert_eq!(
        fs::metadata(temp.path().join("logs/refs/heads/main"))
            .unwrap()
            .len(),
        old_len
    );

    let mut second = command(&family, &["update-ref", "refs/heads/main", C, B]);
    cursor.enrich_command(&mut second, &state).unwrap();
    assert_eq!(
        second.ref_changes,
        vec![ref_change("refs/heads/main", B, C)]
    );
}

#[test]
fn update_ref_stdin_is_reconstructed_from_reflog_delta() {
    let temp = tempfile::tempdir().unwrap();
    append_reflog(temp.path(), "refs/heads/main", &[(A, B, "stdin update")]);
    append_reflog(temp.path(), "refs/heads/topic", &[(A, C, "stdin update")]);
    let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command(&family, &["update-ref", "--stdin"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();
    cmd.ref_changes
        .sort_by(|left, right| left.reference.cmp(&right.reference));

    assert_eq!(
        cmd.ref_changes,
        vec![
            ref_change("refs/heads/main", A, B),
            ref_change("refs/heads/topic", A, C),
        ]
    );
}

#[test]
fn rebase_does_not_consume_adjacent_checkout_head_entry() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    fs::create_dir_all(git_dir.join("logs")).unwrap();
    append_reflog(
        &git_dir,
        "HEAD",
        &[
            (A, B, "checkout: moving from topic-1 to topic-2"),
            (B, C, "rebase (start): checkout topic-1"),
            (C, D, "rebase (pick): Topic 2"),
        ],
    );
    append_reflog(
        &git_dir,
        "refs/heads/topic-2",
        &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
    );
    let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
    let state = family_state(&family);
    let mut cursor = RefCursor::new(family.clone());
    let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

    cursor.enrich_command(&mut cmd, &state).unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![
            ref_change("HEAD", B, C),
            ref_change("HEAD", C, D),
            ref_change("refs/heads/topic-2", B, D),
        ]
    );
}
