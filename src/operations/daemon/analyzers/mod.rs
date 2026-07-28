use crate::error::GitAiError;
use crate::model::domain::{AnalysisResult, NormalizedCommand};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub mod generic;
pub mod history;
pub mod transport;
pub mod workspace;

#[derive(Debug, Clone)]
pub struct AnalysisView<'a> {
    pub refs: &'a HashMap<String, String>,
}

pub trait CommandAnalyzer: Send + Sync {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError>;
}

#[derive(Clone)]
pub struct AnalyzerRegistry {
    generic: Arc<dyn CommandAnalyzer>,
    by_command: HashMap<String, Arc<dyn CommandAnalyzer>>,
}

impl Default for AnalyzerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AnalyzerRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            generic: Arc::new(generic::GenericAnalyzer),
            by_command: HashMap::new(),
        };

        let history: Arc<dyn CommandAnalyzer> = Arc::new(history::HistoryAnalyzer);
        for command in [
            "commit",
            "reset",
            "rebase",
            "cherry-pick",
            "merge",
            "revert",
            "update-ref",
        ] {
            registry.register_command(command, history.clone());
        }

        let workspace: Arc<dyn CommandAnalyzer> = Arc::new(workspace::WorkspaceAnalyzer);
        for command in ["stash", "checkout", "switch"] {
            registry.register_command(command, workspace.clone());
        }

        let transport: Arc<dyn CommandAnalyzer> = Arc::new(transport::TransportAnalyzer);
        for command in ["fetch", "pull", "push", "clone"] {
            registry.register_command(command, transport.clone());
        }

        registry
    }

    pub fn register_command(
        &mut self,
        command: impl Into<String>,
        analyzer: Arc<dyn CommandAnalyzer>,
    ) {
        self.by_command
            .insert(command.into().to_ascii_lowercase(), analyzer);
    }

    pub fn analyze(
        &self,
        cmd: &NormalizedCommand,
        state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError> {
        if let Some(command) = cmd.primary_command.as_ref() {
            let key = command.to_ascii_lowercase();
            if let Some(analyzer) = self.by_command.get(&key) {
                return analyzer.analyze(cmd, state);
            }
        }
        self.generic.analyze(cmd, state)
    }
}

pub(crate) fn command_args(cmd: &NormalizedCommand) -> Vec<String> {
    if !cmd.invoked_args.is_empty() {
        return cmd.invoked_args.clone();
    }
    normalized_args(&cmd.raw_argv)
}

pub(crate) fn normalized_args(argv: &[String]) -> Vec<String> {
    let start = argv
        .first()
        .and_then(|arg| Path::new(arg).file_name().and_then(|name| name.to_str()))
        .is_some_and(|name| name == "git" || name == "git.exe");
    if start {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::domain::{CommandScope, Confidence};

    /// Shared fixture builder for analyzer unit tests: a global-scope command
    /// with no ref changes. `workspace.rs` and `transport.rs` import
    /// this. `history.rs` keeps its own local variant (its default seeds a
    /// HEAD ref change several tests rely on) and `generic.rs` keeps its
    /// 1-arg variant (no argv needed there) — neither is a byte-identical
    /// duplicate of this.
    pub(super) fn command(primary: &str, argv: &[&str]) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: "r".to_string(),
            trace_derived: false,
            raw_argv: argv.iter().map(|s| s.to_string()).collect(),
            primary_command: Some(primary.to_string()),
            invoked_command: Some(primary.to_string()),
            invoked_args: argv.iter().skip(2).map(|s| s.to_string()).collect(),
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
}
