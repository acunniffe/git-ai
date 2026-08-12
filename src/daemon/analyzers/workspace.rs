use crate::daemon::analyzers::{AnalysisView, CommandAnalyzer, command_args, normalized_args};
use crate::daemon::domain::{
    AnalysisResult, CommandClass, Confidence, NormalizedCommand, SemanticEvent, StashOpKind,
};
use crate::error::GitAiError;

#[derive(Default)]
pub struct WorkspaceAnalyzer;

impl CommandAnalyzer for WorkspaceAnalyzer {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError> {
        let name = cmd.primary_command.as_deref().unwrap_or_default();
        let args = command_args(cmd);

        let mut events = Vec::new();
        match name {
            "apply" => events.push(
                if args.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        "--check" | "--stat" | "--numstat" | "--summary"
                    )
                }) {
                    SemanticEvent::ReadOnlyCommand
                } else {
                    SemanticEvent::ApplyPaths
                },
            ),
            "restore" => events.push(SemanticEvent::RestorePaths {
                head: current_head_for_workspace_command(cmd, state.refs),
            }),
            "clean" => events.push(
                if crate::git::command_classification::invocation_has_dry_run(&args) {
                    SemanticEvent::ReadOnlyCommand
                } else {
                    SemanticEvent::CleanedWorkspace {
                        head: current_head_for_workspace_command(cmd, state.refs),
                    }
                },
            ),
            "rm" => events.push(
                if crate::git::command_classification::invocation_has_dry_run(&args) {
                    SemanticEvent::ReadOnlyCommand
                } else if args.iter().any(|arg| arg == "--cached") {
                    // Index-only removal leaves the attributed worktree bytes intact.
                    SemanticEvent::OpaqueCommand
                } else {
                    SemanticEvent::RemovedWorkspacePaths {
                        head: current_head_for_workspace_command(cmd, state.refs),
                    }
                },
            ),
            "mv" => events.push(
                if crate::git::command_classification::invocation_has_dry_run(&args) {
                    SemanticEvent::ReadOnlyCommand
                } else {
                    SemanticEvent::MovedWorkspacePaths {
                        head: current_head_for_workspace_command(cmd, state.refs),
                    }
                },
            ),
            "stash" => {
                let stash_args = stash_command_args(cmd);
                events.push(SemanticEvent::StashOperation {
                    kind: infer_stash_kind(&stash_args),
                    head: current_head_for_workspace_command(cmd, state.refs),
                });
            }
            "checkout" => {
                if is_path_checkout(&args) {
                    events.push(SemanticEvent::CheckoutPaths);
                } else if let Some(change) = cmd.ref_changes.first() {
                    events.push(SemanticEvent::RefUpdated {
                        reference: change.reference.clone(),
                        old: change.old.clone(),
                        new: change.new.clone(),
                    });
                }
            }
            "switch" => {
                if let Some(change) = cmd.ref_changes.first() {
                    events.push(SemanticEvent::RefUpdated {
                        reference: change.reference.clone(),
                        old: change.old.clone(),
                        new: change.new.clone(),
                    });
                }
            }
            _ => unreachable!("registry should not route '{}' to WorkspaceAnalyzer", name),
        }

        if events.is_empty() {
            events.push(SemanticEvent::OpaqueCommand);
        }

        Ok(AnalysisResult {
            class: CommandClass::WorkspaceMutation,
            events,
            confidence: if cmd.exit_code == 0 {
                Confidence::High
            } else {
                Confidence::Low
            },
        })
    }
}

fn stash_command_args(cmd: &NormalizedCommand) -> Vec<String> {
    let args = normalized_args(&cmd.raw_argv);
    if let Some(index) = args.iter().position(|arg| arg == "stash")
        && let Some(stash_args) = args.get(index + 1..)
    {
        return stash_args.to_vec();
    }
    command_args(cmd)
}

fn infer_stash_kind(args: &[String]) -> StashOpKind {
    match args.first().map(String::as_str).unwrap_or("push") {
        "push" | "save" => StashOpKind::Push,
        "create" => StashOpKind::Create,
        "store" => StashOpKind::Store,
        "apply" => StashOpKind::Apply,
        "pop" => StashOpKind::Pop,
        "drop" => StashOpKind::Drop,
        "clear" => StashOpKind::Clear,
        "list" => StashOpKind::List,
        "branch" => StashOpKind::Branch,
        "show" => StashOpKind::Show,
        _ => StashOpKind::Unknown,
    }
}

fn is_path_checkout(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--")
        || args
            .iter()
            .any(|arg| arg.starts_with("--pathspec") || arg == "--ours" || arg == "--theirs")
}

