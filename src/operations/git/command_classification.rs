use super::command_policy;

/// Returns true if the given git subcommand is guaranteed to never mutate
/// repository state (refs, objects, config, worktree). Used to skip expensive
/// trace2 ingestion work and suppress trace2 emission for read-only commands.
pub fn is_definitely_read_only_command(command: &str) -> bool {
    command_policy::is_definitely_read_only_command(command)
}

/// Returns true if the full Git invocation is guaranteed to never mutate
/// repository state. This is intentionally conservative: unknown flags on
/// mixed read/write commands are treated as potentially mutating.
///
/// Extends `is_definitely_read_only_command` to handle commands like `stash`,
/// `worktree`, and `notes` whose read-only status depends on the subcommand:
/// - `git stash list` / `git stash show` are read-only; `pop`/`apply` are not
/// - `git worktree list` is read-only; `add`/`remove` are not
/// - `git notes show` / `git notes list` / `git notes get-ref` are read-only;
///   `add`/`append`/`remove` are not
///
/// IDEs like Zed issue thousands of `stash list` and `worktree list` calls
/// per minute for their git panel UI. These must be identified as read-only
/// so the trace2 pipeline can drop them without processing.
pub fn is_definitely_read_only_git_invocation(command: &str, command_args: &[String]) -> bool {
    if is_definitely_read_only_command(command) {
        return true;
    }

    match command {
        "branch" => branch_invocation_is_read_only(command_args),
        "notes" => matches!(
            command_args.first().map(String::as_str),
            Some("show" | "list" | "get-ref")
        ),
        "remote" => remote_invocation_is_read_only(command_args),
        "stash" => stash_invocation_is_read_only(command_args),
        "tag" => tag_invocation_is_read_only(command_args),
        "worktree" => worktree_invocation_is_read_only(command_args),
        _ => false,
    }
}

/// Returns true when a Git command may mutate repository state and therefore
/// must be treated as an ordered trace2 root.
pub fn may_mutate_repo_state_command(command: &str) -> bool {
    command_policy::may_mutate_repo_state_command(command)
}

/// Returns true when a full Git invocation may mutate repository state and
/// therefore must be treated as an ordered trace2 root.
pub fn git_invocation_may_mutate_repo_state(command: &str, command_args: &[String]) -> bool {
    may_mutate_repo_state_command(command)
        && !is_definitely_read_only_git_invocation(command, command_args)
}

/// Returns true when a Git command must be ordered inside an existing repo
/// family. Commands like clone/init may mutate state, but they establish or
/// target a different repository context and must not be sequenced under the
/// launching repository family.
pub fn participates_in_family_sequencer_command(command: &str) -> bool {
    command_policy::participates_in_family_sequencer_command(command)
}

/// Returns true when a full Git invocation must be ordered inside an existing
/// repo family.
pub fn git_invocation_participates_in_family_sequencer(
    command: &str,
    command_args: &[String],
) -> bool {
    participates_in_family_sequencer_command(command)
        && git_invocation_may_mutate_repo_state(command, command_args)
}

fn branch_invocation_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }

    let mut idx = 0usize;
    let mut saw_list_mode = false;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            return false;
        }
        if branch_arg_is_mutating(arg) {
            return false;
        }
        if arg == "--list" || arg == "-l" {
            saw_list_mode = true;
            idx += 1;
            continue;
        }
        if branch_arg_takes_optional_query_value(arg) {
            idx += 1;
            if args
                .get(idx)
                .is_some_and(|next| !next.starts_with('-') && next != "--")
            {
                idx += 1;
            }
            continue;
        }
        if branch_arg_takes_required_query_value(arg) {
            idx += 1;
            if args
                .get(idx)
                .is_some_and(|next| !next.starts_with('-') && next != "--")
            {
                idx += 1;
                continue;
            }
            return false;
        }
        if branch_arg_is_read_only_flag(arg) || branch_arg_is_inline_query_option(arg) {
            idx += 1;
            continue;
        }
        if !arg.starts_with('-') && saw_list_mode {
            idx += 1;
            continue;
        }
        return false;
    }
    true
}

fn branch_arg_is_mutating(arg: &str) -> bool {
    matches!(
        arg,
        "-d" | "-D"
            | "--delete"
            | "-m"
            | "-M"
            | "--move"
            | "-c"
            | "-C"
            | "--copy"
            | "--set-upstream-to"
            | "--unset-upstream"
            | "--edit-description"
            | "--create-reflog"
    ) || arg.starts_with("--set-upstream-to=")
}

fn branch_arg_takes_optional_query_value(arg: &str) -> bool {
    matches!(
        arg,
        "--contains" | "--no-contains" | "--merged" | "--no-merged"
    )
}

