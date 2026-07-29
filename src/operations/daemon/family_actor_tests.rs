use super::*;
use crate::model::domain::{CommandScope, Confidence, NormalizedCommand, RefChange};
use crate::operations::daemon::ref_cursor::capture_reflog_start_offsets_for_worktree;
use std::fs;
use std::path::PathBuf;

fn sample_normalized_cmd(family_key: &str, seq: u128) -> NormalizedCommand {
    NormalizedCommand {
        scope: CommandScope::Family(FamilyKey::new(family_key)),
        family_key: Some(FamilyKey::new(family_key)),
        worktree: Some(PathBuf::from("/tmp/repo")),
        root_sid: format!("sid-{}", seq),
        trace_derived: false,
        raw_argv: vec!["git".to_string(), "status".to_string()],
        primary_command: Some("status".to_string()),
        invoked_command: Some("status".to_string()),
        invoked_args: Vec::new(),
        observed_child_commands: Vec::new(),
        exit_code: 0,
        started_at_ns: seq,
        finished_at_ns: seq + 1,
        reflog_start_offsets: std::collections::HashMap::new(),
        stash_target_oid: None,
        cherry_pick_source_oids: Vec::new(),
        revert_source_oids: Vec::new(),
        ref_changes: Vec::new(),
        confidence: Confidence::Low,
    }
}

#[tokio::test]
async fn commit_enrichment_retries_until_reflog_entry_is_visible() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().to_path_buf();
    let head_log = worktree.join(".git/logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&worktree.join(".git"));
    fs::write(&head_log, "").unwrap();
    let reflog_start_offsets = capture_reflog_start_offsets_for_worktree(&worktree);

    let family = FamilyKey::new(worktree.to_string_lossy().to_string());
    let state = FamilyState {
        family_key: family.clone(),
        refs: HashMap::new(),
        worktrees: HashMap::new(),
        last_error: None,
        applied_seq: 0,
        watermarks: WatermarkState::default(),
    };
    let mut cmd = NormalizedCommand {
        scope: CommandScope::Family(family.clone()),
        family_key: Some(family.clone()),
        worktree: Some(worktree),
        root_sid: "delayed-reflog".to_string(),
        trace_derived: true,
        raw_argv: vec![
            "git".to_string(),
            "commit".to_string(),
            "-m".to_string(),
            "delayed".to_string(),
        ],
        primary_command: Some("commit".to_string()),
        invoked_command: Some("commit".to_string()),
        invoked_args: vec![
            "commit".to_string(),
            "-m".to_string(),
            "delayed".to_string(),
        ],
        observed_child_commands: Vec::new(),
        exit_code: 0,
        started_at_ns: 1,
        finished_at_ns: 2,
        reflog_start_offsets,
        stash_target_oid: None,
        cherry_pick_source_oids: Vec::new(),
        revert_source_oids: Vec::new(),
        ref_changes: Vec::new(),
        confidence: Confidence::Low,
    };

    let old = "1111111111111111111111111111111111111111";
    let new = "2222222222222222222222222222222222222222";
    let delayed_head_log = head_log.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        fs::write(
            delayed_head_log,
            format!("{old} {new} Test User <test@example.com> 0 +0000\tcommit: delayed\n"),
        )
        .unwrap();
    });

    let mut ref_cursor = RefCursor::new(family);
    enrich_command_with_retries(&mut ref_cursor, &mut cmd, &state)
        .await
        .unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: old.to_string(),
            new: new.to_string(),
        }]
    );
}

