use crate::error::GitAiError;
use crate::model::domain::{
    AnalysisResult, CommandClass, Confidence, NormalizedCommand, SemanticEvent,
};
use crate::operations::daemon::analyzers::{AnalysisView, CommandAnalyzer};
use crate::operations::git::command_classification::is_definitely_read_only_command;
use crate::operations::git::command_policy::{is_repo_admin_command, is_transport_command};

#[derive(Default)]
pub struct GenericAnalyzer;

impl CommandAnalyzer for GenericAnalyzer {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        _state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError> {
        let command_name = cmd
            .primary_command
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        if !cmd.ref_changes.is_empty() {
            return Ok(AnalysisResult {
                class: CommandClass::RefMutation,
                events: cmd
                    .ref_changes
                    .iter()
                    .map(|change| SemanticEvent::RefUpdated {
                        reference: change.reference.clone(),
                        old: change.old.clone(),
                        new: change.new.clone(),
                    })
                    .collect(),
                confidence: Confidence::Medium,
            });
        }

        if is_transport_command(&command_name) {
            return Ok(AnalysisResult {
                class: CommandClass::Transport,
                events: vec![SemanticEvent::OpaqueCommand],
                confidence: Confidence::Low,
            });
        }

        if is_repo_admin_command(&command_name) {
            return Ok(AnalysisResult {
                class: CommandClass::RepoAdmin,
                events: vec![SemanticEvent::OpaqueCommand],
                confidence: Confidence::Low,
            });
        }

        if is_read_only_command(&command_name) {
            return Ok(AnalysisResult {
                class: CommandClass::ReadOnly,
                events: vec![SemanticEvent::ReadOnlyCommand],
                confidence: Confidence::Medium,
            });
        }

        Ok(AnalysisResult {
            class: CommandClass::Opaque,
            events: vec![SemanticEvent::OpaqueCommand],
            confidence: Confidence::Low,
        })
    }
}

fn is_read_only_command(command: &str) -> bool {
    is_definitely_read_only_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::domain::{CommandScope, RefChange};

    fn command(primary: &str) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Global,
            family_key: None,
            worktree: None,
            root_sid: "r".to_string(),
            trace_derived: false,
            raw_argv: vec!["git".to_string(), primary.to_string()],
            primary_command: Some(primary.to_string()),
            invoked_command: Some(primary.to_string()),
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

    #[test]
    fn generic_ref_mutation_when_ref_changes_exist() {
        let analyzer = GenericAnalyzer;
        let mut cmd = command("whatever");
        cmd.ref_changes.push(RefChange {
            reference: "refs/heads/main".to_string(),
            old: "a".to_string(),
            new: "b".to_string(),
        });
        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &std::collections::HashMap::new(),
                },
            )
            .unwrap();
        assert!(matches!(result.class, CommandClass::RefMutation));
    }

    #[test]
    fn generic_never_returns_empty_events() {
        let analyzer = GenericAnalyzer;
        let result = analyzer
            .analyze(
                &command("custom-weird-command"),
                AnalysisView {
                    refs: &std::collections::HashMap::new(),
                },
            )
            .unwrap();
        assert!(!result.events.is_empty());
    }

    #[test]
    fn generic_policy_categories_follow_shared_command_metadata() {
        let analyzer = GenericAnalyzer;
        for (command_name, expected_class) in [
            ("ls-remote", CommandClass::Transport),
            ("maintenance", CommandClass::RepoAdmin),
            ("status", CommandClass::ReadOnly),
        ] {
            let result = analyzer
                .analyze(
                    &command(command_name),
                    AnalysisView {
                        refs: &std::collections::HashMap::new(),
                    },
                )
                .unwrap();
            assert_eq!(
                result.class, expected_class,
                "unexpected class for {command_name}"
            );
        }
    }
}