fn branch_arg_takes_required_query_value(arg: &str) -> bool {
    matches!(arg, "--points-at" | "--sort" | "--format" | "--abbrev")
}

fn branch_arg_is_inline_query_option(arg: &str) -> bool {
    arg.starts_with("--points-at=")
        || arg.starts_with("--sort=")
        || arg.starts_with("--format=")
        || arg.starts_with("--color=")
        || arg.starts_with("--column=")
        || arg.starts_with("--abbrev=")
}

fn branch_arg_is_read_only_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-a" | "--all"
            | "-r"
            | "--remotes"
            | "-v"
            | "-vv"
            | "--verbose"
            | "-q"
            | "--quiet"
            | "--show-current"
            | "--color"
            | "--no-color"
            | "--column"
            | "--ignore-case"
            | "--no-column"
            | "--no-abbrev"
            | "--omit-empty"
    )
}

fn remote_invocation_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }
    let mut idx = 0usize;
    while args
        .get(idx)
        .is_some_and(|arg| matches!(arg.as_str(), "-v" | "--verbose"))
    {
        idx += 1;
    }
    matches!(
        args.get(idx).map(String::as_str),
        None | Some("show" | "get-url")
    )
}

fn stash_invocation_is_read_only(args: &[String]) -> bool {
    let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
    matches!(subcommand.map(String::as_str), Some("list" | "show"))
}

fn tag_invocation_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }

    let mut idx = 0usize;
    let mut query_mode = false;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            return false;
        }
        if tag_arg_is_mutating(arg) {
            return false;
        }
        if arg == "-l" || arg == "--list" || arg == "-v" || arg == "--verify" {
            query_mode = true;
            idx += 1;
            continue;
        }
        if tag_arg_takes_optional_query_value(arg) {
            query_mode = true;
            idx += 1;
            if args
                .get(idx)
                .is_some_and(|next| !next.starts_with('-') && next != "--")
            {
                idx += 1;
            }
            continue;
        }
        if tag_arg_takes_required_query_value(arg) {
            query_mode = true;
            idx += 1;
            if args
                .get(idx)
                .is_some_and(|next| !next.starts_with('-') && next != "--")
            {
                idx += 1;
                continue;
            }
            return false;
        }
        if tag_arg_is_inline_query_option(arg) || tag_arg_is_read_only_flag(arg) {
            query_mode = true;
            idx += 1;
            continue;
        }
        if !arg.starts_with('-') && query_mode {
            idx += 1;
            continue;
        }
        return false;
    }
    true
}

fn tag_arg_is_mutating(arg: &str) -> bool {
    matches!(
        arg,
        "-a" | "--annotate"
            | "-s"
            | "--sign"
            | "-u"
            | "--local-user"
            | "-m"
            | "--message"
            | "-F"
            | "--file"
            | "-d"
            | "--delete"
            | "-f"
            | "--force"
    ) || arg.starts_with("--local-user=")
        || arg.starts_with("--message=")
        || arg.starts_with("--file=")
}

fn tag_arg_takes_optional_query_value(arg: &str) -> bool {
    matches!(
        arg,
        "--contains" | "--no-contains" | "--merged" | "--no-merged"
    )
}

fn tag_arg_takes_required_query_value(arg: &str) -> bool {
    matches!(arg, "--points-at" | "--sort" | "--format")
}

fn tag_arg_is_inline_query_option(arg: &str) -> bool {
    arg.starts_with("--points-at=")
        || arg.starts_with("--sort=")
        || arg.starts_with("--format=")
        || arg.starts_with("--color=")
        || arg.starts_with("--column=")
}

fn tag_arg_is_read_only_flag(arg: &str) -> bool {
    matches!(
        arg,
        "-n" | "--color" | "--column" | "--ignore-case" | "--no-column" | "--no-color"
    ) || arg.starts_with("-n")
}

fn worktree_invocation_is_read_only(args: &[String]) -> bool {
    let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
    matches!(subcommand.map(String::as_str), Some("list"))
}

#[cfg(test)]
mod tests {
    use super::command_policy::{is_builtin_command, is_repo_admin_command, is_transport_command};
    use super::*;

    #[test]
    fn read_only_commands_detected() {
        assert!(is_definitely_read_only_command("check-ignore"));
        assert!(is_definitely_read_only_command("rev-parse"));
        assert!(is_definitely_read_only_command("status"));
        assert!(is_definitely_read_only_command("diff"));
        assert!(is_definitely_read_only_command("log"));
        assert!(is_definitely_read_only_command("cat-file"));
        assert!(is_definitely_read_only_command("ls-files"));
    }

