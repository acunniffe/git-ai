use super::*;

pub(super) fn current_worktree_branch_ref<'a>(
    cmd: &NormalizedCommand,
    state: &'a FamilyState,
) -> Option<&'a str> {
    let worktree = cmd.worktree.as_ref()?;
    let canonical = crate::operations::git::canonicalize::canonicalize_or_self(worktree);
    state
        .worktrees
        .get(&canonical)
        .or_else(|| state.worktrees.get(worktree))
        .and_then(|worktree| worktree.branch.as_deref())
}

pub(super) fn commit_reflog_messages(args: &[String], amend: bool) -> HashSet<String> {
    let Some(subject) = commit_subject_from_args(args) else {
        return HashSet::new();
    };
    let modes = if amend {
        ["commit (amend):"].as_slice()
    } else {
        [
            "commit:",
            "commit (initial):",
            "commit (merge):",
            "commit (cherry-pick):",
            "commit (revert):",
        ]
        .as_slice()
    };
    modes
        .iter()
        .map(|mode| format!("{} {}", mode, subject))
        .collect()
}

pub(super) fn reset_reflog_messages(args: &[String]) -> HashSet<String> {
    let Some(target) = reset_target_arg(args) else {
        return HashSet::new();
    };
    [format!("reset: moving to {target}")].into_iter().collect()
}

pub(super) fn reset_target_arg(args: &[String]) -> Option<String> {
    let mut idx = if args.first().is_some_and(|arg| arg == "reset") {
        1
    } else {
        0
    };
    while idx < args.len() {
        match args[idx].as_str() {
            "--" => return None,
            "--pathspec-from-file" => idx += 2,
            value if value.starts_with("--pathspec-from-file=") => idx += 1,
            value if value.starts_with('-') => idx += 1,
            value => return Some(value.to_string()),
        }
    }
    None
}

pub(super) fn commit_subject_from_args(args: &[String]) -> Option<String> {
    let mut idx = if args.first().is_some_and(|arg| arg == "commit") {
        1
    } else {
        0
    };
    while idx < args.len() {
        let arg = &args[idx];
        match arg.as_str() {
            "-m" | "--message" => {
                return args.get(idx + 1).and_then(|value| commit_subject(value));
            }
            value if value.starts_with("--message=") => {
                return value.strip_prefix("--message=").and_then(commit_subject);
            }
            value if value.starts_with("-m") && value.len() > 2 => {
                return commit_subject(&value[2..]);
            }
            "--" => return None,
            _ => idx += 1,
        }
    }
    None
}

pub(super) fn commit_subject(message: &str) -> Option<String> {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim_end_matches(|character: char| character.is_ascii_whitespace())
                .to_string()
        })
}

pub(super) fn resolve_cherry_pick_source_oids_from_sources(
    _cmd: &NormalizedCommand,
    state: &FamilyState,
    sources: &[&str],
) -> Result<Option<Vec<String>>, GitAiError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for source in sources {
        let Some(oid) = resolve_cherry_pick_source_from_state(source, &state.refs) else {
            return Ok(None);
        };
        if seen.insert(oid.clone()) {
            out.push(oid);
        }
    }

    Ok(Some(out))
}

pub(super) fn cherry_pick_source_is_range(source: &str) -> bool {
    source.contains("..")
}

pub(super) fn concretize_revision_expr(
    expr: &str,
    refs: &HashMap<String, String>,
) -> Option<String> {
    if expr.is_empty() {
        return refs.get("HEAD").cloned();
    }
    if is_valid_git_oid(expr) || is_hex_oid_prefix(expr) {
        return Some(expr.to_string());
    }
    if let Some(oid) = resolve_ref_from_state(expr, refs) {
        return Some(oid);
    }
    let (base, suffix) = split_revision_suffix(expr);
    if suffix.is_empty() {
        return None;
    }
    let base_oid = if base.is_empty() {
        refs.get("HEAD").cloned()
    } else if is_valid_git_oid(base) || is_hex_oid_prefix(base) {
        Some(base.to_string())
    } else {
        resolve_ref_from_state(base, refs)
    }?;
    Some(format!("{base_oid}{suffix}"))
}

pub(super) fn resolve_cherry_pick_source_from_state(
    source: &str,
    refs: &HashMap<String, String>,
) -> Option<String> {
    if cherry_pick_source_is_range(source) {
        return None;
    }

    concretize_revision_expr(source, refs).filter(|oid| valid_non_zero_oid(oid))
}

