use crate::error::GitAiError;
use crate::model::domain::{
    AppliedCommand, ApplyAck, FamilyKey, FamilyState, FamilyStatus, NormalizedCommand,
    WatermarkState,
};
use crate::operations::daemon::analyzers::AnalyzerRegistry;
use crate::operations::daemon::reducer;
use crate::operations::daemon::ref_cursor::RefCursor;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const COMMIT_REF_ENRICHMENT_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(5),
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
];

fn should_retry_commit_ref_enrichment(cmd: &NormalizedCommand) -> bool {
    cmd.exit_code == 0
        && cmd.primary_command.as_deref() == Some("commit")
        && cmd.ref_changes.is_empty()
}

async fn enrich_command_with_commit_retries(
    ref_cursor: &mut RefCursor,
    cmd: &mut NormalizedCommand,
    state: &FamilyState,
) -> Result<HashMap<String, String>, GitAiError> {
    let mut command_start_refs = ref_cursor.enrich_command(cmd, state)?;

    // Git emits trace2 asynchronously, and the daemon can reach this actor
    // before the commit's reflog append is visible to the reader. A successful
    // commit is exact to retry: the matcher still requires the command's own
    // reflog transition and commit message, and the family actor has not
    // reduced the command yet. Never broaden this into a live-HEAD guess.
    if should_retry_commit_ref_enrichment(cmd) {
        for delay in COMMIT_REF_ENRICHMENT_RETRY_DELAYS {
            tokio::time::sleep(*delay).await;
            command_start_refs = ref_cursor.enrich_command(cmd, state)?;
            if !cmd.ref_changes.is_empty() {
                break;
            }
        }
    }

    Ok(command_start_refs)
}

pub enum FamilyMsg {
    Apply(
        Box<NormalizedCommand>,
        oneshot::Sender<Result<AppliedCommand, GitAiError>>,
    ),
    ApplyCheckpoint(oneshot::Sender<Result<ApplyAck, GitAiError>>),
    Status(oneshot::Sender<Result<FamilyStatus, GitAiError>>),
    GetWatermarks(oneshot::Sender<Result<WatermarkState, GitAiError>>),
    UpdateWatermarks(WatermarkState),
    Shutdown,
}

#[derive(Clone)]
pub struct FamilyActorHandle {
    pub family_key: FamilyKey,
    tx: mpsc::Sender<FamilyMsg>,
}

impl FamilyActorHandle {
    /// Send a request built by `make` and await its oneshot reply, wrapping
    /// both the send and receive failure paths in a `GitAiError::Generic`
    /// tagged with `op` (e.g. `"family actor apply send failed"`).
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, GitAiError>>) -> FamilyMsg,
        op: &'static str,
    ) -> Result<T, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| GitAiError::Generic(format!("family actor {op} send failed")))?;
        rx.await
            .map_err(|_| GitAiError::Generic(format!("family actor {op} receive failed")))?
    }

    pub async fn apply(&self, cmd: NormalizedCommand) -> Result<AppliedCommand, GitAiError> {
        self.request(|tx| FamilyMsg::Apply(Box::new(cmd), tx), "apply")
            .await
    }

    pub async fn apply_checkpoint(&self) -> Result<ApplyAck, GitAiError> {
        self.request(FamilyMsg::ApplyCheckpoint, "checkpoint").await
    }

    pub async fn status(&self) -> Result<FamilyStatus, GitAiError> {
        self.request(FamilyMsg::Status, "status").await
    }

    pub async fn watermarks(&self) -> Result<WatermarkState, GitAiError> {
        self.request(FamilyMsg::GetWatermarks, "watermarks").await
    }

    pub async fn update_watermarks(&self, update: WatermarkState) -> Result<(), GitAiError> {
        self.tx
            .send(FamilyMsg::UpdateWatermarks(update))
            .await
            .map_err(|_| {
                GitAiError::Generic("family actor update_watermarks send failed".to_string())
            })
    }

    pub async fn shutdown(&self) -> Result<(), GitAiError> {
        self.tx
            .send(FamilyMsg::Shutdown)
            .await
            .map_err(|_| GitAiError::Generic("family actor shutdown send failed".to_string()))
    }
}