    #[test]
    fn mutating_commands_not_read_only() {
        assert!(!is_definitely_read_only_command("commit"));
        assert!(!is_definitely_read_only_command("push"));
        assert!(!is_definitely_read_only_command("pull"));
        assert!(!is_definitely_read_only_command("rebase"));
        assert!(!is_definitely_read_only_command("merge"));
        assert!(!is_definitely_read_only_command("checkout"));
        assert!(!is_definitely_read_only_command("stash"));
        assert!(!is_definitely_read_only_command("reset"));
        assert!(!is_definitely_read_only_command("fetch"));
    }

    #[test]
    fn unknown_commands_not_read_only() {
        assert!(!is_definitely_read_only_command("my-custom-alias"));
        assert!(!is_definitely_read_only_command(""));
    }

    #[test]
    fn canonical_policy_preserves_cross_consumer_categories() {
        for command in ["clone", "fetch", "pull", "push", "remote", "ls-remote"] {
            assert!(
                is_transport_command(command),
                "{command} should be transport"
            );
        }
        for command in [
            "config",
            "credential",
            "gc",
            "maintenance",
            "fsck",
            "prune",
            "pack-refs",
            "reflog",
            "worktree",
        ] {
            assert!(
                is_repo_admin_command(command),
                "{command} should be repository-admin"
            );
        }

        assert!(is_builtin_command("commit"));
        assert!(!is_builtin_command("ls-remote"));
        assert!(!is_builtin_command("daemon_preconnect"));
    }

    #[test]
    fn stash_list_is_read_only_invocation() {
        assert!(is_definitely_read_only_git_invocation(
            "stash",
            &["list".to_string()]
        ));
    }

    #[test]
    fn stash_show_is_read_only_invocation() {
        assert!(is_definitely_read_only_git_invocation(
            "stash",
            &["show".to_string()]
        ));
    }

    #[test]
    fn stash_mutating_subcommands_are_not_read_only() {
        for subcommand in ["pop", "apply", "drop", "branch", "push", "save"] {
            assert!(!is_definitely_read_only_git_invocation(
                "stash",
                &[subcommand.to_string()]
            ));
        }
        // stash with no subcommand defaults to stash push (mutating)
        assert!(!is_definitely_read_only_git_invocation("stash", &[]));
    }

    #[test]
    fn worktree_list_is_read_only_invocation() {
        assert!(is_definitely_read_only_git_invocation(
            "worktree",
            &["list".to_string()]
        ));
    }

    #[test]
    fn worktree_mutating_subcommands_are_not_read_only() {
        for subcommand in ["add", "remove", "move", "lock", "unlock", "prune"] {
            assert!(!is_definitely_read_only_git_invocation(
                "worktree",
                &[subcommand.to_string()]
            ));
        }
        assert!(!is_definitely_read_only_git_invocation("worktree", &[]));
    }

    #[test]
    fn notes_read_only_subcommands_are_read_only_invocations() {
        assert!(is_definitely_read_only_git_invocation(
            "notes",
            &["show".to_string()]
        ));
        assert!(is_definitely_read_only_git_invocation(
            "notes",
            &["list".to_string()]
        ));
        assert!(is_definitely_read_only_git_invocation(
            "notes",
            &["get-ref".to_string()]
        ));
    }

    #[test]
    fn notes_mutating_subcommands_are_not_read_only_invocations() {
        for subcommand in &["add", "append", "copy", "edit", "merge", "prune", "remove"] {
            assert!(
                !is_definitely_read_only_git_invocation("notes", &[subcommand.to_string()]),
                "git notes {subcommand:?} should not be read-only"
            );
        }
        // Bare `git notes` (no subcommand) is not read-only either.
        assert!(!is_definitely_read_only_git_invocation("notes", &[]));
    }

    #[test]
    fn standard_read_only_commands_are_read_only_invocations_regardless_of_args() {
        for cmd in &[
            "status",
            "diff",
            "show",
            "log",
            "cat-file",
            "rev-parse",
            "for-each-ref",
            "blame",
            "grep",
            "ls-files",
            "ls-tree",
        ] {
            assert!(
                is_definitely_read_only_git_invocation(cmd, &[]),
                "{cmd} should be read-only with no args"
            );
            assert!(
                is_definitely_read_only_git_invocation(
                    cmd,
                    &["anything".to_string(), "--flag".to_string()]
                ),
                "{cmd} should be read-only regardless of args"
            );
        }
    }

