use crate::daemon::analyzers::{AnalysisView, CommandAnalyzer, command_args};
use crate::daemon::domain::{
    AnalysisResult, CommandClass, Confidence, NormalizedCommand, ResetKind, SemanticEvent,
};
use crate::error::GitAiError;
use crate::git::cli_parser::explicit_rebase_branch_arg;
use crate::git::repo_state::is_valid_git_oid;

#[derive(Default)]
pub struct HistoryAnalyzer;

impl CommandAnalyzer for HistoryAnalyzer {
    fn analyze(
        &self,
        cmd: &NormalizedCommand,
        state: AnalysisView<'_>,
    ) -> Result<AnalysisResult, GitAiError> {
        let name = cmd.primary_command.as_deref().unwrap_or_default();
        let args = command_args(cmd);

        let mut events = Vec::new();
        match name {
            "am" => {
                if args.iter().any(|arg| arg == "--abort") {
                    if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                        events.push(SemanticEvent::Reset {
                            kind: ResetKind::Hard,
                            old_head,
                            new_head,
                        });
                    }
                } else {
                    let transitions = cmd
                        .ref_changes
                        .iter()
                        .filter(|change| change.reference == "HEAD")
                        .filter_map(valid_ref_transition)
                        .collect::<Vec<_>>();
                    if let Some((original_head, _)) = transitions.first() {
                        events.push(SemanticEvent::AmApplied {
                            original_head: original_head.clone(),
                            new_commits: transitions.into_iter().map(|(_, new)| new).collect(),
                        });
                    }
                }
            }
            "commit" => {
                let amend = args.iter().any(|arg| arg == "--amend");
                if amend {
                    if let Some((old_head, new_head)) = amend_head_change(cmd) {
                        events.push(SemanticEvent::CommitAmended { old_head, new_head });
                    }
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::CommitCreated {
                        base: sanitize_base(Some(old_head), &new_head),
                        new_head,
                    });
                }
            }
            "revert" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::RevertAbort {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--quit") {
                    events.push(SemanticEvent::RevertQuit {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--skip") {
                    events.push(SemanticEvent::RevertSkip {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--no-commit" || arg == "-n") {
                    events.push(SemanticEvent::RevertNoCommit {
                        source_commits: cmd.revert_source_oids.clone(),
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::CommitCreated {
                        base: sanitize_base(Some(old_head), &new_head),
                        new_head,
                    });
                } else if cmd.exit_code != 0 {
                    events.push(SemanticEvent::RevertPrepared {
                        source_commits: cmd.revert_source_oids.clone(),
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                }
            }
            "reset" => {
                if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::Reset {
                        kind: infer_reset_kind(&args),
                        old_head,
                        new_head,
                    });
                } else if let Some(head) = current_head_from_ref_data(cmd, state.refs) {
                    events.push(SemanticEvent::Reset {
                        kind: infer_reset_kind(&args),
                        old_head: head.clone(),
                        new_head: head,
                    });
                }
            }
            "rebase" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::RebaseAbort {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--quit") {
                    events.push(SemanticEvent::RebaseQuit {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else {
                    if args.iter().any(|arg| arg == "--skip") {
                        events.push(SemanticEvent::RebaseSkip {
                            head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                        });
                    }
                    if let Some((old_head, new_head)) = rebase_change(cmd, state.refs) {
                        events.push(SemanticEvent::RebaseComplete {
                            old_head,
                            new_head,
                            interactive: args
                                .iter()
                                .any(|arg| arg == "-i" || arg == "--interactive"),
                        });
                    }
                    if cmd.exit_code != 0 {
                        events.push(SemanticEvent::RebasePrepared {
                            head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                        });
                    }
                }
            }
            "cherry-pick" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::CherryPickAbort {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--quit") {
                    events.push(SemanticEvent::CherryPickQuit {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--no-commit" || arg == "-n") {
                    events.push(SemanticEvent::CherryPickNoCommit {
                        source_commits: cmd.cherry_pick_source_oids.clone(),
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    let resulting_head = new_head.clone();
                    events.push(SemanticEvent::CherryPickComplete {
                        original_head: old_head,
                        new_head,
                        source_commits: cmd.cherry_pick_source_oids.clone(),
                        new_commits: cherry_pick_new_commits(cmd),
                    });
                    if cmd.exit_code != 0 {
                        events.push(SemanticEvent::CherryPickPrepared {
                            head: resulting_head,
                        });
                    }
                } else if args.iter().any(|arg| arg == "--skip") {
                    events.push(SemanticEvent::CherryPickSkip {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if cmd.exit_code != 0 {
                    events.push(SemanticEvent::CherryPickPrepared {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                }
            }
            "merge" => {
                if args.iter().any(|arg| arg == "--abort") {
                    events.push(SemanticEvent::MergeAbort {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--quit") {
                    events.push(SemanticEvent::MergeQuit {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                } else if args.iter().any(|arg| arg == "--squash") {
                    if let Some(source_head) = squash_source_head(&args, state.refs)
                        && let Some(onto) = current_head_from_ref_data(cmd, state.refs)
                    {
                        events.push(SemanticEvent::MergeSquash { source_head, onto });
                    }
                } else if let Some((old_head, new_head)) = head_change(cmd, state.refs) {
                    events.push(SemanticEvent::MergeComplete {
                        original_head: old_head,
                        new_head,
                    });
                } else if args.iter().any(|arg| arg == "--no-commit") || cmd.exit_code != 0 {
                    events.push(SemanticEvent::MergePrepared {
                        head: current_head_from_ref_data(cmd, state.refs).unwrap_or_default(),
                    });
                }
            }
            "update-ref" => {
                for change in cmd.ref_changes.iter().filter(|change| {
                    (change.reference == "HEAD" || change.reference.starts_with("refs/heads/"))
                        && change.old.trim() != change.new.trim()
                }) {
                    events.push(SemanticEvent::RefUpdated {
                        reference: change.reference.clone(),
                        old: change.old.clone(),
                        new: change.new.clone(),
                    });
                }
            }
            _ => unreachable!("registry should not route '{}' to HistoryAnalyzer", name),
        }

        if events.is_empty() {
            events.push(SemanticEvent::OpaqueCommand);
        }

        Ok(AnalysisResult {
            class: CommandClass::HistoryRewrite,
            events,
            confidence: if cmd.exit_code == 0 {
                Confidence::High
            } else {
                Confidence::Low
            },
        })
    }
}

fn is_zero_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.chars().all(|c| c == '0')
}

fn sanitize_base(base: Option<String>, new_head: &str) -> Option<String> {
    base.filter(|candidate| candidate != new_head && !is_zero_oid(candidate))
}

fn valid_non_zero_oid(value: &str) -> bool {
    is_valid_git_oid(value) && !is_zero_oid(value)
}

fn squash_source_head(
    args: &[String],
    refs: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let source = merge_source_args(args).into_iter().next()?;
    resolve_revision_from_ref_state(source, refs)
}

fn merge_source_args(args: &[String]) -> Vec<&str> {
    let mut sources = Vec::new();
    let mut iter = args.iter().map(String::as_str).peekable();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            sources.extend(iter.filter(|value| !value.is_empty()));
            break;
        }
        if arg == "-m"
            || arg == "--message"
            || arg == "-s"
            || arg == "--strategy"
            || arg == "-X"
            || arg == "--strategy-option"
        {
            let _ = iter.next();
            continue;
        }
        if arg.starts_with("--message=")
            || arg.starts_with("--strategy=")
            || arg.starts_with("--strategy-option=")
            || arg.starts_with("--gpg-sign=")
            || arg.starts_with("-m")
            || arg.starts_with("-s")
            || arg.starts_with("-X")
            || arg.starts_with("-S")
        {
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        sources.push(arg);
    }
    sources
}

fn resolve_revision_from_ref_state(
    revision: &str,
    refs: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if valid_non_zero_oid(revision) {
        return Some(revision.to_string());
    }
    if revision == "HEAD" {
        return refs
            .get("HEAD")
            .filter(|oid| valid_non_zero_oid(oid))
            .cloned();
    }
    if revision.starts_with("refs/") {
        return refs
            .get(revision)
            .filter(|oid| valid_non_zero_oid(oid))
            .cloned();
    }

    for reference in [
        format!("refs/heads/{}", revision),
        format!("refs/remotes/{}", revision),
        format!("refs/tags/{}", revision),
    ] {
        if let Some(oid) = refs.get(&reference)
            && valid_non_zero_oid(oid)
        {
            return Some(oid.clone());
        }
    }

    None
}

fn valid_ref_transition(change: &crate::daemon::domain::RefChange) -> Option<(String, String)> {
    let old = change.old.trim();
    let new = change.new.trim();
    if old == new || !valid_non_zero_oid(old) || !valid_non_zero_oid(new) {
        return None;
    }
    Some((old.to_string(), new.to_string()))
}

fn first_ref_transition_for(cmd: &NormalizedCommand, reference: &str) -> Option<(String, String)> {
    cmd.ref_changes
        .iter()
        .filter(|change| change.reference == reference)
        .find_map(valid_ref_transition)
}

fn current_head_from_ref_data(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<String> {
    cmd.ref_changes
        .iter()
        .rev()
        .find(|change| change.reference == "HEAD")
        .map(|change| change.new.clone())
        .or_else(|| refs.get("HEAD").cloned())
        .filter(|head| valid_non_zero_oid(head))
}

fn amend_head_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    // Amend is defined by the HEAD transition made by `git commit --amend`.
    // Prefer that exact transition over branch hints: branch context is not
    // part of stock trace2 and can be stale if it was read after the command.
    if let Some(change) = first_ref_transition_for(cmd, "HEAD") {
        return Some(change);
    }

    single_branch_ref_change(cmd)
}

fn head_change(
    cmd: &NormalizedCommand,
    _refs: &std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    let head_span = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference == "HEAD"
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if let Some((old_head, new_head)) = change_span(&head_span) {
        return Some((old_head, new_head));
    }

    single_branch_ref_change(cmd)
}

fn single_branch_ref_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    let mut branch_refs = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/")
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if branch_refs.is_empty() {
        return None;
    }
    branch_refs.sort_by(|a, b| a.reference.cmp(&b.reference));
    branch_refs.dedup_by(|a, b| a.reference == b.reference && a.old == b.old && a.new == b.new);
    let first_ref = branch_refs.first()?.reference.as_str();
    if branch_refs
        .iter()
        .any(|change| change.reference.as_str() != first_ref)
    {
        return None;
    }
    change_span(&branch_refs)
}

fn cherry_pick_new_commits(cmd: &NormalizedCommand) -> Vec<String> {
    cmd.ref_changes
        .iter()
        .filter(|change| change.reference == "HEAD")
        .filter_map(valid_ref_transition)
        .map(|(_, new)| new)
        .collect()
}

fn rebase_change(
    cmd: &NormalizedCommand,
    refs: &std::collections::HashMap<String, String>,
) -> Option<(String, String)> {
    if let Some((old_head, new_head)) = explicit_rebase_branch_change(cmd) {
        return Some((old_head, new_head));
    }

    if let Some((old_head, new_head)) = inferred_rebase_branch_change(cmd) {
        return Some((old_head, new_head));
    }

    let (old_head, new_head) = head_change(cmd, refs)?;
    (old_head != new_head).then_some((old_head, new_head))
}

fn inferred_rebase_branch_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    let mut candidates = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/")
                && !change.old.trim().is_empty()
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }

    if candidates.len() == 1 {
        let change = candidates.pop()?;
        return Some((change.old.trim().to_string(), change.new.trim().to_string()));
    }

    None
}

fn explicit_rebase_branch_change(cmd: &NormalizedCommand) -> Option<(String, String)> {
    let args = command_args(cmd);
    let branch = explicit_rebase_branch_arg(&args)?;
    let branch_ref = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{}", branch)
    };
    cmd.ref_changes
        .iter()
        .find(|change| {
            change.reference == branch_ref
                && !change.old.trim().is_empty()
                && !change.new.trim().is_empty()
                && change.old.trim() != change.new.trim()
        })
        .map(|change| (change.old.trim().to_string(), change.new.trim().to_string()))
}

fn change_span(changes: &[&crate::daemon::domain::RefChange]) -> Option<(String, String)> {
    let first = changes.first()?;
    let last = changes.last()?;
    let old_head = first.old.trim();
    let new_head = last.new.trim();
    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return None;
    }
    Some((old_head.to_string(), new_head.to_string()))
}

fn infer_reset_kind(args: &[String]) -> ResetKind {
    if args.iter().any(|arg| arg == "--soft") {
        return ResetKind::Soft;
    }
    if args.iter().any(|arg| arg == "--mixed") {
        return ResetKind::Mixed;
    }
    if args.iter().any(|arg| arg == "--hard") {
        return ResetKind::Hard;
    }
    if args.iter().any(|arg| arg == "--merge") {
        return ResetKind::Merge;
    }
    if args.iter().any(|arg| arg == "--keep") {
        return ResetKind::Keep;
    }
    ResetKind::Mixed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::{CommandScope, RefChange};

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
            index_tree_at_start: None,
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes: vec![RefChange {
                reference: "HEAD".to_string(),
                old: "a".to_string(),
                new: "b".to_string(),
            }],
            confidence: Confidence::Low,
        }
    }

    fn assert_only_opaque(result: &AnalysisResult) {
        assert!(
            result
                .events
                .iter()
                .all(|event| matches!(event, SemanticEvent::OpaqueCommand)),
            "expected only opaque events, got {:?}",
            result.events
        );
    }

    #[test]
    fn update_ref_reports_cursor_ref_changes() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command(
            "update-ref",
            &[
                "git",
                "update-ref",
                "refs/heads/main",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ],
        );
        cmd.ref_changes = vec![RefChange {
            reference: "refs/heads/main".to_string(),
            old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        }];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::RefUpdated { reference, old, new }
                if reference == "refs/heads/main"
                    && old == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )));
    }

    #[test]
    fn update_ref_without_cursor_ref_change_is_opaque() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command(
            "update-ref",
            &[
                "git",
                "update-ref",
                "refs/heads/main",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ],
        );
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();

        assert_only_opaque(&result);
    }

    #[test]
    fn squash_merge_resolves_branch_from_ref_state() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("merge", &["git", "merge", "--squash", "feature"]);
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([
            (
                "HEAD".to_string(),
                "1111111111111111111111111111111111111111".to_string(),
            ),
            (
                "refs/heads/feature".to_string(),
                "2222222222222222222222222222222222222222".to_string(),
            ),
        ]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::MergeSquash { source_head, onto }
                if source_head == "2222222222222222222222222222222222222222"
                    && onto == "1111111111111111111111111111111111111111"
        )));
    }

    #[test]
    fn squash_merge_with_unresolved_source_is_opaque() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("merge", &["git", "merge", "--squash", "feature"]);
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();

        assert_only_opaque(&result);
    }

    #[test]
    fn merge_conflict_emits_prepared_at_current_head() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("merge", &["git", "merge", "feature"]);
        cmd.exit_code = 1;
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert!(matches!(
            result.events.as_slice(),
            [SemanticEvent::MergePrepared { head }]
                if head == "1111111111111111111111111111111111111111"
        ));
    }

    #[test]
    fn merge_continue_transition_emits_complete() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("merge", &["git", "merge", "--continue"]);
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "1111111111111111111111111111111111111111".to_string(),
            new: "2222222222222222222222222222222222222222".to_string(),
        }];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(matches!(
            result.events.as_slice(),
            [SemanticEvent::MergeComplete {
                original_head,
                new_head,
            }] if original_head == "1111111111111111111111111111111111111111"
                && new_head == "2222222222222222222222222222222222222222"
        ));
    }

    #[test]
    fn merge_abort_and_quit_emit_distinct_controls() {
        let analyzer = HistoryAnalyzer;
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);

        for (flag, abort) in [("--abort", true), ("--quit", false)] {
            let mut cmd = command("merge", &["git", "merge", flag]);
            cmd.ref_changes.clear();
            let result = analyzer
                .analyze(&cmd, AnalysisView { refs: &refs })
                .unwrap();
            assert!(result.events.iter().any(|event| match event {
                SemanticEvent::MergeAbort { head } if abort => {
                    head == "1111111111111111111111111111111111111111"
                }
                SemanticEvent::MergeQuit { head } if !abort => {
                    head == "1111111111111111111111111111111111111111"
                }
                _ => false,
            }));
        }
    }

    #[test]
    fn revert_no_commit_and_conflict_preserve_source_context() {
        let analyzer = HistoryAnalyzer;
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);
        let source = "2222222222222222222222222222222222222222".to_string();

        let mut no_commit = command("revert", &["git", "revert", "--no-commit", "source"]);
        no_commit.ref_changes.clear();
        no_commit.revert_source_oids = vec![source.clone()];
        let result = analyzer
            .analyze(&no_commit, AnalysisView { refs: &refs })
            .unwrap();
        assert!(matches!(
            result.events.as_slice(),
            [SemanticEvent::RevertNoCommit {
                source_commits,
                head,
            }] if source_commits == &vec![source.clone()]
                && head == "1111111111111111111111111111111111111111"
        ));

        let mut conflict = command("revert", &["git", "revert", "source"]);
        conflict.exit_code = 1;
        conflict.ref_changes.clear();
        conflict.revert_source_oids = vec![source.clone()];
        let result = analyzer
            .analyze(&conflict, AnalysisView { refs: &refs })
            .unwrap();
        assert!(matches!(
            result.events.as_slice(),
            [SemanticEvent::RevertPrepared {
                source_commits,
                head,
            }] if source_commits == &vec![source]
                && head == "1111111111111111111111111111111111111111"
        ));
    }

    #[test]
    fn revert_controls_are_distinct() {
        let analyzer = HistoryAnalyzer;
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);
        for flag in ["--abort", "--quit", "--skip"] {
            let mut cmd = command("revert", &["git", "revert", flag]);
            cmd.ref_changes.clear();
            let result = analyzer
                .analyze(&cmd, AnalysisView { refs: &refs })
                .unwrap();
            assert!(result.events.iter().any(|event| matches!(
                (flag, event),
                ("--abort", SemanticEvent::RevertAbort { .. })
                    | ("--quit", SemanticEvent::RevertQuit { .. })
                    | ("--skip", SemanticEvent::RevertSkip { .. })
            )));
        }
    }

    #[test]
    fn cherry_pick_conflict_and_terminal_controls_are_distinct() {
        let analyzer = HistoryAnalyzer;
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
        )]);
        let mut conflict = command("cherry-pick", &["git", "cherry-pick", "source"]);
        conflict.exit_code = 1;
        conflict.ref_changes.clear();
        let result = analyzer
            .analyze(&conflict, AnalysisView { refs: &refs })
            .unwrap();
        assert!(matches!(
            result.events.as_slice(),
            [SemanticEvent::CherryPickPrepared { head }]
                if head == "1111111111111111111111111111111111111111"
        ));

        for flag in ["--abort", "--quit", "--skip"] {
            let mut cmd = command("cherry-pick", &["git", "cherry-pick", flag]);
            cmd.ref_changes.clear();
            let result = analyzer
                .analyze(&cmd, AnalysisView { refs: &refs })
                .unwrap();
            assert!(result.events.iter().any(|event| matches!(
                (flag, event),
                ("--abort", SemanticEvent::CherryPickAbort { .. })
                    | ("--quit", SemanticEvent::CherryPickQuit { .. })
                    | ("--skip", SemanticEvent::CherryPickSkip { .. })
            )));
        }
    }

    #[test]
    fn rebase_prepared_quit_and_skip_events_are_explicit() {
        let analyzer = HistoryAnalyzer;
        let refs = std::collections::HashMap::from([(
            "HEAD".to_string(),
            "2222222222222222222222222222222222222222".to_string(),
        )]);
        let mut conflict = command("rebase", &["git", "rebase", "main"]);
        conflict.exit_code = 1;
        conflict.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "1111111111111111111111111111111111111111".to_string(),
            new: "2222222222222222222222222222222222222222".to_string(),
        }];
        let result = analyzer
            .analyze(&conflict, AnalysisView { refs: &refs })
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::RebasePrepared { head }
                if head == "2222222222222222222222222222222222222222"
        )));

        for (flag, expected_quit) in [("--quit", true), ("--skip", false)] {
            let mut cmd = command("rebase", &["git", "rebase", flag]);
            cmd.ref_changes.clear();
            let result = analyzer
                .analyze(&cmd, AnalysisView { refs: &refs })
                .unwrap();
            assert!(result.events.iter().any(|event| match event {
                SemanticEvent::RebaseQuit { .. } if expected_quit => true,
                SemanticEvent::RebaseSkip { .. } if !expected_quit => true,
                _ => false,
            }));
        }
    }

    #[test]
    fn commit_without_amend_emits_commit_created() {
        let analyzer = HistoryAnalyzer;
        let result = analyzer
            .analyze(
                &command("commit", &["git", "commit", "-m", "x"]),
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, SemanticEvent::CommitCreated { .. }))
        );
    }

    #[test]
    fn am_emits_one_batch_event_with_every_head_destination() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("am", &["git", "am", "series.patch"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "1111111111111111111111111111111111111111".to_string(),
                new: "2222222222222222222222222222222222222222".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "2222222222222222222222222222222222222222".to_string(),
                new: "3333333333333333333333333333333333333333".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            &result.events[0],
            SemanticEvent::AmApplied {
                original_head,
                new_commits,
            } if original_head == "1111111111111111111111111111111111111111"
                && new_commits == &vec![
                    "2222222222222222222222222222222222222222".to_string(),
                    "3333333333333333333333333333333333333333".to_string(),
                ]
        ));
    }

    #[test]
    fn am_abort_is_a_hard_reset_not_an_applied_patch() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("am", &["git", "am", "--abort"]);
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "2222222222222222222222222222222222222222".to_string(),
            new: "1111111111111111111111111111111111111111".to_string(),
        }];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            &result.events[0],
            SemanticEvent::Reset {
                kind: ResetKind::Hard,
                old_head,
                new_head,
            } if old_head == "2222222222222222222222222222222222222222"
                && new_head == "1111111111111111111111111111111111111111"
        ));
    }

    #[test]
    fn amend_prefers_head_transition_over_zero_old_branch_change() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "--amend", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::CommitAmended { old_head, new_head }
                if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )));
    }

    #[test]
    fn amend_prefers_head_transition_over_contaminated_branch_hint() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "--amend", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
            },
            RefChange {
                reference: "refs/heads/child".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
            },
            RefChange {
                reference: "refs/heads/parent".to_string(),
                old: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                new: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::CommitAmended { old_head, new_head }
                if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "dddddddddddddddddddddddddddddddddddddddd"
        )));
    }

    #[test]
    fn amend_uses_first_head_transition_when_later_head_moves_are_captured() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "--amend", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
                new: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::CommitAmended { old_head, new_head }
                if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "dddddddddddddddddddddddddddddddddddddddd"
        )));
    }

    #[test]
    fn reset_emits_reset_kind() {
        let analyzer = HistoryAnalyzer;
        let result = analyzer
            .analyze(
                &command("reset", &["git", "reset", "--hard", "HEAD~1"]),
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            SemanticEvent::Reset {
                kind: ResetKind::Hard,
                ..
            }
        )));
    }

    #[test]
    fn commit_without_ref_transition_is_opaque() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert_only_opaque(&result);
    }

    #[test]
    fn commit_without_ref_transition_ignores_family_refs() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            "wrong-family-head".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();

        assert_only_opaque(&result);
    }

    #[test]
    fn commit_without_ref_transition_ignores_family_head() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([(
            "refs/heads/main".to_string(),
            "old-head".to_string(),
        )]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert_only_opaque(&result);
    }

    #[test]
    fn commit_without_ref_transition_does_not_read_head_reflog() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes.clear();

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert_only_opaque(&result);
    }

    #[test]
    fn commit_prefers_head_transition_over_other_branch_ref_changes() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("commit", &["git", "-C", "/repo-b", "commit", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/branch-a".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
            RefChange {
                reference: "refs/heads/branch-b".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "0000000000000000000000000000000000000000".to_string(),
                new: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            },
        ];
        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CommitCreated {
                    new_head,
                    ..
                } if new_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            "expected commit-created event to use the captured HEAD transition, got {:?}",
            result.events
        );
    }

    #[test]
    fn head_change_prefers_head_transition_over_branch_ref_change() {
        let mut cmd = command("commit", &["git", "commit", "-m", "x"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "old-head".to_string(),
                new: "wrong-head".to_string(),
            },
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "old-main".to_string(),
                new: "new-main".to_string(),
            },
        ];
        let change = head_change(&cmd, &Default::default());
        assert_eq!(
            change,
            Some(("old-head".to_string(), "wrong-head".to_string())),
            "expected captured HEAD transition to win over branch ref changes"
        );
    }

    #[test]
    fn rebase_continue_prefers_branch_ref_change_over_head_span() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("rebase", &["git", "rebase", "--continue"]);
        cmd.ref_changes = vec![
            RefChange {
                reference: "refs/heads/feature".to_string(),
                old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                new: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
        ];
        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();
        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::RebaseComplete {
                    old_head,
                    new_head,
                    ..
                } if old_head == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    && new_head == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            "expected rebase-complete to use branch ref span, got {:?}",
            result.events
        );
    }

    #[test]
    fn cherry_pick_uses_full_head_ref_change_span() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command(
            "cherry-pick",
            &["git", "cherry-pick", "source-1", "source-2", "source-3"],
        );
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "a".to_string(),
                new: "b".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "b".to_string(),
                new: "c".to_string(),
            },
            RefChange {
                reference: "HEAD".to_string(),
                old: "c".to_string(),
                new: "d".to_string(),
            },
        ];

        let result = analyzer
            .analyze(
                &cmd,
                AnalysisView {
                    refs: &Default::default(),
                },
            )
            .unwrap();

        assert!(
            result.events.iter().any(|event| matches!(
                event,
                SemanticEvent::CherryPickComplete {
                    original_head,
                    new_head,
                    ..
                } if original_head == "a" && new_head == "d"
            )),
            "expected cherry-pick span event, got {:?}",
            result.events
        );
    }

    #[test]
    fn cherry_pick_without_ref_transition_is_opaque() {
        let analyzer = HistoryAnalyzer;
        let mut cmd = command("cherry-pick", &["git", "cherry-pick", "--continue"]);
        cmd.ref_changes.clear();
        let refs = std::collections::HashMap::from([
            ("HEAD".to_string(), "old-head".to_string()),
            ("refs/heads/main".to_string(), "old-head".to_string()),
        ]);

        let result = analyzer
            .analyze(&cmd, AnalysisView { refs: &refs })
            .unwrap();
        assert_only_opaque(&result);
    }
}
