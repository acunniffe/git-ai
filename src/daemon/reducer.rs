use crate::daemon::analyzers::{AnalysisView, AnalyzerRegistry};
use crate::daemon::domain::{
    AnalysisResult, AppliedCommand, FamilyState, GlobalState, NormalizedCommand, WorktreeState,
};
use crate::error::GitAiError;
use std::path::{Path, PathBuf};

pub fn reduce_family_command(
    state: &mut FamilyState,
    cmd: NormalizedCommand,
    analyzers: &AnalyzerRegistry,
) -> Result<(AppliedCommand, AnalysisResult), GitAiError> {
    reduce_family_command_with_ref_snapshot(
        state,
        cmd,
        analyzers,
        &std::collections::HashMap::new(),
    )
}

pub fn reduce_family_command_with_ref_snapshot(
    state: &mut FamilyState,
    cmd: NormalizedCommand,
    analyzers: &AnalyzerRegistry,
    command_start_refs: &std::collections::HashMap<String, String>,
) -> Result<(AppliedCommand, AnalysisResult), GitAiError> {
    // Analyze against pre-command state so history/ref analyzers can infer old->new correctly.
    let refs_for_analysis;
    let analysis_refs = if command_start_refs.is_empty() {
        &state.refs
    } else {
        refs_for_analysis = state
            .refs
            .iter()
            .map(|(reference, oid)| (reference.clone(), oid.clone()))
            .chain(
                command_start_refs
                    .iter()
                    .map(|(reference, oid)| (reference.clone(), oid.clone())),
            )
            .collect();
        &refs_for_analysis
    };
    let analysis = analyzers.analyze(
        &cmd,
        AnalysisView {
            refs: analysis_refs,
        },
    )?;
    apply_ref_changes(state, &cmd);
    apply_worktree_state(state, &cmd);

    state.applied_seq = state.applied_seq.saturating_add(1);
    let applied = AppliedCommand {
        seq: state.applied_seq,
        command: cmd,
        analysis: analysis.clone(),
    };
    Ok((applied, analysis))
}

pub fn reduce_global_command(
    state: &mut GlobalState,
    cmd: NormalizedCommand,
    analyzers: &AnalyzerRegistry,
) -> Result<(AppliedCommand, AnalysisResult), GitAiError> {
    let empty_refs = std::collections::HashMap::new();
    let analysis = analyzers.analyze(&cmd, AnalysisView { refs: &empty_refs })?;
    state.applied_seq = state.applied_seq.saturating_add(1);
    let applied = AppliedCommand {
        seq: state.applied_seq,
        command: cmd,
        analysis: analysis.clone(),
    };
    Ok((applied, analysis))
}

pub fn reduce_checkpoint(state: &mut FamilyState) {
    state.applied_seq = state.applied_seq.saturating_add(1);
}

fn apply_ref_changes(state: &mut FamilyState, cmd: &NormalizedCommand) {
    for change in &cmd.ref_changes {
        if change.new.trim().is_empty() || is_zero_oid(&change.new) {
            state.refs.remove(&change.reference);
        } else {
            state
                .refs
                .insert(change.reference.clone(), change.new.clone());
        }
    }
}

fn is_zero_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|ch| ch == '0')
}

fn apply_worktree_state(state: &mut FamilyState, cmd: &NormalizedCommand) {
    let Some(worktree) = cmd.worktree.as_ref() else {
        return;
    };
    let key = canonicalize_path(worktree);
    let previous = state.worktrees.get(&key);
    let head_change = cmd
        .ref_changes
        .iter()
        .rfind(|change| change.reference == "HEAD");

    let (head, branch, detached) = if let Some(head_change) = head_change {
        // DEFERRED (code-review #12): `detached` is inferred as "no unique
        // branch ref moved with HEAD". When a checkout/switch to an EXISTING
        // branch produces an ambiguous ref-change pairing (e.g. multiple
        // refs/heads/* share the same old->new as HEAD, so
        // unique_branch_for_head_change returns None), the worktree is
        // misclassified as detached. Harmless for attribution today (the head
        // OID is still correct); a precise fix would consult the actual
        // post-command symbolic-ref/branch name rather than inferring from
        // ref-change pairing.
        let branch = unique_branch_for_head_change(cmd, head_change);
        (
            Some(head_change.new.clone()),
            branch.clone(),
            branch.is_none(),
        )
    } else if let Some(branch) = checkout_or_switch_branch_target(cmd) {
        (
            previous.and_then(|worktree| worktree.head.clone()),
            Some(branch),
            false,
        )
    } else {
        (
            previous.and_then(|worktree| worktree.head.clone()),
            previous.and_then(|worktree| worktree.branch.clone()),
            previous.is_some_and(|worktree| worktree.detached),
        )
    };

    state.worktrees.insert(
        key,
        WorktreeState {
            head,
            branch,
            detached,
            last_updated_ns: cmd.finished_at_ns,
        },
    );
}