    #[test]
    fn mutating_commands_are_not_read_only_invocations() {
        for cmd in &[
            "checkout",
            "cherry-pick",
            "clone",
            "commit",
            "fetch",
            "init",
            "merge",
            "pull",
            "push",
            "rebase",
            "reset",
            "revert",
            "switch",
            "update-ref",
        ] {
            assert!(
                !is_definitely_read_only_git_invocation(cmd, &[]),
                "{cmd} should not be read-only"
            );
        }

        for (cmd, args) in [
            ("branch", vec!["feature"]),
            ("remote", vec!["add", "origin", "/tmp/repo.git"]),
            ("stash", vec!["push"]),
            ("tag", vec!["v1"]),
            ("worktree", vec!["add", "../wt"]),
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                !is_definitely_read_only_git_invocation(cmd, &args),
                "git {cmd} {args:?} should not be read-only"
            );
        }
    }

    #[test]
    fn mutating_trace_commands_are_centrally_classified() {
        for cmd in &[
            "branch",
            "checkout",
            "cherry-pick",
            "clone",
            "commit",
            "fetch",
            "init",
            "merge",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "revert",
            "stash",
            "switch",
            "tag",
            "update-ref",
            "worktree",
        ] {
            assert!(
                may_mutate_repo_state_command(cmd),
                "{cmd} should be treated as a potentially mutating trace root"
            );
        }

        for cmd in &["status", "diff", "show", "log", "cat-file", "rev-parse"] {
            assert!(
                !may_mutate_repo_state_command(cmd),
                "{cmd} should not be treated as mutating"
            );
        }
    }

    #[test]
    fn clone_and_init_mutate_but_do_not_join_existing_family_sequencer() {
        for cmd in ["clone", "init"] {
            assert!(
                may_mutate_repo_state_command(cmd),
                "{cmd} should still be treated as mutating"
            );
            assert!(
                !participates_in_family_sequencer_command(cmd),
                "{cmd} should not be sequenced under an existing repo family"
            );
        }

        for cmd in ["commit", "rebase", "reset", "stash", "update-ref"] {
            assert!(
                participates_in_family_sequencer_command(cmd),
                "{cmd} should be sequenced inside its repo family"
            );
        }
    }

    #[test]
    fn branch_invocation_classification_distinguishes_read_only_queries_from_ref_updates() {
        for args in [
            vec![],
            vec!["--show-current"],
            vec!["--list"],
            vec!["-l"],
            vec!["--contains", "HEAD"],
            vec!["--merged", "main"],
            vec!["--points-at", "HEAD"],
            vec!["--format=%(refname:short)"],
            vec!["--color"],
            vec!["--column"],
            vec!["-vv"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                is_definitely_read_only_git_invocation("branch", &args),
                "git branch {args:?} should be classified as read-only"
            );
            assert!(
                !git_invocation_may_mutate_repo_state("branch", &args),
                "git branch {args:?} should not hold checkpoint sequencing"
            );
        }

        for args in [
            vec!["feature"],
            vec!["-d", "feature"],
            vec!["-D", "feature"],
            vec!["-m", "old", "new"],
            vec!["-M", "old", "new"],
            vec!["-c", "old", "new"],
            vec!["-C", "old", "new"],
            vec!["--set-upstream-to", "origin/main"],
            vec!["--unset-upstream"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                git_invocation_may_mutate_repo_state("branch", &args),
                "git branch {args:?} should be classified as mutating"
            );
            assert!(
                git_invocation_participates_in_family_sequencer("branch", &args),
                "git branch {args:?} should be ordered in the family sequencer"
            );
        }
    }

    #[test]
    fn remote_and_tag_invocation_classification_distinguishes_queries_from_mutations() {
        for (command, args) in [
            ("remote", vec![]),
            ("remote", vec!["-v"]),
            ("remote", vec!["show", "origin"]),
            ("remote", vec!["get-url", "origin"]),
            ("tag", vec![]),
            ("tag", vec!["--list"]),
            ("tag", vec!["-l", "v*"]),
            ("tag", vec!["--points-at", "HEAD"]),
            ("tag", vec!["--contains", "HEAD"]),
            ("tag", vec!["--color"]),
            ("tag", vec!["--column"]),
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                is_definitely_read_only_git_invocation(command, &args),
                "git {command} {args:?} should be classified as read-only"
            );
            assert!(
                !git_invocation_may_mutate_repo_state(command, &args),
                "git {command} {args:?} should not be classified as mutating"
            );
        }

        for (command, args) in [
            ("remote", vec!["add", "origin", "/tmp/repo.git"]),
            ("remote", vec!["remove", "origin"]),
            ("remote", vec!["rename", "origin", "upstream"]),
            ("remote", vec!["set-url", "origin", "/tmp/repo.git"]),
            ("remote", vec!["prune", "origin"]),
            ("tag", vec!["v1"]),
            ("tag", vec!["-a", "v1", "-m", "release"]),
            ("tag", vec!["-d", "v1"]),
            ("tag", vec!["--delete", "v1"]),
            ("tag", vec!["-f", "v1"]),
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(
                git_invocation_may_mutate_repo_state(command, &args),
                "git {command} {args:?} should be classified as mutating"
            );
            assert!(
                git_invocation_participates_in_family_sequencer(command, &args),
                "git {command} {args:?} should be ordered in the family sequencer"
            );
        }
    }
}