#[tokio::test]
async fn trace_cherry_pick_retries_without_initial_reflog_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().to_path_buf();
    let head_log = worktree.join(".git/logs/HEAD");
    fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&worktree.join(".git"));
    fs::write(&head_log, "").unwrap();

    let family = FamilyKey::new(worktree.to_string_lossy().to_string());
    let state = FamilyState {
        family_key: family.clone(),
        refs: HashMap::new(),
        worktrees: HashMap::new(),
        last_error: None,
        applied_seq: 0,
        watermarks: WatermarkState::default(),
    };
    let source = "2222222222222222222222222222222222222222";
    let old = "1111111111111111111111111111111111111111";
    let new = "3333333333333333333333333333333333333333";
    let mut cmd = NormalizedCommand {
        scope: CommandScope::Family(family.clone()),
        family_key: Some(family.clone()),
        worktree: Some(worktree),
        root_sid: "trace-cherry-pick".to_string(),
        trace_derived: true,
        raw_argv: vec![
            "git".to_string(),
            "cherry-pick".to_string(),
            source.to_string(),
        ],
        primary_command: Some("cherry-pick".to_string()),
        invoked_command: Some("cherry-pick".to_string()),
        invoked_args: vec!["cherry-pick".to_string(), source.to_string()],
        observed_child_commands: Vec::new(),
        exit_code: 0,
        started_at_ns: 1,
        finished_at_ns: 2,
        reflog_start_offsets: HashMap::new(),
        stash_target_oid: None,
        cherry_pick_source_oids: Vec::new(),
        revert_source_oids: Vec::new(),
        ref_changes: Vec::new(),
        confidence: Confidence::Low,
    };

    let delayed_head_log = head_log.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(
            delayed_head_log,
            format!("{old} {new} Test User <test@example.com> 0 +0000\tcherry-pick: source\n"),
        )
        .unwrap();
    });

    let mut ref_cursor = RefCursor::new(family);
    enrich_command_with_retries(&mut ref_cursor, &mut cmd, &state)
        .await
        .unwrap();

    assert_eq!(
        cmd.ref_changes,
        vec![RefChange {
            reference: "HEAD".to_string(),
            old: old.to_string(),
            new: new.to_string(),
        }]
    );
}

#[test]
fn ref_enrichment_retry_requires_successful_unenriched_commit_like_command() {
    let mut cmd = sample_normalized_cmd("family-1", 1);
    assert!(!should_retry_ref_enrichment(&cmd));

    cmd.primary_command = Some("commit".to_string());
    assert!(!should_retry_ref_enrichment(&cmd));

    cmd.trace_derived = true;
    assert!(should_retry_ref_enrichment(&cmd));

    cmd.reflog_start_offsets.insert("HEAD".to_string(), 0);
    assert!(should_retry_ref_enrichment(&cmd));

    cmd.primary_command = Some("cherry-pick".to_string());
    assert!(should_retry_ref_enrichment(&cmd));

    cmd.invoked_args = vec!["cherry-pick".to_string(), "--no-commit".to_string()];
    assert!(!should_retry_ref_enrichment(&cmd));
    cmd.invoked_args = vec!["cherry-pick".to_string()];

    cmd.ref_changes.push(RefChange {
        reference: "refs/heads/cherry-side".to_string(),
        old: "1111111111111111111111111111111111111111".to_string(),
        new: "2222222222222222222222222222222222222222".to_string(),
    });
    assert!(should_retry_ref_enrichment(&cmd));

    cmd.reflog_start_offsets.clear();
    assert!(should_retry_ref_enrichment(&cmd));

    cmd.exit_code = 1;
    assert!(!should_retry_ref_enrichment(&cmd));

    cmd.exit_code = 0;
    cmd.ref_changes.push(RefChange {
        reference: "HEAD".to_string(),
        old: "1111111111111111111111111111111111111111".to_string(),
        new: "2222222222222222222222222222222222222222".to_string(),
    });
    assert!(!should_retry_ref_enrichment(&cmd));
}

#[tokio::test]
async fn commit_enrichment_fails_closed_when_reflog_entry_stays_missing() {
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().to_path_buf();
    let family = FamilyKey::new(worktree.to_string_lossy().to_string());
    let state = FamilyState {
        family_key: family.clone(),
        refs: HashMap::new(),
        worktrees: HashMap::new(),
        last_error: None,
        applied_seq: 0,
        watermarks: WatermarkState::default(),
    };
    let mut cmd = sample_normalized_cmd(&family.to_string(), 1);
    cmd.scope = CommandScope::Family(family.clone());
    cmd.family_key = Some(family.clone());
    cmd.worktree = Some(worktree);
    cmd.primary_command = Some("commit".to_string());
    cmd.invoked_command = Some("commit".to_string());
    cmd.raw_argv = vec![
        "git".to_string(),
        "commit".to_string(),
        "-m".to_string(),
        "missing".to_string(),
    ];
    cmd.invoked_args = vec![
        "commit".to_string(),
        "-m".to_string(),
        "missing".to_string(),
    ];

    let mut ref_cursor = RefCursor::new(family);
    enrich_command_with_retries(&mut ref_cursor, &mut cmd, &state)
        .await
        .unwrap();

    assert!(cmd.ref_changes.is_empty());
}