pub fn spawn_family_actor(family_key: FamilyKey) -> FamilyActorHandle {
    let (tx, mut rx) = mpsc::channel::<FamilyMsg>(1024);
    let handle = FamilyActorHandle {
        family_key: family_key.clone(),
        tx,
    };

    tokio::spawn(async move {
        let analyzers = AnalyzerRegistry::new();
        let mut state = FamilyState {
            family_key: family_key.clone(),
            refs: HashMap::new(),
            worktrees: HashMap::new(),
            last_error: None,
            applied_seq: 0,
            watermarks: WatermarkState::default(),
        };
        let mut ref_cursor = RefCursor::new(family_key.clone());

        while let Some(msg) = rx.recv().await {
            match msg {
                FamilyMsg::Apply(cmd, respond_to) => {
                    let mut cmd = *cmd;
                    let canonical_worktree = cmd
                        .worktree
                        .as_deref()
                        .map(crate::operations::git::canonicalize::canonicalize_or_self);
                    let result =
                        enrich_command_with_commit_retries(&mut ref_cursor, &mut cmd, &state)
                            .await
                            .and_then(|command_start_refs| {
                                reducer::reduce_family_command_with_ref_snapshot(
                                    &mut state,
                                    cmd,
                                    &analyzers,
                                    &command_start_refs,
                                    canonical_worktree,
                                )
                                .map(|(applied, _)| applied)
                            });
                    let _ = respond_to.send(result);
                }
                FamilyMsg::ApplyCheckpoint(respond_to) => {
                    reducer::reduce_checkpoint(&mut state);
                    let _ = respond_to.send(Ok(ApplyAck {
                        seq: state.applied_seq,
                        applied: true,
                    }));
                }
                FamilyMsg::Status(respond_to) => {
                    let _ = respond_to.send(Ok(FamilyStatus {
                        family_key: state.family_key.clone(),
                        applied_seq: state.applied_seq,
                        last_error: state.last_error.clone(),
                    }));
                }
                FamilyMsg::GetWatermarks(respond_to) => {
                    let _ = respond_to.send(Ok(state.watermarks.clone()));
                }
                FamilyMsg::UpdateWatermarks(update) => {
                    for (path, mtime_ns) in update.per_file {
                        let entry = state.watermarks.per_file.entry(path).or_insert(0);
                        if mtime_ns > *entry {
                            *entry = mtime_ns;
                        }
                    }
                    for (worktree, ts) in update.per_worktree {
                        let entry = state.watermarks.per_worktree.entry(worktree).or_insert(0);
                        if ts > *entry {
                            *entry = ts;
                            // Prune per-file watermarks superseded by this worktree watermark.
                            // A per-file entry older than worktree_wm would cause Tier 1 false
                            // positives: the file would appear stale even though it was captured
                            // by the full human checkpoint at worktree_wm.
                            state.watermarks.per_file.retain(|_, file_ts| *file_ts > ts);
                        }
                    }
                }
                FamilyMsg::Shutdown => break,
            }
        }
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::domain::{CommandScope, Confidence, NormalizedCommand, RefChange};
    use std::fs;
    use std::path::PathBuf;

    fn sample_normalized_cmd(family_key: &str, seq: u128) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Family(FamilyKey::new(family_key)),
            family_key: Some(FamilyKey::new(family_key)),
            worktree: Some(PathBuf::from("/tmp/repo")),
            root_sid: format!("sid-{}", seq),
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
            reflog_start_offsets: HashMap::new(),
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
            tokio::time::sleep(Duration::from_millis(50)).await;
            fs::write(
                delayed_head_log,
                format!("{old} {new} Test User <test@example.com> 0 +0000\tcommit: delayed\n"),
            )
            .unwrap();
        });

        let mut ref_cursor = RefCursor::new(family);
        enrich_command_with_commit_retries(&mut ref_cursor, &mut cmd, &state)
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
    fn commit_enrichment_retry_requires_successful_unenriched_commit() {
        let mut cmd = sample_normalized_cmd("family-1", 1);
        assert!(!should_retry_commit_ref_enrichment(&cmd));

        cmd.primary_command = Some("commit".to_string());
        assert!(should_retry_commit_ref_enrichment(&cmd));

        cmd.exit_code = 1;
        assert!(!should_retry_commit_ref_enrichment(&cmd));

        cmd.exit_code = 0;
        cmd.ref_changes.push(RefChange {
            reference: "HEAD".to_string(),
            old: "1111111111111111111111111111111111111111".to_string(),
            new: "2222222222222222222222222222222222222222".to_string(),
        });
        assert!(!should_retry_commit_ref_enrichment(&cmd));
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
        enrich_command_with_commit_retries(&mut ref_cursor, &mut cmd, &state)
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
}
