use crate::daemon::domain::{AnalysisResult, NormalizedCommand};
use crate::error::GitAiError;
use crate::git::cli_parser::parse_git_cli_args;
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

pub(crate) fn checkout_is_path_checkout(cmd: &NormalizedCommand) -> bool {
    let args = if cmd.invoked_args.is_empty() {
        parse_git_cli_args(&normalized_args(&cmd.raw_argv)).command_args
    } else {
        cmd.invoked_args.clone()
    };
    let mut positionals = 0usize;
    let mut skip_option_value = false;
    for arg in &args {
        if skip_option_value {
            skip_option_value = false;
            continue;
        }
        match arg.as_str() {
            "--" | "-p" | "--patch" | "--ours" | "--theirs" => return true,
            "-b" | "-B" | "--orphan" | "--conflict" => skip_option_value = true,
            value if value.starts_with("--pathspec") => return true,
            value if value.starts_with('-') => {}
            _ => {
                positionals += 1;
                if positionals > 1 {
                    return true;
                }
            }
        }
    }
    false
}
