use crate::daemon::analyzers::AnalyzerRegistry;
use crate::daemon::domain::{
    AppliedCommand, ApplyAck, FamilyKey, FamilyState, FamilyStatus, NormalizedCommand,
    WatermarkState,
};
use crate::daemon::reducer;
use crate::daemon::ref_cursor::RefCursor;
use crate::error::GitAiError;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

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
    pub async fn apply(&self, cmd: NormalizedCommand) -> Result<AppliedCommand, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(FamilyMsg::Apply(Box::new(cmd), tx))
            .await
            .map_err(|_| GitAiError::Generic("family actor apply send failed".to_string()))?;
        rx.await
            .map_err(|_| GitAiError::Generic("family actor apply receive failed".to_string()))?
    }

    pub async fn apply_checkpoint(&self) -> Result<ApplyAck, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(FamilyMsg::ApplyCheckpoint(tx))
            .await
            .map_err(|_| GitAiError::Generic("family actor checkpoint send failed".to_string()))?;
        rx.await.map_err(|_| {
            GitAiError::Generic("family actor checkpoint receive failed".to_string())
        })?
    }

    pub async fn status(&self) -> Result<FamilyStatus, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(FamilyMsg::Status(tx))
            .await
            .map_err(|_| GitAiError::Generic("family actor status send failed".to_string()))?;
        rx.await
            .map_err(|_| GitAiError::Generic("family actor status receive failed".to_string()))?
    }

    pub async fn watermarks(&self) -> Result<WatermarkState, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(FamilyMsg::GetWatermarks(tx))
            .await
            .map_err(|_| GitAiError::Generic("family actor watermarks send failed".to_string()))?;
        rx.await.map_err(|_| {
            GitAiError::Generic("family actor watermarks receive failed".to_string())
        })?
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
        crate::daemon::storage_sampler::register_ai_dir(
            std::path::PathBuf::from(&family_key.0).join("ai"),
        );

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
                    let result = ref_cursor.enrich_command(&mut cmd, &state).and_then(
                        |command_start_refs| {
                            reducer::reduce_family_command_with_ref_snapshot(
                                &mut state,
                                cmd,
                                &analyzers,
                                &command_start_refs,
                            )
                            .map(|(applied, _)| applied)
                        },
                    );
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
    use crate::daemon::domain::{CommandScope, Confidence, NormalizedCommand};
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
    async fn actor_registers_family_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let family_dir = tmp.path().join(".git");
        let ai_dir = family_dir.join("ai");
        let actor = spawn_family_actor(FamilyKey::new(family_dir.to_string_lossy()));

        for _ in 0..100 {
            if crate::daemon::storage_sampler::is_ai_dir_registered(&ai_dir) {
                actor.shutdown().await.unwrap();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        actor.shutdown().await.unwrap();
        panic!("family storage was not registered");
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
