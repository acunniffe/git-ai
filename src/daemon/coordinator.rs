use crate::daemon::domain::{
    AppliedCommand, ApplyAck, CommandScope, FamilyKey, FamilyStatus, NormalizedCommand,
    WatermarkState,
};
use crate::daemon::family_actor::{FamilyActorHandle, spawn_family_actor};
use crate::daemon::git_backend::GitBackend;
use crate::daemon::global_actor::{GlobalActorHandle, spawn_global_actor};
use crate::error::GitAiError;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct Coordinator<B: GitBackend> {
    backend: Arc<B>,
    global: GlobalActorHandle,
    families: Mutex<HashMap<String, FamilyActorHandle>>,
}

impl<B: GitBackend> Coordinator<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            global: spawn_global_actor(),
            families: Mutex::new(HashMap::new()),
        }
    }

    pub async fn route_command(
        &self,
        cmd: NormalizedCommand,
    ) -> Result<AppliedCommand, GitAiError> {
        match &cmd.scope {
            CommandScope::Global => self.global.apply(cmd).await,
            CommandScope::Family(key) => {
                let actor = self.get_or_create_family_actor(key.clone()).await;
                actor.apply(cmd).await
            }
        }
    }

    pub async fn apply_checkpoint(&self, repo_working_dir: &Path) -> Result<ApplyAck, GitAiError> {
        let family = self.backend.resolve_family(repo_working_dir)?;
        let actor = self.get_or_create_family_actor(family).await;
        actor.apply_checkpoint().await
    }

    pub async fn watermarks_family(
        &self,
        repo_working_dir: &Path,
    ) -> Result<WatermarkState, GitAiError> {
        let family = self.backend.resolve_family(repo_working_dir)?;
        let actor = self.get_or_create_family_actor(family).await;
        actor.watermarks().await
    }

    pub async fn update_watermarks_family(
        &self,
        repo_working_dir: &Path,
        update: WatermarkState,
    ) -> Result<(), GitAiError> {
        let family = self.backend.resolve_family(repo_working_dir)?;
        let actor = self.get_or_create_family_actor(family).await;
        actor.update_watermarks(update).await
    }

    pub async fn status_family(&self, repo_working_dir: &Path) -> Result<FamilyStatus, GitAiError> {
        let family = self.backend.resolve_family(repo_working_dir)?;
        let actor = self.get_or_create_family_actor(family).await;
        actor.status().await
    }

    pub async fn shutdown(&self) -> Result<(), GitAiError> {
        let actors = {
            let map = self.families.lock().await;
            map.values().cloned().collect::<Vec<_>>()
        };
        for actor in actors {
            let _ = actor.shutdown().await;
        }
        self.global.shutdown().await
    }

    async fn get_or_create_family_actor(&self, family_key: FamilyKey) -> FamilyActorHandle {
        let mut map = self.families.lock().await;
        if let Some(existing) = map.get(&family_key.0) {
            return existing.clone();
        }
        let created = spawn_family_actor(family_key.clone());
        map.insert(family_key.0.clone(), created.clone());
        created
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::{CommandScope, Confidence, FamilyKey, NormalizedCommand};
    use crate::daemon::git_backend::GitBackend;
    use crate::git::cli_parser::parse_git_cli_args;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockBackend {
        families: Mutex<HashMap<String, FamilyKey>>,
    }

    impl MockBackend {
        fn with_family(self, worktree: &str, family_key: &str) -> Self {
            self.families
                .lock()
                .unwrap()
                .insert(worktree.to_string(), FamilyKey::new(family_key.to_string()));
            self
        }
    }

    impl GitBackend for MockBackend {
        fn resolve_family(&self, worktree: &Path) -> Result<FamilyKey, GitAiError> {
            self.families
                .lock()
                .unwrap()
                .get(worktree.to_string_lossy().as_ref())
                .cloned()
                .ok_or_else(|| GitAiError::Generic("family not found".to_string()))
        }

        fn resolve_primary_command(
            &self,
            _worktree: &Path,
            argv: &[String],
        ) -> Result<Option<String>, GitAiError> {
            let tokens: &[String] = if argv
                .first()
                .is_some_and(|value| value == "git" || value == "git.exe")
            {
                &argv[1..]
            } else {
                argv
            };
            Ok(parse_git_cli_args(tokens).command)
        }

        fn clone_target(&self, _argv: &[String], _cwd_hint: Option<&Path>) -> Option<PathBuf> {
            None
        }

        fn init_target(&self, _argv: &[String], _cwd_hint: Option<&Path>) -> Option<PathBuf> {
            None
        }
    }

    fn global_cmd() -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: "g1".to_string(),
            raw_argv: vec!["git".to_string(), "help".to_string()],
            primary_command: Some("help".to_string()),
            invoked_command: Some("help".to_string()),
            invoked_args: Vec::new(),
            observed_child_commands: Vec::new(),
            exit_code: 0,
            started_at_ns: 1,
            finished_at_ns: 2,
            reflog_start_offsets: std::collections::HashMap::new(),
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes: Vec::new(),
            confidence: Confidence::Low,
        }
    }

    fn family_cmd(family: &str, worktree: &str) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Family(FamilyKey::new(family.to_string())),
            family_key: Some(FamilyKey::new(family.to_string())),
            worktree: Some(PathBuf::from(worktree)),
            root_sid: "f1".to_string(),
            raw_argv: vec!["git".to_string(), "status".to_string()],
            primary_command: Some("status".to_string()),
            invoked_command: Some("status".to_string()),
            invoked_args: Vec::new(),
            observed_child_commands: Vec::new(),
            exit_code: 0,
            started_at_ns: 1,
            finished_at_ns: 2,
            reflog_start_offsets: std::collections::HashMap::new(),
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes: Vec::new(),
            confidence: Confidence::Low,
        }
    }

    #[tokio::test]
    async fn routes_global_and_family_commands() {
        let backend = Arc::new(MockBackend::default().with_family("/repo", "family:/repo"));
        let coordinator = Coordinator::new(backend);

        let g = coordinator.route_command(global_cmd()).await.unwrap();
        assert_eq!(g.seq, 1);

        let f = coordinator
            .route_command(family_cmd("family:/repo", "/repo"))
            .await
            .unwrap();
        assert_eq!(f.seq, 1);

        coordinator.shutdown().await.unwrap();
    }
}