#[tokio::test]
async fn actor_applies_commands() {
    let actor = spawn_family_actor(FamilyKey::new("family-1"));
    let ack1 = actor
        .apply(sample_normalized_cmd("family-1", 10))
        .await
        .unwrap();
    let ack2 = actor
        .apply(sample_normalized_cmd("family-1", 20))
        .await
        .unwrap();
    assert_eq!(ack1.seq, 1);
    assert_eq!(ack2.seq, 2);
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn actor_status_reports_applied_seq() {
    let actor = spawn_family_actor(FamilyKey::new("family-2"));
    actor
        .apply(sample_normalized_cmd("family-2", 1))
        .await
        .unwrap();
    let status = actor.status().await.unwrap();
    assert_eq!(status.applied_seq, 1);
    actor.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_watermarks_initially_empty() {
    let handle = spawn_family_actor(FamilyKey::new("test-family"));
    let watermarks = handle.watermarks().await.unwrap();
    assert!(watermarks.per_file.is_empty());
    assert!(watermarks.per_worktree.is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_watermarks_update_and_retrieve() {
    let handle = spawn_family_actor(FamilyKey::new("test-family"));

    let mut per_file = HashMap::new();
    per_file.insert("src/main.rs".to_string(), 1000_u128);
    per_file.insert("src/lib.rs".to_string(), 2000_u128);
    handle
        .update_watermarks(WatermarkState {
            per_file,
            per_worktree: HashMap::new(),
        })
        .await
        .unwrap();

    let wm = handle.watermarks().await.unwrap();
    assert_eq!(wm.per_file.get("src/main.rs"), Some(&1000));
    assert_eq!(wm.per_file.get("src/lib.rs"), Some(&2000));

    // Higher per-file mtime overwrites; lower does not
    let mut per_file2 = HashMap::new();
    per_file2.insert("src/main.rs".to_string(), 3000_u128);
    handle
        .update_watermarks(WatermarkState {
            per_file: per_file2,
            per_worktree: HashMap::new(),
        })
        .await
        .unwrap();

    let wm = handle.watermarks().await.unwrap();
    assert_eq!(wm.per_file.get("src/main.rs"), Some(&3000));
    assert_eq!(wm.per_file.get("src/lib.rs"), Some(&2000));

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_worktree_watermark_update_and_retrieve() {
    let handle = spawn_family_actor(FamilyKey::new("test-family"));

    let mut per_worktree = HashMap::new();
    per_worktree.insert("/repo".to_string(), 5000_u128);
    handle
        .update_watermarks(WatermarkState {
            per_file: HashMap::new(),
            per_worktree,
        })
        .await
        .unwrap();

    let wm = handle.watermarks().await.unwrap();
    assert_eq!(wm.per_worktree.get("/repo"), Some(&5000));

    // Monotonic: lower value does not overwrite
    let mut per_worktree2 = HashMap::new();
    per_worktree2.insert("/repo".to_string(), 1000_u128);
    handle
        .update_watermarks(WatermarkState {
            per_file: HashMap::new(),
            per_worktree: per_worktree2,
        })
        .await
        .unwrap();

    let wm = handle.watermarks().await.unwrap();
    assert_eq!(wm.per_worktree.get("/repo"), Some(&5000));

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_worktree_watermark_prunes_stale_per_file_entries() {
    let handle = spawn_family_actor(FamilyKey::new("test-family"));

    // Set per-file watermarks at various timestamps
    let mut per_file = HashMap::new();
    per_file.insert("src/old.rs".to_string(), 1000_u128); // will be pruned: 1000 <= 3000
    per_file.insert("src/also_old.rs".to_string(), 3000_u128); // at boundary: 3000 <= 3000, pruned
    per_file.insert("src/new.rs".to_string(), 5000_u128); // kept: 5000 > 3000
    handle
        .update_watermarks(WatermarkState {
            per_file,
            per_worktree: HashMap::new(),
        })
        .await
        .unwrap();

    // Advance worktree watermark to 3000
    let mut per_worktree = HashMap::new();
    per_worktree.insert("/repo".to_string(), 3000_u128);
    handle
        .update_watermarks(WatermarkState {
            per_file: HashMap::new(),
            per_worktree,
        })
        .await
        .unwrap();

    let wm = handle.watermarks().await.unwrap();
    // Entries at or before worktree_wm are pruned (they are superseded by the full checkpoint)
    assert!(
        !wm.per_file.contains_key("src/old.rs"),
        "old entry should be pruned"
    );
    assert!(
        !wm.per_file.contains_key("src/also_old.rs"),
        "boundary entry should be pruned"
    );
    // Entry newer than worktree_wm is preserved
    assert_eq!(wm.per_file.get("src/new.rs"), Some(&5000));

    handle.shutdown().await.unwrap();
}
