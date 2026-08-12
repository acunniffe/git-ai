use crate::daemon::analyzers::{AnalysisView, CommandAnalyzer, command_args, normalized_args};
use crate::daemon::domain::{
    AnalysisResult, CommandClass, Confidence, NormalizedCommand, PullStrategy, SemanticEvent,
};
use crate::error::GitAiError;
use std::path::PathBuf;

#[derive(Default)]
pub struct TransportAnalyzer;

impl CommandAnalyzer for TransportAnalyzer {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        _state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError> {
        let name = cmd.primary_command.as_deref().unwrap_or_default();
        let args = command_args(cmd);

        let mut events = Vec::new();
        match name {
            "fetch" => events.push(SemanticEvent::FetchCompleted {
                remote: first_positional(&args),
            }),
            "pull" => events.push(SemanticEvent::PullCompleted {
                remote: first_positional(&args),
                strategy: infer_pull_strategy(cmd, &args),
            }),
            "push" => events.push(SemanticEvent::PushCompleted {
                remote: first_positional(&args),
            }),
            "clone" => events.push(SemanticEvent::CloneCompleted {
                target: infer_clone_target(&args)
                    .or_else(|| cmd.worktree.clone())
                    .unwrap_or_else(|| PathBuf::from(".")),
            }),
            "ls-remote" => events.push(SemanticEvent::LsRemoteCompleted),
            _ => unreachable!("registry should not route '{}' to TransportAnalyzer", name),
        }

        Ok(AnalysisResult {
            class: CommandClass::Transport,
            events,
            confidence: if cmd.exit_code == 0 {
                Confidence::High
            } else {
                Confidence::Low
            },
        })
    }
}

fn first_positional(args: &[String]) -> Option<String> {
    args.iter().find(|arg| !arg.starts_with('-')).cloned()
}

fn infer_pull_strategy(cmd: &NormalizedCommand, args: &[String]) -> PullStrategy {
    if let Some(strategy) = infer_pull_strategy_from_args(args) {
        return strategy;
    }
    let raw_args = normalized_args(&cmd.raw_argv);
    if let Some(strategy) = infer_pull_strategy_from_args(&raw_args) {
        return strategy;
    }
    if cmd
        .observed_child_commands
        .iter()
        .any(|child| child == "rebase")
    {
        return PullStrategy::Rebase;
    }
    PullStrategy::Merge
}

fn infer_pull_strategy_from_args(args: &[String]) -> Option<PullStrategy> {
    if args
        .iter()
        .any(|arg| arg == "--no-rebase" || arg == "--rebase=false")
    {
        return Some(PullStrategy::Merge);
    }
    if args.iter().any(|arg| arg == "--ff-only") {
        return Some(PullStrategy::FastForwardOnly);
    }
    if args
        .iter()
        .any(|arg| arg == "--rebase=merges" || arg == "--rebase-merges")
    {
        return Some(PullStrategy::RebaseMerges);
    }
    if args
        .iter()
        .any(|arg| arg == "--rebase" || arg == "--rebase=true")
    {
        return Some(PullStrategy::Rebase);
    }
    None
}

fn infer_clone_target(args: &[String]) -> Option<PathBuf> {
    if args.is_empty() {
        return None;
    }
    let mut filtered = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-C" || arg == "--origin" || arg == "--template" {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        filtered.push(arg.clone());
    }
    filtered.last().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::CommandScope;

    fn command(primary: &str, argv: &[&str]) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: "r".to_string(),
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

    #[test]
    fn pull_with_rebase_maps_strategy() {
        let analyzer = TransportAnalyzer;
        let result = analyzer
            .analyze(
                &command("pull", &["git", "pull", "--rebase"]),
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::PullCompleted {
                strategy: PullStrategy::Rebase,
                ..
            }
        )));
    }
}