pub(super) fn split_revision_suffix(expr: &str) -> (&str, &str) {
    let idx = expr
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '~' | '^').then_some(idx))
        .unwrap_or(expr.len());
    expr.split_at(idx)
}

pub(super) fn resolve_ref_from_state(name: &str, refs: &HashMap<String, String>) -> Option<String> {
    if name == "HEAD" || name == "@" {
        return refs
            .get("HEAD")
            .filter(|oid| valid_non_zero_oid(oid))
            .cloned();
    }
    if let Some(value) = refs.get(name).filter(|oid| valid_non_zero_oid(oid)) {
        return Some(value.clone());
    }
    for candidate in [
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/tags/{name}"),
    ] {
        if let Some(value) = refs.get(&candidate).filter(|oid| valid_non_zero_oid(oid)) {
            return Some(value.clone());
        }
    }
    None
}

pub(super) fn is_hex_oid_prefix(value: &str) -> bool {
    (4..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

pub(super) fn pull_reflog_action(cmd: &NormalizedCommand) -> String {
    let raw_args = normalized_args(&cmd.raw_argv);
    let parsed = parse_git_cli_args(&raw_args);
    let args = if parsed.command.as_deref() == Some("pull") {
        parsed.command_args
    } else if cmd.invoked_command.as_deref() == Some("pull") {
        cmd.invoked_args.clone()
    } else {
        command_args(cmd)
    };
    let args = pull_command_args(&args);
    if args.is_empty() {
        "pull".to_string()
    } else {
        std::iter::once("pull")
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(super) fn pull_command_args(args: &[String]) -> &[String] {
    if args.first().is_some_and(|arg| arg == "pull") {
        &args[1..]
    } else {
        args
    }
}

pub(super) fn pull_reflog_message_prefixes(action: &str) -> Vec<String> {
    if action == "pull" {
        return vec!["pull:".to_string(), "pull (".to_string()];
    }
    vec![format!("{}:", action), format!("{} ", action)]
}

pub(super) fn rebase_start_checkout_target_from_args(args: &[String]) -> Option<String> {
    let summary = summarize_rebase_args(args);
    if summary.is_control_mode {
        return None;
    }
    summary
        .onto_spec
        .or_else(|| summary.positionals.first().cloned())
}

pub(super) fn rebase_start_message_targets(message: &str, target: &str) -> bool {
    message
        .strip_prefix("rebase (start): checkout ")
        .is_some_and(|message_target| message_target == target)
}

pub(super) fn rebase_finish_returns_to_branch(message: &str, branch_ref: &str) -> bool {
    message == format!("rebase (finish): returning to {}", branch_ref)
}

pub(super) fn rebase_finish_returned_branch(message: &str) -> Option<&str> {
    message.strip_prefix("rebase (finish): returning to ")
}

pub(super) fn rebase_branch_finish_message_is(message: &str, branch_ref: &str) -> bool {
    message.starts_with(&format!("rebase (finish): {}", branch_ref))
}

pub(super) fn pull_finish_returned_branch(message: &str, action: &str) -> Option<String> {
    message
        .strip_prefix(&format!("{action} (finish): returning to "))
        .map(ToOwned::to_owned)
}

pub(super) fn latest_rebase_finish_for_branch<'a>(
    entries: &'a [CursorEntry],
    branch_ref: &str,
) -> Option<&'a CursorEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| rebase_finish_returns_to_branch(&entry.message, branch_ref))
}

pub(super) fn rebase_start_marker_for_explicit_branch<'a>(
    entries: &'a [CursorEntry],
    branch_ref: &str,
) -> Option<&'a CursorEntry> {
    if let Some(finish) = latest_rebase_finish_for_branch(entries, branch_ref)
        && let Some(start) = entries.iter().rev().find(|entry| {
            entry.end_offset < finish.end_offset && rebase_reflog_action_is(&entry.message, "start")
        })
    {
        return Some(start);
    }

    entries
        .iter()
        .rev()
        .find(|entry| rebase_reflog_action_is(&entry.message, "start"))
}

pub(super) fn working_log_base_oids(worktree: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(repo) = find_repository_in_path(&worktree.to_string_lossy()) else {
        return out;
    };
    let Ok(entries) = fs::read_dir(&repo.storage.working_logs) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "initial" {
            out.insert("0000000000000000000000000000000000000000".to_string());
        } else if valid_non_zero_oid(&name) {
            out.insert(name);
        }
    }
    out
}

