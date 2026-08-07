//! Shared fixtures for the `ref_cursor` `tests_*` siblings: fake commit SHAs,
//! a minimal `FamilyState`, `NormalizedCommand` builders, and a reflog-file
//! writer. Each sibling test module glob-imports what it needs; unused
//! consts/fns don't warn because of the glob.
use super::*;
use crate::model::domain::{
    CommandScope, Confidence, FamilyKey, FamilyState, NormalizedCommand, WatermarkState,
};
use std::collections::HashMap;
use std::fs;

pub(super) const A: &str = "1111111111111111111111111111111111111111";
pub(super) const B: &str = "2222222222222222222222222222222222222222";
pub(super) const C: &str = "3333333333333333333333333333333333333333";
pub(super) const D: &str = "4444444444444444444444444444444444444444";
pub(super) const E: &str = "5555555555555555555555555555555555555555";
pub(super) const F: &str = "6666666666666666666666666666666666666666";
pub(super) const G: &str = "7777777777777777777777777777777777777777";

pub(super) fn family_state(family: &FamilyKey) -> FamilyState {
    FamilyState {
        family_key: family.clone(),
        refs: HashMap::new(),
        worktrees: HashMap::new(),
        last_error: None,
        applied_seq: 0,
        watermarks: WatermarkState::default(),
    }
}

pub(super) fn command(family: &FamilyKey, args: &[&str]) -> NormalizedCommand {
    command_with_worktree(family, None, args)
}

pub(super) fn command_with_worktree(
    family: &FamilyKey,
    worktree: Option<PathBuf>,
    args: &[&str],
) -> NormalizedCommand {
    NormalizedCommand {
        scope: CommandScope::Family(family.clone()),
        family_key: Some(family.clone()),
        worktree,
        root_sid: "sid".to_string(),
        trace_derived: false,
        raw_argv: std::iter::once("git".to_string())
            .chain(args.iter().map(|arg| arg.to_string()))
            .collect(),
        primary_command: args.first().map(|arg| arg.to_string()),
        invoked_command: args.first().map(|arg| arg.to_string()),
        invoked_args: args.iter().map(|arg| arg.to_string()).collect(),
        observed_child_commands: Vec::new(),
        exit_code: 0,
        started_at_ns: 1,
        finished_at_ns: 2,
        reflog_start_offsets: HashMap::new(),
        stash_target_oid: None,
        cherry_pick_source_oids: Vec::new(),
        revert_source_oids: Vec::new(),
        ref_changes: Vec::new(),
        confidence: Confidence::Low,
    }
}

pub(super) fn ref_change(reference: &str, old: &str, new: &str) -> RefChange {
    RefChange {
        reference: reference.to_string(),
        old: old.to_string(),
        new: new.to_string(),
    }
}

pub(super) fn append_reflog(common_dir: &Path, reference: &str, entries: &[(&str, &str, &str)]) {
    // Discovery only honors directory-form git dirs that carry a HEAD file,
    // so make the synthetic git dir valid before writing its reflog.
    crate::operations::git::test_utils::seed_valid_git_dir(common_dir);
    let path = common_dir.join("logs").join(reference);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = String::new();
    for (old, new, message) in entries {
        text.push_str(&format!(
            "{old} {new} Test User <test@example.com> 0 +0000\t{message}\n"
        ));
    }
    fs::write(path, text).unwrap();
}

#[test]
fn ref_change_fixture_builds_expected_values() {
    let change = ref_change("refs/heads/main", A, B);

    assert_eq!(change.reference, "refs/heads/main");
    assert_eq!(change.old, A);
    assert_eq!(change.new, B);
}
