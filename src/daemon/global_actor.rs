use crate::daemon::analyzers::AnalyzerRegistry;
use crate::daemon::domain::{AppliedCommand, GlobalState, NormalizedCommand};
use crate::daemon::reducer;
use crate::error::GitAiError;
use tokio::sync::{mpsc, oneshot};

pub enum GlobalMsg {
    Apply(
        Box<NormalizedCommand>,
        oneshot::Sender<Result<AppliedCommand, GitAiError>>,
    ),
    Shutdown,
}

#[derive(Clone)]
pub struct GlobalActorHandle {
    tx: mpsc::Sender<GlobalMsg>,
}

impl GlobalActorHandle {
    pub async fn apply(&self, cmd: NormalizedCommand) -> Result<AppliedCommand, GitAiError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(GlobalMsg::Apply(Box::new(cmd), tx))
            .await
            .map_err(|_| GitAiError::Generic("global actor apply send failed".to_string()))?;
        rx.await
            .map_err(|_| GitAiError::Generic("global actor apply receive failed".to_string()))?
    }

    pub async fn shutdown(&self) -> Result<(), GitAiError> {
        self.tx
            .send(GlobalMsg::Shutdown)
            .await
            .map_err(|_| GitAiError::Generic("global actor shutdown send failed".to_string()))
    }
}

pub fn spawn_global_actor() -> GlobalActorHandle {
    let (tx, mut rx) = mpsc::channel::<GlobalMsg>(1024);
    let handle = GlobalActorHandle { tx };

    tokio::spawn(async move {
        let analyzers = AnalyzerRegistry::new();
        let mut state = GlobalState { applied_seq: 0 };

        while let Some(msg) = rx.recv().await {
            match msg {
                GlobalMsg::Apply(cmd, respond_to) => {
                    let result = reducer::reduce_global_command(&mut state, *cmd, &analyzers)
                        .map(|(applied, _)| applied);
                    let _ = respond_to.send(result);
                }
                GlobalMsg::Shutdown => break,
            }
        }
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::{CommandScope, Confidence, NormalizedCommand};

    fn global_cmd(seq: u128) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: format!("global-{}", seq),
            raw_argv: vec!["git".to_string(), "help".to_string()],
            primary_command: Some("help".to_string()),
            invoked_command: Some("help".to_string()),
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
    async fn global_actor_applies_commands() {
        let actor = spawn_global_actor();
        let ack = actor.apply(global_cmd(1)).await.unwrap();
        assert_eq!(ack.seq, 1);
        actor.shutdown().await.unwrap();
    }
}