fn current_head_for_workspace_command(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<String> {
    current_branch_ref(cmd)
        .and_then(|reference| refs.get(&reference).cloned())
        .or_else(|| refs.get("HEAD").cloned())
        .or_else(|| {
            cmd.ref_changes
                .iter()
                .find(|change| change.reference == "HEAD")
                .map(|change| change.old.clone())
        })
        .filter(|head| !head.trim().is_empty())
}

fn current_branch_ref(_cmd: &NormalizedCommand) -> Option<String> {
    None
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
    fn stash_apply_maps_to_stash_operation() {
        let analyzer = WorkspaceAnalyzer;
        let mut refs = std::collections::HashMap::new();
        refs.insert("HEAD".to_string(), "abc123".to_string());
        let cmd = command("stash", &["git", "stash", "apply", "stash@{0}"]);
        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::StashOperation {
                kind: StashOpKind::Apply,
                head: Some(head),
                ..
            } if head == "abc123"
        )));
    }

    #[test]
    fn stash_lifecycle_kinds_are_explicit() {
        let analyzer = WorkspaceAnalyzer;
        let refs = std::collections::HashMap::new();
        for (subcommand, expected) in [
            ("create", StashOpKind::Create),
            ("store", StashOpKind::Store),
            ("clear", StashOpKind::Clear),
        ] {
            let cmd = command("stash", &["git", "stash", subcommand]);
            let result = analyzer
                .analyze(&cmd, AnalysisView { refs: &refs })
                .unwrap();
            assert!(result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::StashOperation { kind, .. } if *kind == expected
            )));
        }
    }

    #[test]
    fn apply_distinguishes_mutation_from_check_mode() {
        let analyzer = WorkspaceAnalyzer;
        let refs = std::collections::HashMap::new();
        let applied = analyzer
            .analyze(
                &command("apply", &["git", "apply", "--cached", "change.patch"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(applied.events, vec![SemanticEvent::ApplyPaths]);

        let checked = analyzer
            .analyze(
                &command("apply", &["git", "apply", "--check", "change.patch"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(checked.events, vec![SemanticEvent::ReadOnlyCommand]);
    }

    #[test]
    fn restore_and_clean_emit_workspace_semantics() {
        let analyzer = WorkspaceAnalyzer;
        let refs = std::collections::HashMap::new();
        let restored = analyzer
            .analyze(
                &command("restore", &["git", "restore", "--", "file.txt"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(
            restored.events,
            vec![SemanticEvent::RestorePaths { head: None }]
        );
        let cleaned = analyzer
            .analyze(
                &command("clean", &["git", "clean", "-fd"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(
            cleaned.events,
            vec![SemanticEvent::CleanedWorkspace { head: None }]
        );
    }

    #[test]
    fn rm_distinguishes_worktree_cached_and_dry_run_modes() {
        let analyzer = WorkspaceAnalyzer;
        let refs = std::collections::HashMap::new();
        let removed = analyzer
            .analyze(
                &command("rm", &["git", "rm", "-rf", "--", "pkg"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(
            removed.events,
            vec![SemanticEvent::RemovedWorkspacePaths { head: None }]
        );

        let cached = analyzer
            .analyze(
                &command("rm", &["git", "rm", "--cached", "file.txt"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(cached.events, vec![SemanticEvent::OpaqueCommand]);

        let dry_run = analyzer
            .analyze(
                &command("rm", &["git", "rm", "-n", "file.txt"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(dry_run.events, vec![SemanticEvent::ReadOnlyCommand]);

        let bundled_dry_run = analyzer
            .analyze(
                &command("rm", &["git", "rm", "-nr", "file.txt"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(bundled_dry_run.events, vec![SemanticEvent::ReadOnlyCommand]);

        let clean_bundled_dry_run = analyzer
            .analyze(
                &command("clean", &["git", "clean", "-ndx", "build"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(
            clean_bundled_dry_run.events,
            vec![SemanticEvent::ReadOnlyCommand]
        );
    }

    #[test]
    fn mv_emits_path_move_semantics() {
        let analyzer = WorkspaceAnalyzer;
        let refs = std::collections::HashMap::new();
        let moved = analyzer
            .analyze(
                &command("mv", &["git", "mv", "--force", "source.txt", "target.txt"]),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(
            moved.events,
            vec![SemanticEvent::MovedWorkspacePaths { head: None }]
        );

        let preview = analyzer
            .analyze(
                &command(
                    "mv",
                    &["git", "mv", "--dry-run", "source.txt", "target.txt"],
                ),
                AnalysisView { refs: &refs },
            )
            .unwrap();
        assert_eq!(preview.events, vec![SemanticEvent::ReadOnlyCommand]);
    }
}
