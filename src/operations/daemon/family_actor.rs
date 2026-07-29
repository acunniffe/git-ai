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
    Duration::from_millis(500),
    Duration::from_millis(1_000),
    Duration::from_millis(2_000),
];

fn should_retry_ref_enrichment(cmd: &NormalizedCommand) -> bool {
    cmd.trace_derived
        && cmd.exit_code == 0
        && matches!(
            cmd.primary_command.as_deref(),
            Some("commit") | Some("cherry-pick")
        )
        && command_can_move_head(cmd)
        && !has_head_ref_change(cmd)
}

fn command_can_move_head(cmd: &NormalizedCommand) -> bool {
    match cmd.primary_command.as_deref() {
        Some("commit") => !has_command_arg(cmd, "--dry-run"),
        Some("cherry-pick") => !["--abort", "--no-commit", "--quit", "-n"]
            .iter()
            .any(|arg| has_command_arg(cmd, arg)),
        _ => false,
    }
}

fn has_command_arg(cmd: &NormalizedCommand, expected: &str) -> bool {
    cmd.raw_argv.iter().any(|arg| arg == expected)
        || cmd.invoked_args.iter().any(|arg| arg == expected)
}

fn has_head_ref_change(cmd: &NormalizedCommand) -> bool {
    cmd.ref_changes
        .iter()
        .any(|change| change.reference == "HEAD")
}

async fn enrich_command_with_retries(
    ref_cursor: &mut RefCursor,
    cmd: &mut NormalizedCommand,
    state: &FamilyState,
) -> Result<HashMap<String, String>, GitAiError> {
    let mut command_start_refs = ref_cursor.enrich_command(cmd, state)?;

    // Git emits trace2 asynchronously, and the daemon can reach this actor
    // before a commit-like command's reflog append is visible to the reader.
    // A successful commit or cherry-pick is exact to retry: the matcher still
    // requires the command's own HEAD reflog transition and command metadata,
    // and the family actor has not reduced the command yet. A branch-only
    // match is still incomplete because history analysis needs HEAD. Never
    // broaden this into a live-HEAD guess.
    if should_retry_ref_enrichment(cmd) {
        for delay in COMMIT_REF_ENRICHMENT_RETRY_DELAYS {
            tokio::time::sleep(*delay).await;
            command_start_refs = ref_cursor.enrich_command(cmd, state)?;
            if has_head_ref_change(cmd) {
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
                    let result = enrich_command_with_retries(&mut ref_cursor, &mut cmd, &state)
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
#[path = "family_actor_tests.rs"]
mod tests;
