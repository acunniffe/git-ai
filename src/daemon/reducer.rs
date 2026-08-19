use crate::daemon::analyzers::{AnalysisView, AnalyzerRegistry, checkout_is_path_checkout};
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
    // Analyze against pre-command state so history/ref analyzers can infer old->new correctly.
    let analysis = analyzers.analyze(&cmd, AnalysisView { refs: &state.refs })?;
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
    let orphan_branch = (cmd.exit_code == 0)
        .then(|| checkout_orphan_branch_target(cmd))
        .flatten();
    let command_branch = checkout_or_switch_branch_target(cmd, state);

    let (head, branch, detached) = if let Some(branch) = orphan_branch {
        (None, Some(branch), false)
    } else if let Some(head_change) = head_change {
        let branch = command_branch.or_else(|| unique_branch_for_head_change(cmd, head_change));
        (
            Some(head_change.new.clone()),
            branch.clone(),
            branch.is_none(),
        )
    } else if let Some(branch) = command_branch {
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

    if cmd.exit_code == 0
        && let Some(created_branch) = checkout_or_switch_created_branch_target(cmd)
        && let Some(head) = head.as_ref()
    {
        state.refs.insert(created_branch, head.clone());
    }

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

fn checkout_or_switch_branch_target(
    cmd: &NormalizedCommand,
    state: &FamilyState,
) -> Option<String> {
    if cmd.exit_code != 0
        && !cmd
            .ref_changes
            .iter()
            .any(|change| change.reference == "HEAD")
    {
        return None;
    }
    let command = cmd.primary_command.as_deref()?;
    let args = command_args(cmd);
    match command {
        "checkout" if !checkout_is_path_checkout(cmd) => checkout_branch_target(&args, state),
        "switch" => switch_branch_target(&args),
        _ => None,
    }
    .map(qualify_branch_ref)
}

fn checkout_or_switch_created_branch_target(cmd: &NormalizedCommand) -> Option<String> {
    let command = cmd.primary_command.as_deref()?;
    let args = command_args(cmd);
    let branch = match command {
        "checkout" => checkout_created_branch_target(&args),
        "switch" => switch_created_branch_target(&args),
        _ => None,
    }?;
    Some(qualify_branch_ref(branch))
}

fn checkout_orphan_branch_target(cmd: &NormalizedCommand) -> Option<String> {
    if cmd.primary_command.as_deref() != Some("checkout") {
        return None;
    }
    checkout_orphan_target(&command_args(cmd)).map(qualify_branch_ref)
}

fn qualify_branch_ref(branch: String) -> String {
    if branch.starts_with("refs/") {
        branch
    } else {
        format!("refs/heads/{branch}")
    }
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

fn checkout_branch_target(args: &[String], state: &FamilyState) -> Option<String> {
    if let Some(created) = checkout_created_branch_target(args) {
        return Some(created);
    }
    let mut idx = usize::from(args.first().is_some_and(|arg| arg == "checkout"));
    let mut candidate = None;
    while idx < args.len() {
        match args[idx].as_str() {
            "--detach" | "-d" | "--" => return None,
            "--conflict" => idx += 2,
            value if value.starts_with("--conflict=") => idx += 1,
            value if value.starts_with('-') => idx += 1,
            value => {
                candidate = Some(value.to_string());
                break;
            }
        }
    }
    let candidate = candidate?;
    let reference = if candidate.starts_with("refs/heads/") {
        candidate
    } else {
        format!("refs/heads/{candidate}")
    };
    state.refs.contains_key(&reference).then_some(reference)
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

fn checkout_orphan_target(args: &[String]) -> Option<String> {
    let mut idx = usize::from(args.first().is_some_and(|arg| arg == "checkout"));
    while idx < args.len() {
        match args[idx].as_str() {
            "--orphan" => return args.get(idx + 1).cloned(),
            value if value.starts_with("--orphan=") => {
                return Some(value["--orphan=".len()..].to_string());
            }
            "--" => return None,
            _ => idx += 1,
        }
    }
    None
}

fn switch_branch_target(args: &[String]) -> Option<String> {
    if let Some(created) = switch_created_branch_target(args) {
        return Some(created);
    }
    let mut idx = usize::from(args.first().is_some_and(|arg| arg == "switch"));
    while idx < args.len() {
        match args[idx].as_str() {
            "--detach" | "-d" | "--" => return None,
            value if !value.starts_with('-') => return Some(value.to_string()),
            _ => idx += 1,
        }
    }
    None
}

fn switch_created_branch_target(args: &[String]) -> Option<String> {
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
            "--" => return None,
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
        assert_eq!(
            state.refs.get("refs/heads/feature").map(String::as_str),
            Some("aaa")
        );
    }

    #[test]
    fn reducer_updates_branch_for_checkout_existing_branch_with_head_move() {
        let mut state = family_state();
        state
            .refs
            .insert("refs/heads/main".to_string(), "bbb".to_string());
        state.worktrees.insert(
            PathBuf::from("/tmp/repo"),
            WorktreeState {
                head: Some("aaa".to_string()),
                branch: Some("refs/heads/feature".to_string()),
                detached: false,
                last_updated_ns: 1,
            },
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.raw_argv = vec![
            "git".to_string(),
            "checkout".to_string(),
            "main".to_string(),
        ];
        cmd.primary_command = Some("checkout".to_string());
        cmd.invoked_command = Some("checkout".to_string());
        cmd.invoked_args = vec!["main".to_string()];
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "aaa".to_string(),
            new: "bbb".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("bbb"));
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/main"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_tracks_nonzero_merge_checkout_when_head_move_was_observed() {
        let mut state = family_state();
        state
            .refs
            .insert("refs/heads/main".to_string(), "bbb".to_string());
        state.worktrees.insert(
            PathBuf::from("/tmp/repo"),
            WorktreeState {
                head: Some("aaa".to_string()),
                branch: Some("refs/heads/feature".to_string()),
                detached: false,
                last_updated_ns: 1,
            },
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.raw_argv = vec![
            "git".to_string(),
            "checkout".to_string(),
            "--merge".to_string(),
            "main".to_string(),
        ];
        cmd.primary_command = Some("checkout".to_string());
        cmd.invoked_command = Some("checkout".to_string());
        cmd.invoked_args = vec!["--merge".to_string(), "main".to_string()];
        cmd.exit_code = 1;
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "aaa".to_string(),
            new: "bbb".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head.as_deref(), Some("bbb"));
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/main"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_finds_checkout_after_raw_git_global_options() {
        let mut state = family_state();
        state
            .refs
            .insert("refs/heads/main".to_string(), "bbb".to_string());
        state.worktrees.insert(
            PathBuf::from("/tmp/repo"),
            WorktreeState {
                head: Some("aaa".to_string()),
                branch: Some("refs/heads/feature".to_string()),
                detached: false,
                last_updated_ns: 1,
            },
        );
        let registry = AnalyzerRegistry::new();
        let mut cmd = normalized();
        cmd.raw_argv = vec![
            "git".to_string(),
            "-C".to_string(),
            "/tmp/repo".to_string(),
            "checkout".to_string(),
            "main".to_string(),
        ];
        cmd.primary_command = Some("checkout".to_string());
        cmd.invoked_command = Some("checkout".to_string());
        cmd.invoked_args.clear();
        cmd.ref_changes = vec![RefChange {
            reference: "HEAD".to_string(),
            old: "aaa".to_string(),
            new: "bbb".to_string(),
        }];

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/main"));
        assert!(!worktree.detached);
    }

    #[test]
    fn reducer_preserves_branch_for_checkout_path_forms_and_failed_checkout() {
        for (args, exit_code) in [
            (vec!["main", "--", "src/lib.rs"], 0),
            (vec!["main", "--", "checkout"], 0),
            (vec!["main", "src/lib.rs"], 0),
            (vec!["-p", "main"], 0),
            (vec!["main"], 1),
        ] {
            let mut state = family_state();
            state
                .refs
                .insert("refs/heads/main".to_string(), "bbb".to_string());
            state.worktrees.insert(
                PathBuf::from("/tmp/repo"),
                WorktreeState {
                    head: Some("aaa".to_string()),
                    branch: Some("refs/heads/feature".to_string()),
                    detached: false,
                    last_updated_ns: 1,
                },
            );
            let registry = AnalyzerRegistry::new();
            let mut cmd = normalized();
            cmd.raw_argv = std::iter::once("git")
                .chain(std::iter::once("checkout"))
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect();
            cmd.primary_command = Some("checkout".to_string());
            cmd.invoked_command = Some("checkout".to_string());
            cmd.invoked_args = args.iter().map(|arg| arg.to_string()).collect();
            cmd.exit_code = exit_code;
            cmd.ref_changes.clear();

            let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
            let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

            assert_eq!(
                worktree.branch.as_deref(),
                Some("refs/heads/feature"),
                "checkout args {args:?} exit_code={exit_code} changed the tracked branch"
            );
            assert_eq!(worktree.head.as_deref(), Some("aaa"));
            assert!(!worktree.detached);
        }
    }

    #[test]
    fn reducer_tracks_checkout_orphan_as_unborn_without_inventing_a_tip() {
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
            "--orphan".to_string(),
            "empty".to_string(),
        ];
        cmd.primary_command = Some("checkout".to_string());
        cmd.invoked_command = Some("checkout".to_string());
        cmd.invoked_args = vec!["--orphan".to_string(), "empty".to_string()];
        cmd.ref_changes.clear();

        let (_applied, _analysis) = reduce_family_command(&mut state, cmd, &registry).unwrap();
        let worktree = state.worktrees.get(&PathBuf::from("/tmp/repo")).unwrap();

        assert_eq!(worktree.head, None);
        assert_eq!(worktree.branch.as_deref(), Some("refs/heads/empty"));
        assert!(!worktree.detached);
        assert!(!state.refs.contains_key("refs/heads/empty"));
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