pub(super) fn checkout_is_path_checkout(cmd: &NormalizedCommand) -> bool {
    let args = command_args(cmd);
    args.iter().any(|arg| arg == "--")
        || args
            .iter()
            .any(|arg| arg.starts_with("--pathspec") || arg == "--ours" || arg == "--theirs")
}

pub(super) fn rebase_command_args(cmd: &NormalizedCommand) -> Vec<String> {
    let args = command_args(cmd);
    if args.first().is_some_and(|arg| arg == "rebase") {
        args[1..].to_vec()
    } else {
        args
    }
}

pub(super) fn command_uses_ref_cursor(primary: &str) -> bool {
    matches!(
        primary,
        "commit"
            | "revert"
            | "reset"
            | "checkout"
            | "switch"
            | "merge"
            | "cherry-pick"
            | "rebase"
            | "pull"
            | "branch"
            | "stash"
            | "update-ref"
    )
}

pub(super) fn command_can_move_refs_on_nonzero(primary: Option<&str>) -> bool {
    matches!(
        primary,
        Some("checkout" | "switch" | "stash" | "rebase" | "pull" | "branch" | "cherry-pick")
    )
}

pub(super) fn command_can_clamp_non_authoritative_cold_seed(cmd: &NormalizedCommand) -> bool {
    matches!(cmd.primary_command.as_deref(), Some("cherry-pick"))
}

pub(super) fn message_matches(message: &str, prefixes: &[&str]) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|prefix| message.starts_with(prefix))
}

pub(super) fn valid_ref_transition(old: &str, new: &str) -> bool {
    is_valid_git_oid(old) && is_valid_git_oid(new) && old != new
}

pub(super) fn zero_oid() -> String {
    "0000000000000000000000000000000000000000".to_string()
}

pub(super) fn reflog_timestamp_window(cmd: &NormalizedCommand) -> ReflogTimestampWindow {
    let start = unix_nanos_to_reflog_secs(cmd.started_at_ns).saturating_sub(1);
    let end = unix_nanos_to_reflog_secs(cmd.finished_at_ns).saturating_add(1);
    ReflogTimestampWindow {
        start_secs: start.min(end),
        end_secs: start.max(end),
    }
}

pub(super) fn unix_nanos_to_reflog_secs(value: u128) -> i64 {
    i64::try_from(value / 1_000_000_000).unwrap_or(i64::MAX)
}

pub(super) fn entry_to_ref_change(entry: &CursorEntry) -> RefChange {
    RefChange {
        reference: entry.reference.clone(),
        old: entry.old.clone(),
        new: entry.new.clone(),
    }
}

pub(super) fn dedup_ref_changes(changes: &mut Vec<RefChange>) {
    let mut seen = HashSet::new();
    changes.retain(|change| {
        seen.insert((
            change.reference.clone(),
            change.old.clone(),
            change.new.clone(),
        ))
    });
}

pub(super) fn common_key(reference: &str) -> String {
    format!("common:{}", reference)
}

pub(super) fn branch_arg_to_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{}", branch)
    }
}

pub(super) fn head_key(git_dir: &Path) -> String {
    let normalized = crate::operations::git::canonicalize::canonicalize_or_self(git_dir)
        .to_string_lossy()
        .to_string();
    format!("worktree:{}:HEAD", normalized)
}

#[cfg(test)]
mod tests {
    use super::is_hex_oid_prefix;
    use crate::operations::git::oid::is_full_oid;

    #[test]
    fn oid_prefixes_remain_distinct_from_full_oids() {
        for len in [4, 39, 41, 63] {
            let value = "a".repeat(len);
            assert!(is_hex_oid_prefix(&value), "expected {len}-byte prefix");
            assert!(!is_full_oid(&value), "unexpected {len}-byte full OID");
        }

        for len in [40, 64] {
            let value = "A".repeat(len);
            assert!(is_hex_oid_prefix(&value), "expected {len}-byte prefix");
            assert!(is_full_oid(&value), "expected {len}-byte full OID");
        }

        for value in [
            "a".repeat(3),
            "a".repeat(65),
            format!("{}g", "a".repeat(39)),
        ] {
            assert!(!is_hex_oid_prefix(&value));
            assert!(!is_full_oid(&value));
        }
    }
}