fn unique_branch_for_head_change(
    cmd: &NormalizedCommand,
    head_change: &crate::daemon::domain::RefChange,
) -> Option<String> {
    let mut matches = cmd
        .ref_changes
        .iter()
        .filter(|change| {
            change.reference.starts_with("refs/heads/")
                && change.old == head_change.old
                && change.new == head_change.new
        })
        .map(|change| change.reference.clone());
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

fn checkout_or_switch_branch_target(cmd: &NormalizedCommand) -> Option<String> {
    let command = cmd.primary_command.as_deref()?;
    let args = command_args(cmd);
    match command {
        "checkout" => checkout_created_branch_target(&args),
        "switch" => switch_branch_target(&args),
        _ => None,
    }
    .map(|branch| {
        if branch.starts_with("refs/") {
            branch
        } else {
            format!("refs/heads/{branch}")
        }
    })
}

fn command_args(cmd: &NormalizedCommand) -> Vec<String> {
    if !cmd.invoked_args.is_empty() {
        let mut args = vec![
            cmd.invoked_command
                .clone()
                .or_else(|| cmd.primary_command.clone())
                .unwrap_or_default(),
        ];
        args.extend(cmd.invoked_args.clone());
        return args;
    }
    cmd.raw_argv
        .iter()
        .skip_while(|arg| arg.as_str() != cmd.primary_command.as_deref().unwrap_or(""))
        .cloned()
        .collect()
}

fn checkout_created_branch_target(args: &[String]) -> Option<String> {
    let mut idx = usize::from(args.first().is_some_and(|arg| arg == "checkout"));
    while idx < args.len() {
        match args[idx].as_str() {
            "-b" | "-B" => return args.get(idx + 1).cloned(),
            value if value.starts_with("-b") && value.len() > 2 => {
                return Some(value[2..].to_string());
            }
            value if value.starts_with("-B") && value.len() > 2 => {
                return Some(value[2..].to_string());
            }
            "--" => return None,
            _ => idx += 1,
        }
    }
    None
}

fn switch_branch_target(args: &[String]) -> Option<String> {
    let mut idx = usize::from(args.first().is_some_and(|arg| arg == "switch"));
    while idx < args.len() {
        match args[idx].as_str() {
            "-c" | "-C" | "--create" | "--force-create" => return args.get(idx + 1).cloned(),
            value if value.starts_with("--create=") => {
                return Some(value["--create=".len()..].to_string());
            }
            value if value.starts_with("--force-create=") => {
                return Some(value["--force-create=".len()..].to_string());
            }
            value if value.starts_with("-c") && value.len() > 2 => {
                return Some(value[2..].to_string());
            }
            value if value.starts_with("-C") && value.len() > 2 => {
                return Some(value[2..].to_string());
            }
            "--detach" | "-d" | "--" => return None,
            value if !value.starts_with('-') => return Some(value.to_string()),
            _ => idx += 1,
        }
    }
    None
}

fn canonicalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::analyzers::AnalyzerRegistry;
    use crate::daemon::domain::{
        CommandScope, Confidence, FamilyKey, FamilyState, GlobalState, RefChange, WatermarkState,
        WorktreeState,
    };
    use std::collections::HashMap;

    fn family_state() -> FamilyState {
        FamilyState {
            family_key: FamilyKey::new("family:/tmp/repo"),
            refs: HashMap::new(),
            worktrees: HashMap::new(),
            last_error: None,
            applied_seq: 0,
            watermarks: WatermarkState::default(),
        }
    }

    fn normalized() -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Family(FamilyKey::new("family:/tmp/repo")),
            family_key: Some(FamilyKey::new("family:/tmp/repo")),
            worktree: Some(PathBuf::from("/tmp/repo")),
            root_sid: "sid".to_string(),
            raw_argv: vec!["git".to_string(), "update-ref".to_string()],
            primary_command: Some("update-ref".to_string()),
            invoked_command: Some("update-ref".to_string()),
            invoked_args: Vec::new(),
            observed_child_commands: Vec::new(),
            exit_code: 0,
            started_at_ns: 1,
            finished_at_ns: 2,
            reflog_start_offsets: std::collections::HashMap::new(),
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes: vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: "".to_string(),
                new: "abc".to_string(),
            }],
            confidence: Confidence::Low,
        }
    }

    #[test]
    fn reducer_applies_ref_changes_and_produces_applied_command() {
        let mut state = family_state();
        let registry = AnalyzerRegistry::new();
        let (applied, analysis) =
            reduce_family_command(&mut state, normalized(), &registry).unwrap();
        assert_eq!(applied.seq, 1);
        assert!(matches!(
            analysis.class,
            crate::daemon::domain::CommandClass::HistoryRewrite
        ));
        assert_eq!(
            state.refs.get("refs/heads/main").map(String::as_str),
            Some("abc")
        );
    }

    #[test]
    fn reducer_does_not_update_refs_without_ref_transition_for_head_moving_commands() {
        let mut state = family_state();
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes.clear();
        cmd.raw_argv = vec!["git".to_string(), "commit".to_string()];
        cmd.primary_command = Some("commit".to_string());
        cmd.invoked_command = Some("commit".to_string());

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();

        assert_eq!(state.refs.get("refs/heads/main").map(String::as_str), None);
    }

    #[test]
    fn reducer_preserves_refs_for_stash_without_ref_transition() {
        let mut state = family_state();
        state
            .refs
            .insert("refs/heads/main".to_string(), "abc".to_string());
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes.clear();
        cmd.raw_argv = vec!["git".to_string(), "stash".to_string(), "push".to_string()];
        cmd.primary_command = Some("stash".to_string());
        cmd.invoked_command = Some("stash".to_string());
        cmd.invoked_args = vec!["push".to_string()];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();

        assert_eq!(
            state.refs.get("refs/heads/main").map(String::as_str),
            Some("abc")
        );
    }

    #[test]
    fn reducer_removes_refs_deleted_with_zero_oid() {
        let mut state = family_state();
        state.refs.insert(
            "refs/heads/feature".to_string(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes = vec![RefChange {
            reference: "refs/heads/feature".to_string(),
            old: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            new: "0000000000000000000000000000000000000000".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();

        assert!(!state.refs.contains_key("refs/heads/feature"));
    }

    #[test]
    fn reducer_records_worktree_branch_from_unique_head_branch_transition() {
        let mut state = family_state();
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes = vec![
            RefChange {
                reference: "HEAD".to_string(),
                old: "aaa".to_string(),
                new: "bbb".to_string(),
            },
            RefChange {
                reference: "refs/heads/main".to_string(),
                old: "aaa".to_string(),
                new: "bbb".to_string(),
            },
        ];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("bbb"));
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/main"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_preserves_worktree_branch_when_command_does_not_move_head() {
        let mut state = family_state();
        state.worktrees.insert(
            PathBuf::from("/tmp/repo"),
            WorktreeState {
                head: Some("aaa".to_string()),
                branch: Some("refs/heads/main".to_string()),
                detached: false,
                last_updated_ns: 1,
            },
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes = vec![RefChange {
            reference: "refs/heads/other".to_string(),
            old: "ccc".to_string(),
            new: "ddd".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("aaa"));
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/main"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_updates_branch_for_checkout_new_branch_without_head_oid_move() {
        let mut state = family_state();
        state.worktrees.insert(
            PathBuf::from("/tmp/repo"),
            WorktreeState {
                head: Some("aaa".to_string()),
                branch: Some("refs/heads/main".to_string()),
                detached: false,
                last_updated_ns: 1,
            },
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.raw_argv = vec![
            "git".to_string(),
            "checkout".to_string(),
            "-b".to_string(),
            "feature".to_string(),
        ];
        cmd.primary_command = Some("checkout".to_string());
        cmd.invoked_command = Some("checkout".to_string());
        cmd.invoked_args = vec!["-b".to_string(), "feature".to_string()];
        cmd.ref_changes.clear();

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("aaa"));
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/feature"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_marks_head_only_transition_as_detached_or_unknown_branch() {
        let mut state = family_state();
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "aaa".to_string(),
            new: "bbb".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("bbb"));
        assert_eq!(worktree.branch, None);
        assert!(worktree.detached);
    }

    #[test]
    fn global_reducer_never_drops_commands() {
        let mut state = GlobalState { applied_seq: 0 };
        let registry = AnalyzerRegistry::new();
        let (applied, _analysis) =
            reduce_global_command(&mut state, normalized(), &registry).unwrap();
        assert_eq!(applied.seq, 1);
        assert_eq!(state.applied_seq, 1);
    }
}
