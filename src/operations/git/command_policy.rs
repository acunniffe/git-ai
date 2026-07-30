const POLICY_READ_ONLY: u8 = 1 << 0;
const POLICY_MAY_MUTATE: u8 = 1 << 1;
const POLICY_FAMILY_SEQUENCER: u8 = 1 << 2;
const POLICY_TRANSPORT: u8 = 1 << 3;
const POLICY_REPO_ADMIN: u8 = 1 << 4;
const POLICY_BUILTIN: u8 = 1 << 5;

/// The single source of truth for command-name policy metadata.
///
/// The macro expands to one string match, so every predicate below remains a
/// constant-time lookup with no allocation or runtime table walk. Invocation
/// rules that depend on argv stay in `command_classification`.
macro_rules! define_command_policy {
    ($( $command:literal => $flags:expr, )* $(,)?) => {
        fn command_policy_flags(command: &str) -> u8 {
            match command {
                $( $command => $flags, )*
                _ => 0,
            }
        }
    };
}

define_command_policy! {
    "add" => POLICY_BUILTIN,
    "blame" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "branch" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "cat-file" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "check-attr" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "check-ignore" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "check-mailmap" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "checkout" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "cherry-pick" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "clean" => POLICY_BUILTIN,
    "clone" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_TRANSPORT,
    "commit" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "config" => POLICY_BUILTIN | POLICY_REPO_ADMIN,
    "count-objects" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "credential" => POLICY_REPO_ADMIN,
    "describe" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "diff" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "diff-files" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "diff-index" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "diff-tree" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "fetch" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER | POLICY_TRANSPORT,
    "for-each-ref" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "fsck" => POLICY_REPO_ADMIN,
    "gc" => POLICY_REPO_ADMIN,
    "grep" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "hash-object" => POLICY_BUILTIN,
    "help" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "init" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_REPO_ADMIN,
    "log" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "ls-files" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "ls-remote" => POLICY_TRANSPORT,
    "ls-tree" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "maintenance" => POLICY_REPO_ADMIN,
    "merge" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "merge-base" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "mktree" => POLICY_BUILTIN,
    "mv" => POLICY_BUILTIN,
    "name-rev" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "notes" => POLICY_BUILTIN,
    "pack-refs" => POLICY_REPO_ADMIN,
    "prune" => POLICY_REPO_ADMIN,
    "pull" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER | POLICY_TRANSPORT,
    "push" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER | POLICY_TRANSPORT,
    "rebase" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "reflog" => POLICY_REPO_ADMIN,
    "remote" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER | POLICY_TRANSPORT,
    "reset" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "restore" => POLICY_BUILTIN,
    "rev-list" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "rev-parse" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "revert" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "rm" => POLICY_BUILTIN,
    "shortlog" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "show" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "stash" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "status" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "switch" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "symbolic-ref" => POLICY_BUILTIN,
    "tag" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "update-ref" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER,
    "var" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "verify-commit" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "verify-tag" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "version" => POLICY_BUILTIN | POLICY_READ_ONLY,
    "worktree" => POLICY_BUILTIN | POLICY_MAY_MUTATE | POLICY_FAMILY_SEQUENCER | POLICY_REPO_ADMIN,
}

fn command_has_policy(command: &str, policy: u8) -> bool {
    command_policy_flags(command) & policy != 0
}

pub(crate) fn is_builtin_command(command: &str) -> bool {
    command_has_policy(command, POLICY_BUILTIN)
}

pub(crate) fn is_transport_command(command: &str) -> bool {
    command_has_policy(command, POLICY_TRANSPORT)
}

pub(crate) fn is_repo_admin_command(command: &str) -> bool {
    command_has_policy(command, POLICY_REPO_ADMIN)
}

pub(crate) fn is_definitely_read_only_command(command: &str) -> bool {
    command_has_policy(command, POLICY_READ_ONLY)
}

pub(crate) fn may_mutate_repo_state_command(command: &str) -> bool {
    command_has_policy(command, POLICY_MAY_MUTATE)
}

pub(crate) fn participates_in_family_sequencer_command(command: &str) -> bool {
    command_has_policy(command, POLICY_FAMILY_SEQUENCER)
}
