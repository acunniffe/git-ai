use crate::daemon::analyzers::{command_args, normalized_args};
use crate::daemon::domain::{Confidence, FamilyKey, FamilyState, NormalizedCommand, RefChange};
use crate::error::GitAiError;
use crate::git::cli_parser::{
    explicit_rebase_branch_arg, parse_git_cli_args, summarize_rebase_args,
};
use crate::git::find_repository_in_path;
use crate::git::repo_state::{common_dir_for_worktree, git_dir_for_worktree, is_valid_git_oid};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct RefCursor {
    family: FamilyKey,
    offsets: HashMap<String, u64>,
    anchors: HashMap<String, ReflogAnchor>,
    consumed_offsets: HashMap<String, HashSet<u64>>,
    consumed_anchors: HashMap<String, HashMap<u64, ReflogAnchor>>,
    /// Per-command soft hints from the daemon-ingress reflog offset capture.
    /// Populated at the start of each command's enrichment and cleared when the
    /// next command's enrichment begins. Unlike `offsets` (the authoritative
    /// in-order cursor), these are captured asynchronously and may race ahead of
    /// or behind the command's own reflog entry, so they only *bias* entry
    /// selection — they never move the cursor. See `select_entry_for_hint`.
    command_start_hints: HashMap<String, u64>,
    stash_stack: Vec<String>,
    pending_cherry_pick_source_oids: Vec<String>,
    pending_revert_source_oids: Vec<String>,
}

#[derive(Debug, Clone)]
struct CursorEntry {
    key: String,
    path: PathBuf,
    reference: String,
    old: String,
    new: String,
    message: String,
    timestamp_secs: Option<i64>,
    start_offset: u64,
    end_offset: u64,
}

#[derive(Debug, Clone)]
struct UpdateRefSpec {
    reference: String,
    new_oid: String,
    old_oid: Option<String>,
}

#[derive(Debug, Clone)]
enum BranchCommandSpec {
    CreateOrReset {
        reference: String,
    },
    Delete {
        references: Vec<String>,
    },
    Rename {
        old_reference: Option<String>,
        new_reference: String,
    },
    Copy {
        old_reference: Option<String>,
        new_reference: String,
    },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchLifecycleKind {
    Rename,
    Copy,
}

#[derive(Debug, Clone)]
struct BranchLifecycleRecord {
    old_reference: String,
    oid: String,
}

#[derive(Debug, Clone)]
struct ReflogRecord {
    old: String,
    new: String,
    message: String,
    timestamp_secs: Option<i64>,
    start_offset: u64,
    end_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReflogAnchor {
    old: String,
    new: String,
    message: String,
    end_offset: u64,
}

const CHERRY_PICK_REFLOG_PREFIXES: &[&str] = &["cherry-pick:", "commit (cherry-pick):"];
const AM_REFLOG_PREFIXES: &[&str] = &["am:"];

enum ColdSeedMatchSpec {
    SingleEntry {
        expected: ExpectedTransition,
        prefixes: Vec<String>,
    },
    HeadSpan {
        expected: ExpectedTransition,
        prefixes: Vec<String>,
        limit: usize,
    },
    PullSpan {
        action: String,
        expected: ExpectedTransition,
    },
    RebaseSpan {
        expected: ExpectedTransition,
    },
}

impl RefCursor {
    pub fn new(family: FamilyKey) -> Self {
        Self {
            family,
            offsets: HashMap::new(),
            anchors: HashMap::new(),
            consumed_offsets: HashMap::new(),
            consumed_anchors: HashMap::new(),
            command_start_hints: HashMap::new(),
            stash_stack: Vec::new(),
            pending_cherry_pick_source_oids: Vec::new(),
            pending_revert_source_oids: Vec::new(),
        }
    }

    pub fn enrich_command(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<HashMap<String, String>, GitAiError> {
        cmd.ref_changes.clear();
        self.initialize_from_command_reflog_start_offsets(cmd)?;
        let command_start_refs =
            refs_at_reflog_start_offsets(&self.family, &cmd.reflog_start_offsets)?;

        if cmd.exit_code != 0 && !command_can_move_refs_on_nonzero(cmd.primary_command.as_deref()) {
            return Ok(command_start_refs);
        }

        let Some(primary) = cmd.primary_command.as_deref() else {
            return Ok(command_start_refs);
        };
        if !command_uses_ref_cursor(primary)
            || crate::git::command_classification::is_definitely_read_only_git_invocation(
                primary,
                &command_args(cmd),
            )
        {
            return Ok(command_start_refs);
        }

        match primary {
            "am" => self.enrich_am(cmd, state),
            "commit" => self.enrich_commit(cmd, state),
            "revert" => self.enrich_revert(cmd, state),
            "reset" => {
                let args = command_args(cmd);
                let mut expected = self.head_expected_transition(cmd, state);
                let reset_messages = reset_reflog_messages(&args);
                if !reset_messages.is_empty() {
                    expected = expected
                        .without_old_oid_constraint()
                        .with_reflog_messages(reset_messages);
                }
                self.consume_head_transition_for_command(cmd, state, &["reset:"], expected)
            }
            "checkout" => {
                if checkout_is_path_checkout(cmd) {
                    Ok(())
                } else if checkout_or_switch_is_orphan(cmd) {
                    if cmd.exit_code == 0 {
                        enrich_orphan_head_transition(cmd, state, &command_start_refs);
                    }
                    Ok(())
                } else {
                    self.consume_head_transition_for_command(
                        cmd,
                        state,
                        &["checkout:"],
                        self.head_expected_transition(cmd, state),
                    )
                }
            }
            "switch" => {
                if checkout_or_switch_is_orphan(cmd) {
                    if cmd.exit_code == 0 {
                        enrich_orphan_head_transition(cmd, state, &command_start_refs);
                    }
                    Ok(())
                } else {
                    self.consume_head_transition_for_command(
                        cmd,
                        state,
                        &["checkout:", "switch:"],
                        self.head_expected_transition(cmd, state),
                    )
                }
            }
            "merge" => {
                let args = command_args(cmd);
                let prefixes = merge_reflog_prefixes(&args);
                self.consume_head_transition_for_command(
                    cmd,
                    state,
                    prefixes,
                    self.head_expected_transition(cmd, state),
                )
            }
            "cherry-pick" => self.enrich_cherry_pick(cmd, state),
            "rebase" => self.consume_rebase_transition(cmd, state),
            "pull" => self.consume_pull_transition(cmd, state),
            "branch" => self.enrich_branch(cmd, state),
            "stash" => self.enrich_stash(cmd, state),
            "update-ref" => self.enrich_update_ref(cmd, state),
            "fast-import" | "filter-branch" | "filter-repo" | "symbolic-ref" => {
                self.enrich_unstructured_ref_mutation(cmd, state)
            }
            _ => Ok(()),
        }?;

        if !cmd.ref_changes.is_empty() {
            cmd.confidence = Confidence::High;
        }
        Ok(command_start_refs)
    }

    fn initialize_from_command_reflog_start_offsets(
        &mut self,
        cmd: &NormalizedCommand,
    ) -> Result<(), GitAiError> {
        // Each command's enrichment starts fresh: a prior command's ingress hint
        // must never bias this command's entry selection.
        self.command_start_hints.clear();

        if cmd.reflog_start_offsets.is_empty() {
            return Ok(());
        }

        let offsets = cmd
            .reflog_start_offsets
            .iter()
            .map(|(key, offset)| (key.clone(), *offset))
            .collect::<Vec<_>>();
        for (key, offset) in offsets {
            if self.offsets.contains_key(&key) {
                // An authoritative in-order cursor already exists (established by
                // prior command processing or a checkpoint boundary). The ingress
                // offset is captured asynchronously and can race ahead of the
                // command's own reflog entry; letting it advance the cursor would
                // skip that entry and lose attribution (the graphite/gt-create
                // flake). Keep the in-order cursor as the floor and remember the
                // ingress offset only as a soft selection hint for disambiguating
                // colliding entries (e.g. an untraced commit sharing a message).
                self.command_start_hints.insert(key, offset);
            } else if self.command_start_offset_is_authoritative(&key, offset)? {
                // No cursor yet (cold start / first traced command). The ingress
                // offset is the command-start boundary used to skip genuinely
                // prior untraced history, so it seeds the fresh cursor.
                //
                // But the capture is asynchronous and can race AHEAD of git,
                // landing past the command's own entry (concurrent-worktree-burst
                // / rebase-patch-stack flake). Seeding at such a late offset makes
                // it a hard floor and silently drops the command's own entry. To
                // stay safe we clamp the seed back to the START of the command's
                // own matching entry when that entry lies before the offset, and
                // also keep the offset as a soft selection hint so genuinely prior
                // untraced history is still skipped.
                let seed = self.clamp_seed_to_own_entry(&key, offset, cmd)?;
                if seed != offset {
                    self.command_start_hints.insert(key.clone(), offset);
                }
                self.initialize_reflog_cursor(&key, seed)?;
            } else if command_can_clamp_non_authoritative_cold_seed(cmd) {
                let seed = self.clamp_seed_to_own_entry(&key, offset, cmd)?;
                if seed != offset {
                    self.command_start_hints.insert(key.clone(), offset);
                    self.initialize_reflog_cursor(&key, seed)?;
                }
            }
        }

        Ok(())
    }

    /// Clamp a cold-start seed offset so it never lands past the command's own
    /// matching reflog entry. Returns the start offset of the earliest entry
    /// at/before `offset` that matches this command (by expected transition and
    /// message prefixes); if no such entry precedes the offset, returns `offset`
    /// unchanged (the offset legitimately skips only prior, non-matching
    /// history).
    fn clamp_seed_to_own_entry(
        &self,
        key: &str,
        offset: u64,
        cmd: &NormalizedCommand,
    ) -> Result<u64, GitAiError> {
        if offset == 0 {
            return Ok(0);
        }
        let Some(path) = self.reflog_path_for_key(key) else {
            return Ok(offset);
        };
        let Some(spec) = self.cold_seed_match_spec(cmd) else {
            // No command-specific matcher (e.g. update-ref --stdin / stash that
            // match by transition only): keep the offset as the boundary to
            // preserve existing first-observed-boundary semantics.
            return Ok(offset);
        };
        match spec {
            ColdSeedMatchSpec::SingleEntry { expected, prefixes } => {
                let prefix_refs = prefixes.iter().map(String::as_str).collect::<Vec<_>>();
                let reference = if let Some(reference) = key.strip_prefix("common:") {
                    reference.to_string()
                } else {
                    "HEAD".to_string()
                };
                let entries = read_reflog_entries(key.to_string(), &path, &reference, None)?;
                let earliest_own = entries
                    .into_iter()
                    .filter(|entry| {
                        entry.start_offset < offset
                            && expected.matches(entry)
                            && message_matches(&entry.message, &prefix_refs)
                    })
                    .map(|entry| entry.start_offset)
                    .min();
                Ok(earliest_own.unwrap_or(offset))
            }
            ColdSeedMatchSpec::HeadSpan {
                expected,
                prefixes,
                limit,
            } => {
                if limit == 0 {
                    return Ok(offset);
                }
                let prefix_refs = prefixes.iter().map(String::as_str).collect::<Vec<_>>();
                let reference = if let Some(reference) = key.strip_prefix("common:") {
                    reference.to_string()
                } else {
                    "HEAD".to_string()
                };
                let entries = read_reflog_entries(key.to_string(), &path, &reference, None)?;
                Ok(
                    head_span_start_near_offset(&entries, offset, &prefix_refs, expected, limit)
                        .unwrap_or(offset),
                )
            }
            ColdSeedMatchSpec::PullSpan { action, expected } => {
                self.clamp_seed_to_pull_span_entry(key, &path, offset, &action, expected)
            }
            ColdSeedMatchSpec::RebaseSpan { expected } => {
                self.clamp_seed_to_rebase_span_entry(key, &path, offset, expected)
            }
        }
    }

    fn clamp_seed_to_pull_span_entry(
        &self,
        key: &str,
        path: &Path,
        offset: u64,
        action: &str,
        expected: ExpectedTransition,
    ) -> Result<u64, GitAiError> {
        let reference = if let Some(reference) = key.strip_prefix("common:") {
            reference.to_string()
        } else {
            "HEAD".to_string()
        };
        let entries = read_reflog_entries_including_noops(key.to_string(), path, &reference, None)?;
        let prefixes = pull_reflog_message_prefixes(action);
        let prefix_refs = prefixes.iter().map(String::as_str).collect::<Vec<_>>();

        if key.starts_with("common:") {
            return Ok(
                clamp_seed_to_entry_containing_offset(&entries, offset, &prefix_refs)
                    .unwrap_or(offset),
            );
        }

        Ok(pull_span_start_containing_offset(&entries, offset, action, expected).unwrap_or(offset))
    }

    fn clamp_seed_to_rebase_span_entry(
        &self,
        key: &str,
        path: &Path,
        offset: u64,
        expected: ExpectedTransition,
    ) -> Result<u64, GitAiError> {
        let reference = if let Some(reference) = key.strip_prefix("common:") {
            reference.to_string()
        } else {
            "HEAD".to_string()
        };
        let entries = read_reflog_entries_including_noops(key.to_string(), path, &reference, None)?;
        if key.starts_with("common:") {
            return Ok(
                clamp_seed_to_entry_containing_offset(&entries, offset, &["rebase"])
                    .unwrap_or(offset),
            );
        }

        Ok(rebase_span_start_containing_offset(&entries, offset, expected).unwrap_or(offset))
    }

    /// The expected transition + reflog message prefixes used to recognize a
    /// command's OWN entry during cold-start seed clamping. Returns None for
    /// commands matched by transition alone (no message discriminator), where
    /// clamping must not change the seed (update-ref --stdin, stash, etc.).
    fn cold_seed_match_spec(&self, cmd: &NormalizedCommand) -> Option<ColdSeedMatchSpec> {
        // Only commands with enough reflog structure to distinguish their own
        // rows should clamp a cold asynchronous seed backward. Single-entry
        // commits use their subject-specific reflog message; rebase/pull use
        // their start/pick/finish span shape.
        let args = command_args(cmd);
        match cmd.primary_command.as_deref()? {
            "am" => {
                if args.iter().any(|arg| {
                    matches!(arg.as_str(), "--quit" | "--show-current-patch")
                        || arg.starts_with("--show-current-patch=")
                }) {
                    return None;
                }
                if args.iter().any(|arg| arg == "--abort") {
                    return Some(ColdSeedMatchSpec::SingleEntry {
                        expected: ExpectedTransition::default(),
                        prefixes: vec!["am".to_string(), "reset:".to_string()],
                    });
                }
                Some(ColdSeedMatchSpec::HeadSpan {
                    expected: ExpectedTransition::default(),
                    prefixes: AM_REFLOG_PREFIXES
                        .iter()
                        .map(|prefix| (*prefix).to_string())
                        .collect(),
                    limit: usize::MAX,
                })
            }
            "commit" => {
                let amend = args.iter().any(|arg| arg == "--amend");
                let prefixes: Vec<String> = if amend {
                    vec!["commit (amend):".to_string()]
                } else {
                    vec!["commit".to_string(), "commit (initial):".to_string()]
                };
                let expected = ExpectedTransition::default()
                    .with_reflog_messages(commit_reflog_messages(&args, amend));
                Some(ColdSeedMatchSpec::SingleEntry { expected, prefixes })
            }
            "pull" => Some(ColdSeedMatchSpec::PullSpan {
                action: pull_reflog_action(cmd),
                expected: ExpectedTransition::default(),
            }),
            "rebase" => Some(ColdSeedMatchSpec::RebaseSpan {
                expected: ExpectedTransition::default(),
            }),
            "cherry-pick" => {
                if args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"))
                    || args.iter().any(|arg| arg == "--no-commit" || arg == "-n")
                {
                    return None;
                }
                let source_args = cherry_pick_source_args(&args);
                let limit = if args.iter().any(|arg| arg == "--stdin")
                    || source_args
                        .iter()
                        .any(|source| cherry_pick_source_is_range(source))
                {
                    usize::MAX
                } else if source_args.is_empty() {
                    self.pending_cherry_pick_source_oids.len().max(1)
                } else {
                    source_args.len().max(1)
                };
                Some(ColdSeedMatchSpec::HeadSpan {
                    expected: ExpectedTransition::default(),
                    prefixes: CHERRY_PICK_REFLOG_PREFIXES
                        .iter()
                        .map(|prefix| (*prefix).to_string())
                        .collect(),
                    limit,
                })
            }
            "revert" => {
                if args.iter().any(|arg| {
                    matches!(arg.as_str(), "--abort" | "--quit" | "--skip")
                        || arg == "--no-commit"
                        || arg == "-n"
                }) {
                    return None;
                }
                let limit = if args.iter().any(|arg| arg == "--continue") {
                    self.pending_revert_source_oids.len().max(1)
                } else {
                    revert_source_args(&args).len().max(1)
                };
                Some(ColdSeedMatchSpec::HeadSpan {
                    expected: ExpectedTransition::default(),
                    prefixes: revert_reflog_prefixes(&args)
                        .iter()
                        .map(|prefix| (*prefix).to_string())
                        .collect(),
                    limit,
                })
            }
            _ => None,
        }
    }

    fn enrich_commit(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        let amend = args.iter().any(|arg| arg == "--amend");
        let prefixes = if amend {
            &["commit (amend):"] as &[&str]
        } else {
            &["commit", "commit (initial):"]
        };
        let expected = self
            .head_expected_transition(cmd, state)
            .without_old_oid_constraint()
            .with_reflog_messages(commit_reflog_messages(&args, amend));
        let entry = self.find_commit_head_entry(cmd, prefixes, expected.clone())?;
        crate::wltrace::wltrace(
            "ref_cursor.commit_entry",
            cmd.worktree.as_deref().unwrap_or(Path::new("")),
            || {
                let cursor = cmd
                    .worktree
                    .as_deref()
                    .and_then(git_dir_for_worktree)
                    .map(|git_dir| head_key(&git_dir))
                    .map(|key| {
                        format!(
                            "offset={:?} hint={:?} consumed={:?}",
                            self.offsets.get(&key),
                            self.command_start_hints.get(&key),
                            self.consumed_offsets.get(&key),
                        )
                    })
                    .unwrap_or_else(|| "no-key".to_string());
                format!(
                    "sid={} exit={} entry={} expected_new={:?} messages={:?} {cursor}",
                    cmd.root_sid,
                    cmd.exit_code,
                    entry
                        .as_ref()
                        .map(|entry| format!(
                            "{}->{}@{} msg={}",
                            &entry.old[..8.min(entry.old.len())],
                            &entry.new[..8.min(entry.new.len())],
                            entry.start_offset,
                            entry.message.chars().take(40).collect::<String>()
                        ))
                        .unwrap_or_else(|| "NONE".to_string()),
                    expected
                        .new_oid
                        .as_ref()
                        .map(|oid| &oid[..8.min(oid.len())]),
                    expected.messages,
                )
            },
        );
        let Some(entry) = entry else {
            return Ok(());
        };

        self.consume_head_entry_for_command(cmd, entry)
    }

    fn find_commit_head_entry(
        &mut self,
        cmd: &NormalizedCommand,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        // Prefer the exact argv-derived reflog message when possible: it is the
        // strongest discriminator for untraced duplicate commit entries. A
        // commit-msg hook can rewrite the subject after argv is fixed, though, and
        // git records that final subject in the reflog. In that case fall back to a
        // command-window match below instead of dropping the HEAD transition.
        if let Some(entry) =
            self.find_head_entry(cmd.worktree.as_deref(), message_prefixes, expected.clone())?
        {
            return Ok(Some(entry));
        }

        if expected.messages.is_empty() {
            return Ok(None);
        }

        self.find_head_entry_in_command_window(
            cmd,
            message_prefixes,
            expected.without_reflog_message_constraint(),
        )
    }

    fn enrich_am(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        if args.iter().any(|arg| {
            matches!(arg.as_str(), "--quit" | "--show-current-patch")
                || arg.starts_with("--show-current-patch=")
        }) {
            return Ok(());
        }
        if args.iter().any(|arg| arg == "--abort") {
            return self.consume_head_transition_for_command(
                cmd,
                state,
                &["am", "reset:"],
                self.head_expected_transition(cmd, state),
            );
        }

        self.consume_head_span_for_command_limited(
            cmd,
            state,
            AM_REFLOG_PREFIXES,
            self.head_expected_transition(cmd, state),
            usize::MAX,
        )
    }

    fn enrich_cherry_pick(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"))
        {
            self.pending_cherry_pick_source_oids.clear();
            return Ok(());
        }

        let is_no_commit = args.iter().any(|arg| arg == "--no-commit" || arg == "-n");
        let is_continue = args.iter().any(|arg| arg == "--continue");
        let is_skip = args.iter().any(|arg| arg == "--skip");

        if is_skip && !self.pending_cherry_pick_source_oids.is_empty() {
            self.pending_cherry_pick_source_oids.remove(0);
        }

        let source_args = if is_continue || is_skip {
            Vec::new()
        } else {
            cherry_pick_source_args(&args)
        };
        let explicit_sources = if is_continue || is_skip {
            Some(Vec::new())
        } else {
            resolve_cherry_pick_source_oids_from_sources(cmd, state, &source_args)?
        };
        let unresolved_explicit_sources = !source_args.is_empty() && explicit_sources.is_none();
        let explicit_sources = explicit_sources.unwrap_or_default();
        cmd.cherry_pick_source_oids = if explicit_sources.is_empty() && !unresolved_explicit_sources
        {
            self.pending_cherry_pick_source_oids.clone()
        } else {
            explicit_sources
        };

        if cmd.exit_code != 0 && unresolved_explicit_sources {
            return Ok(());
        }

        if is_no_commit {
            return Ok(());
        }

        let source_limit = if unresolved_explicit_sources || args.iter().any(|arg| arg == "--stdin")
        {
            usize::MAX
        } else {
            cmd.cherry_pick_source_oids.len().max(1)
        };
        self.consume_head_span_for_command_limited(
            cmd,
            state,
            CHERRY_PICK_REFLOG_PREFIXES,
            self.head_expected_transition(cmd, state),
            source_limit,
        )?;

        let applied_count = cmd
            .ref_changes
            .iter()
            .filter(|change| change.reference == "HEAD")
            .count();
        if cmd.exit_code != 0 {
            self.pending_cherry_pick_source_oids = cmd
                .cherry_pick_source_oids
                .iter()
                .skip(applied_count.min(cmd.cherry_pick_source_oids.len()))
                .cloned()
                .collect();
        } else if is_continue
            || is_skip
            || !cmd.cherry_pick_source_oids.is_empty()
            || applied_count > 0
        {
            self.pending_cherry_pick_source_oids.clear();
        }

        Ok(())
    }

    fn enrich_revert(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"))
        {
            self.pending_revert_source_oids.clear();
            return Ok(());
        }

        let is_no_commit = args.iter().any(|arg| arg == "--no-commit" || arg == "-n");
        let is_continue = args.iter().any(|arg| arg == "--continue");
        let is_skip = args.iter().any(|arg| arg == "--skip");
        if is_skip && !self.pending_revert_source_oids.is_empty() {
            self.pending_revert_source_oids.remove(0);
        }
        // Continue/skip do not repeat source OIDs on argv. The daemon persists
        // the resolved list with the attempt snapshot and reuses it when the
        // sequencer later creates destination commits.
        let source_args = if is_continue || is_skip {
            Vec::new()
        } else {
            revert_source_args(&args)
        };
        let explicit_sources = if source_args.is_empty() {
            Some(Vec::new())
        } else {
            resolve_cherry_pick_source_oids_from_sources(cmd, state, &source_args)?
        };
        let unresolved_explicit_sources = !source_args.is_empty() && explicit_sources.is_none();
        let explicit_sources = explicit_sources.unwrap_or_default();
        cmd.revert_source_oids = if explicit_sources.is_empty() && !unresolved_explicit_sources {
            self.pending_revert_source_oids.clone()
        } else {
            explicit_sources
        };

        if is_no_commit {
            return Ok(());
        }

        let source_limit = if unresolved_explicit_sources {
            usize::MAX
        } else {
            cmd.revert_source_oids.len().max(1)
        };
        self.consume_head_span_for_command_limited(
            cmd,
            state,
            revert_reflog_prefixes(&args),
            self.head_expected_transition(cmd, state),
            source_limit,
        )?;

        let applied_count = cmd
            .ref_changes
            .iter()
            .filter(|change| change.reference == "HEAD")
            .count();
        if cmd.exit_code != 0 {
            self.pending_revert_source_oids = cmd
                .revert_source_oids
                .iter()
                .skip(applied_count.min(cmd.revert_source_oids.len()))
                .cloned()
                .collect();
        } else if is_continue || is_skip || !cmd.revert_source_oids.is_empty() || applied_count > 0
        {
            self.pending_revert_source_oids.clear();
        }

        Ok(())
    }

    fn enrich_branch(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        let spec = parse_branch_command_spec(&args);
        let mut changes = Vec::new();

        match spec {
            BranchCommandSpec::CreateOrReset { reference } => {
                if let Some(entry) = self.find_common_ref_entry(
                    &reference,
                    ExpectedTransition::default(),
                    &["branch:"],
                )? {
                    self.consume_entry(&entry)?;
                    changes.push(entry_to_ref_change(&entry));
                }
            }
            BranchCommandSpec::Delete { references } => {
                let zero = zero_oid();
                for reference in references {
                    self.clear_ref_cursor(&common_key(&reference));
                    if let Some(old) = state
                        .refs
                        .get(&reference)
                        .filter(|oid| valid_non_zero_oid(oid))
                    {
                        changes.push(RefChange {
                            reference,
                            old: old.clone(),
                            new: zero.clone(),
                        });
                    }
                }
            }
            BranchCommandSpec::Rename {
                old_reference,
                new_reference,
            } => {
                self.enrich_branch_relocation(
                    state,
                    BranchLifecycleKind::Rename,
                    old_reference,
                    new_reference,
                    &mut changes,
                )?;
            }
            BranchCommandSpec::Copy {
                old_reference,
                new_reference,
            } => {
                self.enrich_branch_relocation(
                    state,
                    BranchLifecycleKind::Copy,
                    old_reference,
                    new_reference,
                    &mut changes,
                )?;
            }
            BranchCommandSpec::None => {}
        }

        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    fn enrich_branch_relocation(
        &mut self,
        state: &FamilyState,
        kind: BranchLifecycleKind,
        old_reference: Option<String>,
        new_reference: String,
        changes: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        let lifecycle = self.consume_branch_lifecycle_record(&new_reference, kind)?;
        let source_reference = old_reference.or_else(|| {
            lifecycle
                .as_ref()
                .map(|record| record.old_reference.clone())
        });
        let source_oid = source_reference
            .as_ref()
            .and_then(|reference| state.refs.get(reference).cloned())
            .or_else(|| lifecycle.as_ref().map(|record| record.oid.clone()));
        let Some(source_oid) = source_oid.filter(|oid| valid_non_zero_oid(oid)) else {
            return Ok(());
        };

        if kind == BranchLifecycleKind::Rename
            && let Some(source_reference) = source_reference.as_ref()
            && source_reference != &new_reference
        {
            self.clear_ref_cursor(&common_key(source_reference));
            changes.push(RefChange {
                reference: source_reference.clone(),
                old: source_oid.clone(),
                new: zero_oid(),
            });
        }

        let new_old = state
            .refs
            .get(&new_reference)
            .filter(|oid| valid_non_zero_oid(oid))
            .cloned()
            .unwrap_or_else(zero_oid);
        if new_old != source_oid {
            changes.push(RefChange {
                reference: new_reference.clone(),
                old: new_old,
                new: source_oid,
            });
        }
        Ok(())
    }

    fn enrich_update_ref(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        let spec = parse_update_ref_spec(&args)?;
        let Some(spec) = spec else {
            let mut changes = Vec::new();
            if let Some(worktree) = cmd.worktree.as_deref() {
                while let Some(entry) = self.find_head_entry_without_hint(
                    Some(worktree),
                    &[],
                    ExpectedTransition::default(),
                )? {
                    self.consume_entry(&entry)?;
                    changes.push(entry_to_ref_change(&entry));
                }
            }
            self.drain_unstructured_common_ref_entries(&mut changes)?;
            dedup_ref_changes(&mut changes);
            cmd.ref_changes = changes;
            return Ok(());
        };

        let mut changes = Vec::new();
        if spec.reference == "HEAD" {
            if let Some(change) = direct_update_ref_change_from_argv(&spec) {
                changes.push(change.clone());
                if let Some(head) =
                    self.find_direct_ref_transition_entry(cmd, "HEAD", &change.old, &change.new)?
                {
                    let head_timestamp = head.timestamp_secs;
                    self.consume_entry(&head)?;
                    if let Some(timestamp) = head_timestamp {
                        if let Some(branch) = current_worktree_branch_ref(cmd, state) {
                            if let Some(entry) = self
                                .direct_ref_transition_entries(
                                    cmd,
                                    branch,
                                    &change.old,
                                    &change.new,
                                )?
                                .into_iter()
                                .find(|entry| entry.timestamp_secs == Some(timestamp))
                            {
                                self.consume_entry(&entry)?;
                                changes.push(entry_to_ref_change(&entry));
                            }
                        } else {
                            self.consume_unique_direct_common_ref_matching_timestamp(
                                cmd,
                                &change.old,
                                &change.new,
                                timestamp,
                                &mut changes,
                            )?;
                        }
                    }
                } else {
                    self.consume_common_refs_matching_transition(
                        &change.old,
                        &change.new,
                        &mut changes,
                    )?;
                }
            } else if let Some(entry) = self.find_head_entry(
                cmd.worktree.as_deref(),
                &[],
                ExpectedTransition {
                    old_oids: spec.old_oid.iter().cloned().collect(),
                    new_oid: Some(spec.new_oid.clone()),
                    messages: HashSet::new(),
                },
            )? {
                self.consume_entry(&entry)?;
                changes.push(entry_to_ref_change(&entry));
                self.consume_common_refs_matching_transition(&entry.old, &entry.new, &mut changes)?;
            }
        } else if let Some(change) = direct_update_ref_change_from_argv(&spec) {
            let old = change.old.clone();
            let new = change.new.clone();
            changes.push(change);
            if let Some(branch) =
                self.find_direct_ref_transition_entry(cmd, &spec.reference, &old, &new)?
            {
                let branch_timestamp = branch.timestamp_secs;
                self.consume_entry(&branch)?;
                let branch_can_affect_head = current_worktree_branch_ref(cmd, state)
                    .is_none_or(|current_branch| current_branch == spec.reference);
                if branch_can_affect_head
                    && let Some(timestamp) = branch_timestamp
                    && let Some(head) = self
                        .direct_ref_transition_entries(cmd, "HEAD", &old, &new)?
                        .into_iter()
                        .find(|entry| entry.timestamp_secs == Some(timestamp))
                {
                    self.consume_entry(&head)?;
                    changes.push(entry_to_ref_change(&head));
                }
            }
        } else if let Some(entry) = self.find_common_ref_entry(
            &spec.reference,
            ExpectedTransition {
                old_oids: spec.old_oid.iter().cloned().collect(),
                new_oid: Some(spec.new_oid.clone()),
                messages: HashSet::new(),
            },
            &[],
        )? {
            self.consume_entry(&entry)?;
            let old = entry.old.clone();
            let new = entry.new.clone();
            changes.push(entry_to_ref_change(&entry));
            if let Some(head) = self.find_head_entry(
                cmd.worktree.as_deref(),
                &[],
                ExpectedTransition {
                    old_oids: [old.clone()].into_iter().collect(),
                    new_oid: Some(new.clone()),
                    messages: HashSet::new(),
                },
            )? {
                self.consume_entry(&head)?;
                changes.push(entry_to_ref_change(&head));
            }
        }

        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    fn enrich_unstructured_ref_mutation(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let mut changes = Vec::new();
        if let Some(worktree) = cmd.worktree.as_deref() {
            while let Some(entry) = self.find_head_entry_without_hint(
                Some(worktree),
                &[],
                ExpectedTransition::default(),
            )? {
                self.consume_entry(&entry)?;
                changes.push(entry_to_ref_change(&entry));
            }
        }
        if !changes.iter().any(|change| change.reference == "HEAD")
            && let Some(entry) = self.find_late_unstructured_head_entry(cmd, state)?
        {
            self.consume_entry(&entry)?;
            changes.push(entry_to_ref_change(&entry));
        }
        self.drain_unstructured_common_ref_entries(&mut changes)?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    /// Recover a fast unstructured mutation whose asynchronous command-start
    /// sample landed after Git had already appended the command's HEAD reflog
    /// row. The normal cursor cannot walk backwards from that late cold seed,
    /// so match the full log against the sequencer's pre-command HEAD and the
    /// command's wall-clock window. This state anchor prevents an older,
    /// unrelated ref mutation from being claimed by the traced command.
    fn find_late_unstructured_head_entry(
        &self,
        cmd: &NormalizedCommand,
        state: &FamilyState,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = cmd.worktree.as_deref() else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let key = head_key(&git_dir);
        let Some(sampled_offset) = cmd.reflog_start_offsets.get(&key).copied() else {
            return Ok(None);
        };
        let Some(old_head) = sequenced_head_before_command(worktree, state) else {
            return Ok(None);
        };
        let command_window = reflog_timestamp_window(cmd);
        let path = git_dir.join("logs").join("HEAD");

        Ok(read_reflog_entries(key, &path, "HEAD", None)?
            .into_iter()
            .rfind(|entry| {
                entry.end_offset <= sampled_offset
                    && !self.entry_consumed(entry)
                    && entry.old == old_head
                    && valid_ref_transition(&entry.old, &entry.new)
                    && entry
                        .timestamp_secs
                        .is_some_and(|timestamp| command_window.contains(timestamp))
            }))
    }

    fn drain_unstructured_common_ref_entries(
        &mut self,
        changes: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        for reference in self.discover_common_refs()? {
            if reference == "HEAD" || reference == "ORIG_HEAD" {
                continue;
            }
            while let Some(entry) = self.find_common_ref_entry_without_hint(
                &reference,
                ExpectedTransition::default(),
                &[],
            )? {
                self.consume_entry(&entry)?;
                changes.push(entry_to_ref_change(&entry));
            }
        }
        Ok(())
    }

    fn enrich_stash(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = command_args(cmd);
        let stash_args = stash_command_args(&args);
        let kind = stash_args.first().map(String::as_str).unwrap_or("push");

        if matches!(kind, "apply" | "pop" | "drop" | "branch") {
            let target = stash_target_arg(stash_args, kind);
            cmd.stash_target_oid = self.resolve_stash_target_at_cursor(target)?;
        } else if kind == "store" {
            cmd.stash_target_oid = stash_store_target_arg(stash_args).cloned();
        }

        if matches!(kind, "push" | "save" | "store") {
            if let Some(entry) = self.find_stash_push_entry(stash_args, kind)? {
                self.consume_entry(&entry)?;
                self.apply_stash_ref_entry(kind, &entry);
                cmd.ref_changes.push(entry_to_ref_change(&entry));
            }
        } else if matches!(kind, "pop" | "drop") {
            self.consume_destructive_stash_operation(stash_target_arg(stash_args, kind), cmd)?;
        } else if kind == "clear" {
            self.stash_stack.clear();
            self.sync_common_ref_cursor_to_log_end_after_rewrite("refs/stash")?;
        }

        if matches!(kind, "apply" | "pop" | "branch")
            && (kind == "branch" || !state.refs.contains_key("HEAD"))
        {
            let expected = if kind == "branch" {
                self.head_expected_transition(cmd, state)
            } else {
                ExpectedTransition::default()
            };
            if let Some(head) = self.find_head_entry(cmd.worktree.as_deref(), &[], expected)?
                && message_matches(&head.message, &["reset:", "checkout:"])
            {
                self.consume_entry(&head)?;
                cmd.ref_changes.push(entry_to_ref_change(&head));
            }
        }

        Ok(())
    }

    fn consume_destructive_stash_operation(
        &mut self,
        target: Option<&String>,
        cmd: &mut NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let key = common_key("refs/stash");
        let old_cursor = self.offsets.get(&key).copied();
        let log_len_after = self.common_ref_log_len("refs/stash")?;
        let log_was_rewritten = match (old_cursor, log_len_after) {
            (Some(cursor), Some(len)) => len < cursor,
            (Some(_), None) => true,
            _ => false,
        };

        if !log_was_rewritten {
            return Ok(());
        }

        let target_oid = cmd
            .stash_target_oid
            .clone()
            .or_else(|| self.resolve_stash_target_at_cursor(target).ok().flatten());
        let Some(target_oid) = target_oid else {
            self.sync_common_ref_cursor_to_log_end_after_rewrite("refs/stash")?;
            return Ok(());
        };

        let target_index = stash_target_index(target);
        let old_top = self.stash_stack.first().cloned();
        self.remove_stash_from_stack(target_index, &target_oid);
        let new_top = self.stash_stack.first().cloned().unwrap_or_else(zero_oid);

        if old_top.as_deref() == Some(target_oid.as_str()) {
            cmd.ref_changes.push(RefChange {
                reference: "refs/stash".to_string(),
                old: target_oid.clone(),
                new: new_top,
            });
        }
        if cmd.stash_target_oid.is_none() {
            cmd.stash_target_oid = Some(target_oid);
        }

        self.sync_common_ref_cursor_to_log_end_after_rewrite("refs/stash")?;
        Ok(())
    }

    fn consume_rebase_transition(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        if cmd.exit_code != 0 && self.consume_failed_explicit_branch_rebase_start(cmd)? {
            return Ok(());
        }

        // A failed control-mode rebase (--continue/--skip that stopped on a
        // later conflict) may have produced no reflog entry of its own (e.g.
        // an empty resolution git skipped). Because trace processing is
        // asynchronous, the entry written by the EVENTUAL successful continue
        // can already be in the reflog by the time this command is enriched;
        // consuming it here starves that final continue of its evidence and
        // loses the rebased-branch transition (and with it the migration of
        // conflict-resolution working logs). Failed commands take no side
        // effects, so their ref evidence is never used: consume nothing. Any
        // entry this command did create is picked up by the successful
        // continue's chain walk below.
        if cmd.exit_code != 0 && summarize_rebase_args(&rebase_command_args(cmd)).is_control_mode {
            return Ok(());
        }

        let expected = self.head_expected_transition(cmd, state);
        let first = match self.find_rebase_start_entry(cmd, expected.clone())? {
            Some(entry) => Some(entry),
            None => self.find_head_entry_without_hint(
                cmd.worktree.as_deref(),
                &["rebase"],
                expected.clone(),
            )?,
        };
        crate::wltrace::wltrace(
            "ref_cursor.rebase_first_entry",
            cmd.worktree.as_deref().unwrap_or(std::path::Path::new("")),
            || {
                let cursor = cmd
                    .worktree
                    .as_deref()
                    .and_then(git_dir_for_worktree)
                    .map(|git_dir| head_key(&git_dir))
                    .map(|key| {
                        format!(
                            "offset={:?} hint={:?}",
                            self.offsets.get(&key),
                            self.command_start_hints.get(&key)
                        )
                    })
                    .unwrap_or_else(|| "no-key".to_string());
                format!(
                    "sid={} exit={} first={} expected_old={:?} expected_new={:?} {cursor}",
                    cmd.root_sid,
                    cmd.exit_code,
                    first
                        .as_ref()
                        .map(|entry| format!(
                            "{}->{} msg={}",
                            &entry.old[..8.min(entry.old.len())],
                            &entry.new[..8.min(entry.new.len())],
                            entry.message.chars().take(40).collect::<String>()
                        ))
                        .unwrap_or_else(|| "NONE".to_string()),
                    expected
                        .old_oids
                        .iter()
                        .map(|oid| oid.chars().take(8).collect::<String>())
                        .collect::<Vec<_>>(),
                    expected
                        .new_oid
                        .as_ref()
                        .map(|oid| &oid[..8.min(oid.len())]),
                )
            },
        );
        let Some(first) = first else {
            return Ok(());
        };

        let mut changes = vec![entry_to_ref_change(&first)];
        let old = first.old.clone();
        let mut new = first.new.clone();
        self.consume_entry(&first)?;

        let failed = cmd.exit_code != 0;
        if failed {
            cmd.ref_changes = changes;
            return Ok(());
        }

        let mut consumed_finish = rebase_reflog_action_is(&first.message, "finish");
        let mut next_start = first.end_offset;
        while !consumed_finish {
            let Some(next) = self.find_head_entry_after(
                cmd.worktree.as_deref(),
                next_start,
                &["rebase"],
                ExpectedTransition {
                    old_oids: [new.clone()].into_iter().collect(),
                    new_oid: None,
                    messages: HashSet::new(),
                },
            )?
            else {
                break;
            };
            if rebase_reflog_action_is(&next.message, "start") {
                break;
            }
            new = next.new.clone();
            consumed_finish = rebase_reflog_action_is(&next.message, "finish");
            next_start = next.end_offset;
            self.consume_entry(&next)?;
            changes.push(entry_to_ref_change(&next));
        }

        self.consume_common_refs_matching_transition(&old, &new, &mut changes)?;
        self.consume_rebase_finish_branch_ref(cmd.worktree.as_deref(), &new, state, &mut changes)?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    // DEFERRED (code-review #14): this finder scans the reflog from
    // reflog_start_offset and takes the first unconsumed entry matching the
    // message/transition; it does not use the command's ingress hint (the
    // reflog position at which THIS command began) to bound the search. In
    // pathological histories with repeated identical rebase-start messages and
    // transitions, it could match an earlier same-shaped entry than the one
    // this command produced. Bounding by the per-command ingress offset would
    // make the match exact; deferred as it needs the ingress offset threaded
    // through to the finder.
    fn find_rebase_start_entry(
        &mut self,
        cmd: &NormalizedCommand,
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = cmd.worktree.as_deref() else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let args = rebase_command_args(cmd);
        let target = rebase_start_checkout_target_from_args(&args);
        let key = head_key(&git_dir);
        let path = git_dir.join("logs").join("HEAD");
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key, &path, "HEAD", start)?;

        Ok(entries.into_iter().find(|entry| {
            !self.entry_consumed(entry)
                && rebase_reflog_action_is(&entry.message, "start")
                && expected.matches_span_boundary(entry)
                && target
                    .as_deref()
                    .is_none_or(|target| rebase_start_message_targets(&entry.message, target))
        }))
    }

    fn consume_failed_explicit_branch_rebase_start(
        &mut self,
        cmd: &mut NormalizedCommand,
    ) -> Result<bool, GitAiError> {
        let args = rebase_command_args(cmd);
        let Some(branch_arg) = explicit_rebase_branch_arg(&args) else {
            return Ok(false);
        };
        let Some(worktree) = cmd.worktree.as_deref() else {
            return Ok(false);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(false);
        };

        let branch_ref = branch_arg_to_ref(&branch_arg);
        let head_key = head_key(&git_dir);
        let head_path = git_dir.join("logs").join("HEAD");
        let start = self.reflog_start_offset(&head_key, &head_path)?;
        let head_entries =
            read_reflog_entries_including_noops(head_key, &head_path, "HEAD", start)?;
        let Some(start_marker) =
            rebase_start_marker_for_explicit_branch(&head_entries, &branch_ref)
        else {
            return Ok(false);
        };

        let finish_new = latest_rebase_finish_for_branch(&head_entries, &branch_ref)
            .filter(|finish| finish.end_offset > start_marker.end_offset)
            .map(|finish| finish.new.as_str());
        let original_head =
            self.original_head_for_explicit_rebase_branch(&branch_ref, finish_new)?;

        let mut changes = vec![entry_to_ref_change(start_marker)];
        if let Some(original_head) = original_head {
            changes.push(RefChange {
                reference: branch_ref,
                old: original_head.clone(),
                new: original_head,
            });
        }

        self.advance_cursor_to_entry(start_marker);
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(true)
    }

    fn consume_pull_transition(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let action = pull_reflog_action(cmd);
        let prefixes = pull_reflog_message_prefixes(&action);
        let prefix_refs = prefixes.iter().map(String::as_str).collect::<Vec<_>>();
        let expected = self.head_expected_transition(cmd, state);
        self.consume_pull_head_span_for_action(cmd, state, &prefix_refs, expected, &action)
    }

    fn consume_head_entry_for_command(
        &mut self,
        cmd: &mut NormalizedCommand,
        entry: CursorEntry,
    ) -> Result<(), GitAiError> {
        crate::wltrace::wltrace(
            "ref_cursor.consume_head_entry",
            cmd.worktree.as_deref().unwrap_or(Path::new("")),
            || {
                format!(
                    "sid={} cmd={} entry={}->{}@{} msg={}",
                    cmd.root_sid,
                    cmd.primary_command.as_deref().unwrap_or("unknown"),
                    &entry.old[..8.min(entry.old.len())],
                    &entry.new[..8.min(entry.new.len())],
                    entry.start_offset,
                    entry.message.chars().take(40).collect::<String>(),
                )
            },
        );
        self.consume_entry(&entry)?;
        let old = entry.old.clone();
        let new = entry.new.clone();
        let mut changes = vec![entry_to_ref_change(&entry)];
        self.consume_common_refs_matching_transition(&old, &new, &mut changes)?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    fn consume_head_transition_for_command(
        &mut self,
        cmd: &mut NormalizedCommand,
        _state: &FamilyState,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<(), GitAiError> {
        let Some(entry) =
            self.find_head_entry(cmd.worktree.as_deref(), message_prefixes, expected)?
        else {
            return Ok(());
        };

        self.consume_head_entry_for_command(cmd, entry)
    }

    fn consume_head_span_for_command_limited(
        &mut self,
        cmd: &mut NormalizedCommand,
        _state: &FamilyState,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
        limit: usize,
    ) -> Result<(), GitAiError> {
        if limit == 0 {
            return Ok(());
        }
        let Some(first) = self.find_head_span_start_entry(
            cmd.worktree.as_deref(),
            message_prefixes,
            expected,
            limit,
        )?
        else {
            return Ok(());
        };

        let old = first.old.clone();
        let mut new = first.new.clone();
        let mut changes = vec![entry_to_ref_change(&first)];
        let mut next_start = first.end_offset;
        self.consume_entry(&first)?;

        while changes.len() < limit
            && let Some(next) = self.find_head_entry_after(
                cmd.worktree.as_deref(),
                next_start,
                message_prefixes,
                ExpectedTransition {
                    old_oids: [new.clone()].into_iter().collect(),
                    new_oid: None,
                    messages: HashSet::new(),
                },
            )?
        {
            new = next.new.clone();
            next_start = next.end_offset;
            self.consume_entry(&next)?;
            changes.push(entry_to_ref_change(&next));
        }

        self.consume_common_refs_matching_transition(&old, &new, &mut changes)?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    fn consume_pull_head_span_for_action(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
        action: &str,
    ) -> Result<(), GitAiError> {
        let first = match self.find_pull_start_entry(cmd, expected.clone(), action)? {
            Some(entry) => Some(entry),
            None => self.find_head_entry_without_hint(
                cmd.worktree.as_deref(),
                message_prefixes,
                expected,
            )?,
        };
        let Some(first) = first else {
            return Ok(());
        };

        let old = first.old.clone();
        let mut new = first.new.clone();
        let mut changes = vec![entry_to_ref_change(&first)];
        let mut consumed_finish = pull_reflog_action_state(&first.message, action).is_none()
            || pull_reflog_action_is(&first.message, action, "finish");
        let mut next_start = first.end_offset;
        self.consume_entry(&first)?;

        while !consumed_finish
            && let Some(next) = self.find_head_entry_after(
                cmd.worktree.as_deref(),
                next_start,
                message_prefixes,
                ExpectedTransition {
                    old_oids: [new.clone()].into_iter().collect(),
                    new_oid: None,
                    messages: HashSet::new(),
                },
            )?
        {
            if pull_reflog_action_starts_new_command(&next.message, action) {
                break;
            }
            new = next.new.clone();
            consumed_finish = pull_reflog_action_is(&next.message, action, "finish");
            next_start = next.end_offset;
            self.consume_entry(&next)?;
            changes.push(entry_to_ref_change(&next));
        }

        self.consume_common_refs_matching_transition(&old, &new, &mut changes)?;
        self.consume_pull_finish_branch_ref(
            cmd.worktree.as_deref(),
            &new,
            state,
            action,
            message_prefixes,
            &mut changes,
        )?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    fn find_pull_start_entry(
        &mut self,
        cmd: &NormalizedCommand,
        expected: ExpectedTransition,
        action: &str,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = cmd.worktree.as_deref() else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let key = head_key(&git_dir);
        let path = git_dir.join("logs").join("HEAD");
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key, &path, "HEAD", start)?;

        Ok(entries.into_iter().find(|entry| {
            !self.entry_consumed(entry)
                && pull_reflog_action_is(&entry.message, action, "start")
                && expected.matches_span_boundary(entry)
        }))
    }

    // Span commands can append several contiguous HEAD rows. If command ingress
    // captured a late reflog offset, prefer the first matching span at/after that
    // hint, then fall back to the latest matching span before the hint when the
    // hint landed after the command's own rows.
    fn find_head_span_start_entry(
        &mut self,
        worktree: Option<&Path>,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
        limit: usize,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = worktree else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let path = git_dir.join("logs").join("HEAD");
        let key = head_key(&git_dir);
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key, &path, "HEAD", start)?;
        let mut contiguous = VecDeque::<CursorEntry>::new();
        let hint = self.command_start_hints.get(&head_key(&git_dir)).copied();
        let mut latest_before_hint: Option<CursorEntry> = None;

        for entry in entries {
            if self.entry_consumed(&entry) || !message_matches(&entry.message, message_prefixes) {
                contiguous.clear();
                continue;
            }
            if contiguous
                .back()
                .is_some_and(|previous| previous.new != entry.old)
            {
                contiguous.clear();
            }
            let matches = expected.matches(&entry);
            contiguous.push_back(entry);
            while contiguous.len() > limit {
                contiguous.pop_front();
            }
            if matches {
                let Some(candidate) = contiguous.front().cloned() else {
                    continue;
                };
                let Some(hint) = hint else {
                    return Ok(Some(candidate));
                };
                if candidate.start_offset >= hint {
                    return Ok(Some(candidate));
                }
                latest_before_hint = Some(candidate);
            }
        }

        Ok(latest_before_hint)
    }

    fn find_head_entry_in_command_window(
        &mut self,
        cmd: &NormalizedCommand,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = cmd.worktree.as_deref() else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let key = head_key(&git_dir);
        let path = git_dir.join("logs").join("HEAD");
        let start = self.reflog_start_offset(&key, &path)?;
        let command_window = reflog_timestamp_window(cmd);
        let candidates = read_reflog_entries(key.clone(), &path, "HEAD", start)?
            .into_iter()
            .filter(|entry| {
                !self.entry_consumed(entry)
                    && expected.matches(entry)
                    && message_matches(&entry.message, message_prefixes)
                    && entry
                        .timestamp_secs
                        .is_some_and(|timestamp| command_window.contains(timestamp))
            })
            .collect::<Vec<_>>();

        if self.command_start_hints.contains_key(&key) {
            return Ok(self.select_candidate_with_hint(&key, candidates));
        }
        Ok(match candidates.as_slice() {
            [entry] => Some(entry.clone()),
            _ => None,
        })
    }

    fn find_head_entry(
        &mut self,
        worktree: Option<&Path>,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = worktree else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let path = git_dir.join("logs").join("HEAD");
        self.find_entry_in_log(
            head_key(&git_dir),
            &path,
            "HEAD",
            expected,
            message_prefixes,
        )
    }

    fn find_head_entry_without_hint(
        &mut self,
        worktree: Option<&Path>,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = worktree else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let path = git_dir.join("logs").join("HEAD");
        self.find_entry_in_log_with_hint(
            head_key(&git_dir),
            &path,
            "HEAD",
            expected,
            message_prefixes,
            false,
        )
    }

    fn find_head_entry_after(
        &mut self,
        worktree: Option<&Path>,
        start_offset: u64,
        message_prefixes: &[&str],
        expected: ExpectedTransition,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let Some(worktree) = worktree else {
            return Ok(None);
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(None);
        };
        let key = head_key(&git_dir);
        let path = git_dir.join("logs").join("HEAD");
        Ok(read_reflog_entries(key, &path, "HEAD", Some(start_offset))?
            .into_iter()
            .find(|entry| {
                !self.entry_consumed(entry)
                    && expected.matches(entry)
                    && message_matches(&entry.message, message_prefixes)
            }))
    }

    fn consume_unique_direct_common_ref_matching_timestamp(
        &mut self,
        cmd: &NormalizedCommand,
        old: &str,
        new: &str,
        timestamp: i64,
        out: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        let mut matches = Vec::new();
        for reference in self.discover_common_refs()? {
            if reference == "ORIG_HEAD" || reference == "refs/stash" {
                continue;
            }
            matches.extend(
                self.direct_ref_transition_entries(cmd, &reference, old, new)?
                    .into_iter()
                    .filter(|entry| entry.timestamp_secs == Some(timestamp)),
            );
        }
        if matches.len() == 1 {
            let entry = matches.remove(0);
            self.consume_entry(&entry)?;
            out.push(entry_to_ref_change(&entry));
        }
        Ok(())
    }

    fn find_direct_ref_transition_entry(
        &mut self,
        cmd: &NormalizedCommand,
        reference: &str,
        old: &str,
        new: &str,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        Ok(self
            .direct_ref_transition_entries(cmd, reference, old, new)?
            .into_iter()
            .next())
    }

    fn direct_ref_transition_entries(
        &mut self,
        cmd: &NormalizedCommand,
        reference: &str,
        old: &str,
        new: &str,
    ) -> Result<Vec<CursorEntry>, GitAiError> {
        let (key, path) = if reference == "HEAD" {
            let Some(worktree) = cmd.worktree.as_deref() else {
                return Ok(Vec::new());
            };
            let Some(git_dir) = git_dir_for_worktree(worktree) else {
                return Ok(Vec::new());
            };
            (head_key(&git_dir), git_dir.join("logs").join("HEAD"))
        } else {
            (
                common_key(reference),
                self.common_dir().join("logs").join(reference),
            )
        };

        let command_window = reflog_timestamp_window(cmd);
        let matches_command = |entry: &CursorEntry, this: &Self| {
            !this.entry_consumed(entry)
                && entry.old == old
                && entry.new == new
                && entry
                    .timestamp_secs
                    .is_some_and(|timestamp| command_window.contains(timestamp))
        };

        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key.clone(), &path, reference, start)?;
        let matches = entries
            .into_iter()
            .filter(|entry| matches_command(entry, self))
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return Ok(matches);
        }

        Ok(read_reflog_entries(key, &path, reference, None)?
            .into_iter()
            .filter(|entry| matches_command(entry, self))
            .collect())
    }

    fn find_common_ref_entry(
        &mut self,
        reference: &str,
        expected: ExpectedTransition,
        message_prefixes: &[&str],
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let path = self.common_dir().join("logs").join(reference);
        self.find_entry_in_log(
            common_key(reference),
            &path,
            reference,
            expected,
            message_prefixes,
        )
    }

    fn find_common_ref_entry_without_hint(
        &mut self,
        reference: &str,
        expected: ExpectedTransition,
        message_prefixes: &[&str],
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let path = self.common_dir().join("logs").join(reference);
        self.find_entry_in_log_with_hint(
            common_key(reference),
            &path,
            reference,
            expected,
            message_prefixes,
            false,
        )
    }

    fn find_stash_push_entry(
        &mut self,
        stash_args: &[String],
        kind: &str,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let expected_message = stash_push_message_from_args(stash_args, kind);
        let path = self.common_dir().join("logs").join("refs/stash");
        let key = common_key("refs/stash");
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key, &path, "refs/stash", start)?;

        Ok(entries.into_iter().find(|entry| {
            !self.entry_consumed(entry)
                && expected_message
                    .as_deref()
                    .is_none_or(|message| stash_reflog_message_matches(&entry.message, message))
        }))
    }

    fn find_entry_in_log(
        &mut self,
        key: String,
        path: &Path,
        reference: &str,
        expected: ExpectedTransition,
        message_prefixes: &[&str],
    ) -> Result<Option<CursorEntry>, GitAiError> {
        self.find_entry_in_log_with_hint(key, path, reference, expected, message_prefixes, true)
    }

    fn find_entry_in_log_with_hint(
        &mut self,
        key: String,
        path: &Path,
        reference: &str,
        expected: ExpectedTransition,
        message_prefixes: &[&str],
        use_hint: bool,
    ) -> Result<Option<CursorEntry>, GitAiError> {
        let start = self.reflog_start_offset(&key, path)?;
        let entries = read_reflog_entries(key.clone(), path, reference, start)?;
        let mut candidates = entries.into_iter().filter(|entry| {
            !self.entry_consumed(entry)
                && expected.matches(entry)
                && message_matches(&entry.message, message_prefixes)
        });
        if use_hint {
            Ok(self.select_candidate_with_hint(&key, candidates))
        } else {
            Ok(candidates.next())
        }
    }

    /// Choose among reflog entries that match a command's expected transition,
    /// biased by the soft command-start hint (the daemon-ingress reflog offset).
    ///
    /// The in-order cursor already bounds `candidates` to unconsumed entries. The
    /// hint disambiguates *which* matching entry is this command's: an untraced
    /// commit can share a message with the traced one, and the hint — captured at
    /// the command's true start — sits after the untraced entry but before the
    /// traced entry, so we prefer the first match at/after it.
    ///
    /// But the hint is captured asynchronously and can also race *behind* git,
    /// landing after the command's own entry (the graphite/gt-create flake). In
    /// that case no match exists at/after the hint, so we fall back to the latest
    /// match before the hint. That still preserves the single-candidate case, and
    /// it avoids consuming an older untraced duplicate-message commit when both
    /// the untraced commit and this command's commit sit before the late hint.
    fn select_candidate_with_hint<I>(&self, key: &str, candidates: I) -> Option<CursorEntry>
    where
        I: IntoIterator<Item = CursorEntry>,
    {
        let Some(hint) = self.command_start_hints.get(key).copied() else {
            return candidates.into_iter().next();
        };
        let mut latest_before_hint: Option<CursorEntry> = None;
        for entry in candidates {
            // An entry "at/after the hint" is one whose start offset is >= the
            // hint. Prefer the first such entry (skips an untraced collision the
            // hint was captured after). If the hint raced past the command's own
            // entry, remember the latest matching entry before the hint.
            if entry.start_offset >= hint {
                return Some(entry);
            }
            latest_before_hint = Some(entry);
        }
        latest_before_hint
    }

    fn consume_common_refs_matching_transition(
        &mut self,
        old: &str,
        new: &str,
        out: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        let refs = self.discover_common_refs()?;
        for reference in refs {
            if reference == "HEAD" || reference == "ORIG_HEAD" || reference == "refs/stash" {
                continue;
            }
            let expected = ExpectedTransition {
                old_oids: [old.to_string()].into_iter().collect(),
                new_oid: Some(new.to_string()),
                messages: HashSet::new(),
            };
            if let Some(entry) = self.find_common_ref_entry(&reference, expected, &[])? {
                self.consume_entry(&entry)?;
                out.push(entry_to_ref_change(&entry));
            }
        }
        Ok(())
    }

    fn consume_rebase_finish_branch_ref(
        &mut self,
        worktree: Option<&Path>,
        new: &str,
        state: &FamilyState,
        out: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        self.consume_finish_branch_ref(
            worktree,
            new,
            state,
            &["rebase"],
            |message| rebase_finish_returned_branch(message).map(ToOwned::to_owned),
            out,
        )
    }

    fn consume_pull_finish_branch_ref(
        &mut self,
        worktree: Option<&Path>,
        new: &str,
        state: &FamilyState,
        action: &str,
        message_prefixes: &[&str],
        out: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError> {
        self.consume_finish_branch_ref(
            worktree,
            new,
            state,
            message_prefixes,
            |message| pull_finish_returned_branch(message, action),
            out,
        )
    }

    fn consume_finish_branch_ref<F>(
        &mut self,
        worktree: Option<&Path>,
        new: &str,
        state: &FamilyState,
        message_prefixes: &[&str],
        branch_from_message: F,
        out: &mut Vec<RefChange>,
    ) -> Result<(), GitAiError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(worktree) = worktree else {
            return Ok(());
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return Ok(());
        };
        let key = head_key(&git_dir);
        let path = git_dir.join("logs").join("HEAD");
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries_including_noops(key, &path, "HEAD", start)?;
        let Some(finish) = entries
            .into_iter()
            .find(|entry| entry.new == new && branch_from_message(&entry.message).is_some())
        else {
            return Ok(());
        };
        let Some(branch_ref) = branch_from_message(&finish.message) else {
            return Ok(());
        };

        self.advance_cursor_to_entry(&finish);
        let old_oids = state
            .refs
            .get(&branch_ref)
            .filter(|oid| valid_non_zero_oid(oid))
            .cloned()
            .into_iter()
            .collect();
        if let Some(entry) = self.find_common_ref_entry(
            &branch_ref,
            ExpectedTransition {
                old_oids,
                new_oid: Some(new.to_string()),
                messages: HashSet::new(),
            },
            message_prefixes,
        )? {
            self.consume_entry(&entry)?;
            out.push(entry_to_ref_change(&entry));
        }
        Ok(())
    }

    fn resolve_stash_target_at_cursor(
        &self,
        target: Option<&String>,
    ) -> Result<Option<String>, GitAiError> {
        let target = target.map(String::as_str).unwrap_or("stash@{0}");
        if is_valid_git_oid(target) {
            return Ok(Some(target.to_string()));
        }
        if matches!(target, "stash" | "refs/stash") {
            return self.resolve_stash_target_at_cursor(Some(&"stash@{0}".to_string()));
        }
        let Some(index) = target
            .strip_prefix("stash@{")
            .and_then(|value| value.strip_suffix('}'))
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return Ok(None);
        };
        if let Some(oid) = self.stash_stack.get(index) {
            return Ok(Some(oid.clone()));
        }
        let path = self.common_dir().join("logs").join("refs/stash");
        let key = common_key("refs/stash");
        let entries = read_reflog_entries(key.clone(), &path, "refs/stash", Some(0))?;
        let cursor = self.offsets.get(&key).copied().unwrap_or(u64::MAX);
        let mut stack = entries
            .into_iter()
            .filter(|entry| entry.end_offset <= cursor)
            .filter(|entry| valid_non_zero_oid(&entry.new))
            .map(|entry| entry.new)
            .collect::<Vec<_>>();
        stack.reverse();
        Ok(stack.get(index).cloned())
    }

    fn apply_stash_ref_entry(&mut self, kind: &str, entry: &CursorEntry) {
        match kind {
            "push" | "save" | "store" => {
                if valid_non_zero_oid(&entry.new)
                    && !self.stash_stack.iter().any(|oid| oid == &entry.new)
                {
                    self.stash_stack.insert(0, entry.new.clone());
                }
            }
            "pop" | "drop" | "branch" => {
                if let Some(position) = self.stash_stack.iter().position(|oid| oid == &entry.old) {
                    self.stash_stack.remove(position);
                }
                if valid_non_zero_oid(&entry.new)
                    && !self.stash_stack.iter().any(|oid| oid == &entry.new)
                {
                    self.stash_stack.insert(0, entry.new.clone());
                }
            }
            _ => {}
        }
    }

    fn discover_common_refs(&self) -> Result<Vec<String>, GitAiError> {
        let logs = self.common_dir().join("logs");
        let mut refs = Vec::new();
        discover_reflog_refs(&logs, &logs, &mut refs)?;
        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    fn entry_consumed(&self, entry: &CursorEntry) -> bool {
        self.consumed_offsets
            .get(&entry.key)
            .is_some_and(|offsets| offsets.contains(&entry.end_offset))
            && self
                .consumed_anchors
                .get(&entry.key)
                .and_then(|anchors| anchors.get(&entry.end_offset))
                .is_some_and(|anchor| anchor == &ReflogAnchor::from(entry))
    }

    fn reflog_start_offset(&mut self, key: &str, path: &Path) -> Result<Option<u64>, GitAiError> {
        let Some(offset) = self.offsets.get(key).copied() else {
            return Ok(None);
        };
        if offset == 0 {
            return Ok(Some(0));
        }

        let len = match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.clear_ref_cursor(key);
                return Ok(None);
            }
            Err(error) => return Err(GitAiError::IoError(error)),
        };
        if offset > len {
            self.clear_ref_cursor(key);
            return Ok(None);
        }

        if let Some(anchor) = self.anchors.get(key) {
            let record = read_reflog_record_ending_at(path, offset)?;
            if record.as_ref().map(ReflogAnchor::from) != Some(anchor.clone()) {
                self.clear_ref_cursor(key);
                return Ok(None);
            }
        }

        Ok(Some(offset))
    }

    fn initialize_reflog_cursor(&mut self, key: &str, offset: u64) -> Result<(), GitAiError> {
        self.offsets.insert(key.to_string(), offset);
        self.consumed_offsets.remove(key);
        self.consumed_anchors.remove(key);
        if offset == 0 {
            self.anchors.remove(key);
            return Ok(());
        }
        let Some(path) = self.reflog_path_for_key(key) else {
            self.anchors.remove(key);
            return Ok(());
        };
        if let Some(record) = read_reflog_record_ending_at(&path, offset)? {
            self.anchors
                .insert(key.to_string(), ReflogAnchor::from(&record));
        } else {
            self.anchors.remove(key);
        }
        Ok(())
    }

    fn command_start_offset_is_authoritative(
        &self,
        key: &str,
        offset: u64,
    ) -> Result<bool, GitAiError> {
        let Some(existing) = self.offsets.get(key).copied() else {
            if key.starts_with("common:") {
                return Ok(true);
            }
            return self.reflog_has_records_after_offset(key, offset);
        };
        if existing >= offset {
            return Ok(false);
        }
        self.reflog_has_records_after_offset(key, offset)
    }

    fn reflog_has_records_after_offset(&self, key: &str, offset: u64) -> Result<bool, GitAiError> {
        let Some(path) = self.reflog_path_for_key(key) else {
            return Ok(false);
        };
        let len = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(GitAiError::IoError(error)),
        };
        if offset >= len {
            return Ok(false);
        }
        Ok(!read_reflog_records(&path, Some(offset))?.is_empty())
    }

    fn reflog_path_for_key(&self, key: &str) -> Option<PathBuf> {
        if let Some(reference) = key.strip_prefix("common:") {
            return Some(self.common_dir().join("logs").join(reference));
        }
        let git_dir = key
            .strip_prefix("worktree:")
            .and_then(|value| value.strip_suffix(":HEAD"))?;
        Some(PathBuf::from(git_dir).join("logs").join("HEAD"))
    }

    fn ref_tip_at_cursor_start(&self, key: &str) -> Option<String> {
        self.anchors
            .get(key)
            .map(|anchor| anchor.new.clone())
            .filter(|oid| valid_non_zero_oid(oid))
    }

    fn consume_entry(&mut self, entry: &CursorEntry) -> Result<(), GitAiError> {
        self.consumed_offsets
            .entry(entry.key.clone())
            .or_default()
            .insert(entry.end_offset);
        self.consumed_anchors
            .entry(entry.key.clone())
            .or_default()
            .insert(entry.end_offset, ReflogAnchor::from(entry));
        self.compact_consumed_entries(&entry.key, &entry.path, &entry.reference)
    }

    fn advance_cursor_to_entry(&mut self, entry: &CursorEntry) {
        self.offsets.insert(entry.key.clone(), entry.end_offset);
        self.anchors
            .insert(entry.key.clone(), ReflogAnchor::from(entry));
        self.consumed_offsets.remove(&entry.key);
        self.consumed_anchors.remove(&entry.key);
    }

    fn compact_consumed_entries(
        &mut self,
        key: &str,
        path: &Path,
        reference: &str,
    ) -> Result<(), GitAiError> {
        let start = self.offsets.get(key).copied();
        let entries = read_reflog_entries(key.to_string(), path, reference, start)?;
        let mut advanced_to = start.unwrap_or(0);
        let mut anchor = None;
        for entry in entries {
            if self.entry_consumed(&entry) {
                advanced_to = entry.end_offset;
                anchor = Some(ReflogAnchor::from(&entry));
            } else {
                break;
            }
        }

        if advanced_to > start.unwrap_or(0) {
            self.offsets.insert(key.to_string(), advanced_to);
            if let Some(anchor) = anchor {
                self.anchors.insert(key.to_string(), anchor);
            }
            if let Some(consumed) = self.consumed_offsets.get_mut(key) {
                consumed.retain(|offset| *offset > advanced_to);
                if consumed.is_empty() {
                    self.consumed_offsets.remove(key);
                }
            }
            if let Some(anchors) = self.consumed_anchors.get_mut(key) {
                anchors.retain(|offset, _| *offset > advanced_to);
                if anchors.is_empty() {
                    self.consumed_anchors.remove(key);
                }
            }
        }
        Ok(())
    }

    fn consume_branch_lifecycle_record(
        &mut self,
        reference: &str,
        kind: BranchLifecycleKind,
    ) -> Result<Option<BranchLifecycleRecord>, GitAiError> {
        let path = self.common_dir().join("logs").join(reference);
        let key = common_key(reference);
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key.clone(), &path, reference, start)?;
        for entry in entries {
            let Some((old_reference, new_reference)) =
                parse_branch_lifecycle_message(kind, &entry.message)
            else {
                continue;
            };
            if new_reference != reference {
                continue;
            }
            self.consume_entry(&entry)?;
            return Ok(Some(BranchLifecycleRecord {
                old_reference,
                oid: entry.new,
            }));
        }
        Ok(None)
    }

    fn sync_common_ref_cursor_to_log_end_after_rewrite(
        &mut self,
        reference: &str,
    ) -> Result<(), GitAiError> {
        let key = common_key(reference);
        let path = self.common_dir().join("logs").join(reference);
        match fs::metadata(&path) {
            Ok(metadata) => {
                let len = metadata.len();
                self.offsets.insert(key.clone(), len);
                self.consumed_offsets.remove(&key);
                self.consumed_anchors.remove(&key);
                if let Some(record) = read_reflog_record_ending_at(&path, len)? {
                    self.anchors.insert(key, ReflogAnchor::from(&record));
                } else {
                    self.anchors.remove(&key);
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.clear_ref_cursor(&key);
                Ok(())
            }
            Err(error) => Err(GitAiError::IoError(error)),
        }
    }

    fn common_ref_log_len(&self, reference: &str) -> Result<Option<u64>, GitAiError> {
        let path = self.common_dir().join("logs").join(reference);
        match fs::metadata(path) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(GitAiError::IoError(error)),
        }
    }

    fn original_head_for_explicit_rebase_branch(
        &mut self,
        branch_ref: &str,
        finished_new: Option<&str>,
    ) -> Result<Option<String>, GitAiError> {
        let path = self.common_dir().join("logs").join(branch_ref);
        let key = common_key(branch_ref);
        let start = self.reflog_start_offset(&key, &path)?;
        let entries = read_reflog_entries(key.clone(), &path, branch_ref, start)?;

        if let Some(finished_new) = finished_new
            && let Some(entry) = entries.iter().rev().find(|entry| {
                entry.new == finished_new
                    && rebase_branch_finish_message_is(&entry.message, branch_ref)
                    && valid_non_zero_oid(&entry.old)
            })
        {
            return Ok(Some(entry.old.clone()));
        }

        let latest_entry_tip = entries
            .iter()
            .rev()
            .find(|entry| valid_non_zero_oid(&entry.new))
            .map(|entry| entry.new.clone());
        Ok(latest_entry_tip.or_else(|| self.ref_tip_at_cursor_start(&key)))
    }

    fn remove_stash_from_stack(&mut self, target_index: Option<usize>, target_oid: &str) {
        if let Some(index) = target_index
            && self
                .stash_stack
                .get(index)
                .is_some_and(|oid| oid == target_oid)
        {
            self.stash_stack.remove(index);
            return;
        }
        if let Some(position) = self.stash_stack.iter().position(|oid| oid == target_oid) {
            self.stash_stack.remove(position);
        }
    }

    fn common_dir(&self) -> PathBuf {
        PathBuf::from(&self.family.0)
    }

    fn head_expected_transition(
        &self,
        cmd: &NormalizedCommand,
        state: &FamilyState,
    ) -> ExpectedTransition {
        let expected = ExpectedTransition::from_state_and_working_logs(cmd, state);
        if self.head_cursor_initialized(cmd.worktree.as_deref()) {
            expected.without_old_oid_constraint()
        } else {
            expected
        }
    }

    fn head_cursor_initialized(&self, worktree: Option<&Path>) -> bool {
        let Some(worktree) = worktree else {
            return false;
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return false;
        };
        self.offsets.contains_key(&head_key(&git_dir))
    }

    fn clear_ref_cursor(&mut self, key: &str) {
        self.offsets.remove(key);
        self.anchors.remove(key);
        self.consumed_offsets.remove(key);
        self.consumed_anchors.remove(key);
    }
}

fn merge_reflog_prefixes(args: &[String]) -> &'static [&'static str] {
    if args.iter().any(|arg| arg == "--continue") {
        &["commit (merge):"]
    } else {
        &["merge"]
    }
}

fn revert_reflog_prefixes(args: &[String]) -> &'static [&'static str] {
    if args.iter().any(|arg| arg == "--continue") {
        // Git commonly records sequencer continuation as a plain `commit:`
        // entry; some versions/modes use the more specific parenthesized form.
        &["commit:", "commit (revert):"]
    } else {
        &["revert:"]
    }
}

pub(crate) fn capture_reflog_start_offsets_for_worktree(worktree: &Path) -> HashMap<String, u64> {
    let mut offsets = HashMap::new();

    if let Some(git_dir) = git_dir_for_worktree(worktree) {
        let path = git_dir.join("logs").join("HEAD");
        if let Ok(metadata) = fs::metadata(&path) {
            offsets.insert(head_key(&git_dir), metadata.len());
        }
    }

    let Some(common_dir) = common_dir_for_worktree(worktree) else {
        return offsets;
    };
    let logs = common_dir.join("logs");
    let mut refs = Vec::new();
    if discover_reflog_refs(&logs, &logs, &mut refs).is_ok() {
        for reference in refs {
            if reference == "HEAD" {
                continue;
            }
            let path = logs.join(&reference);
            if let Ok(metadata) = fs::metadata(&path) {
                offsets.insert(common_key(&reference), metadata.len());
            }
        }
    }
    offsets
}

pub(crate) fn refs_at_reflog_start_offsets(
    family: &FamilyKey,
    offsets: &HashMap<String, u64>,
) -> Result<HashMap<String, String>, GitAiError> {
    let common_dir = PathBuf::from(&family.0);
    let mut refs = HashMap::new();

    for (key, offset) in offsets {
        if *offset == 0 {
            continue;
        }
        let Some((reference, path)) = reflog_reference_and_path_for_key(&common_dir, key) else {
            continue;
        };
        let Some(record) = read_reflog_record_ending_at(&path, *offset)? else {
            continue;
        };
        if valid_non_zero_oid(&record.new) {
            refs.insert(reference, record.new);
        }
    }

    Ok(refs)
}

fn reflog_reference_and_path_for_key(common_dir: &Path, key: &str) -> Option<(String, PathBuf)> {
    if let Some(reference) = key.strip_prefix("common:") {
        return Some((
            reference.to_string(),
            common_dir.join("logs").join(reference),
        ));
    }
    let git_dir = key
        .strip_prefix("worktree:")
        .and_then(|value| value.strip_suffix(":HEAD"))?;
    Some((
        "HEAD".to_string(),
        PathBuf::from(git_dir).join("logs").join("HEAD"),
    ))
}

impl From<&CursorEntry> for ReflogAnchor {
    fn from(entry: &CursorEntry) -> Self {
        Self {
            old: entry.old.clone(),
            new: entry.new.clone(),
            message: entry.message.clone(),
            end_offset: entry.end_offset,
        }
    }
}

impl From<&ReflogRecord> for ReflogAnchor {
    fn from(record: &ReflogRecord) -> Self {
        Self {
            old: record.old.clone(),
            new: record.new.clone(),
            message: record.message.clone(),
            end_offset: record.end_offset,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ExpectedTransition {
    old_oids: HashSet<String>,
    new_oid: Option<String>,
    messages: HashSet<String>,
}

fn current_worktree_branch_ref<'a>(
    cmd: &NormalizedCommand,
    state: &'a FamilyState,
) -> Option<&'a str> {
    let worktree = cmd.worktree.as_ref()?;
    let canonical = worktree.canonicalize().unwrap_or_else(|_| worktree.clone());
    state
        .worktrees
        .get(&canonical)
        .or_else(|| state.worktrees.get(worktree))
        .and_then(|worktree| worktree.branch.as_deref())
}

impl ExpectedTransition {
    fn with_reflog_messages(mut self, messages: HashSet<String>) -> Self {
        self.messages = messages;
        self
    }

    fn without_reflog_message_constraint(mut self) -> Self {
        self.messages.clear();
        self
    }

    fn without_old_oid_constraint(mut self) -> Self {
        self.old_oids.clear();
        self
    }

    fn from_state_and_working_logs(cmd: &NormalizedCommand, state: &FamilyState) -> Self {
        let mut old_oids = HashSet::new();
        if let Some(head) = state
            .refs
            .get("HEAD")
            .filter(|head| valid_non_zero_oid(head))
        {
            old_oids.insert(head.clone());
        }
        for (reference, oid) in &state.refs {
            if reference.starts_with("refs/heads/") && valid_non_zero_oid(oid) {
                old_oids.insert(oid.clone());
            }
        }
        if let Some(worktree) = cmd.worktree.as_ref() {
            old_oids.extend(working_log_base_oids(worktree));
        }
        Self {
            old_oids,
            new_oid: None,
            messages: HashSet::new(),
        }
    }

    fn matches(&self, entry: &CursorEntry) -> bool {
        if !valid_ref_transition(&entry.old, &entry.new) {
            return false;
        }
        if !self.messages.is_empty() && !self.messages.contains(&entry.message) {
            return false;
        }
        if !self.old_oids.is_empty() && !self.old_oids.contains(&entry.old) {
            return false;
        }
        if let Some(new_oid) = self.new_oid.as_ref()
            && &entry.new != new_oid
        {
            return false;
        }
        true
    }

    fn matches_span_boundary(&self, entry: &CursorEntry) -> bool {
        if !valid_ref_transition(&entry.old, &entry.new) {
            return false;
        }
        if !self.messages.is_empty() && !self.messages.contains(&entry.message) {
            return false;
        }
        if !self.old_oids.is_empty()
            && !self.old_oids.contains(&entry.old)
            && !self.old_oids.contains(&entry.new)
        {
            return false;
        }
        if let Some(new_oid) = self.new_oid.as_ref()
            && &entry.new != new_oid
        {
            return false;
        }
        true
    }
}

fn commit_reflog_messages(args: &[String], amend: bool) -> HashSet<String> {
    // Git synthesizes `fixup! <target subject>` / `squash! <target subject>`
    // regardless of a later `-m` body. The target subject is not present in
    // argv, so constraining the reflog to the literal `-m` value would reject
    // the command's real transition. Prefix + OID/offset matching remains.
    if args.iter().any(|arg| {
        matches!(arg.as_str(), "--fixup" | "--squash")
            || arg.starts_with("--fixup=")
            || arg.starts_with("--squash=")
    }) {
        return HashSet::new();
    }
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

fn reset_reflog_messages(args: &[String]) -> HashSet<String> {
    let Some(target) = reset_target_arg(args) else {
        return HashSet::new();
    };
    [format!("reset: moving to {target}")].into_iter().collect()
}

fn reset_target_arg(args: &[String]) -> Option<String> {
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

fn commit_subject_from_args(args: &[String]) -> Option<String> {
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

fn commit_subject(message: &str) -> Option<String> {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim_end_matches(|character: char| character.is_ascii_whitespace())
                .to_string()
        })
}

fn resolve_cherry_pick_source_oids_from_sources(
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

fn cherry_pick_source_args(args: &[String]) -> Vec<&str> {
    let args = if args.first().is_some_and(|arg| arg == "cherry-pick") {
        &args[1..]
    } else {
        args
    };
    let mut sources = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            sources.extend(args[idx + 1..].iter().map(String::as_str));
            break;
        }
        if matches!(arg, "--abort" | "--continue" | "--quit" | "--skip") {
            return Vec::new();
        }
        if matches!(
            arg,
            "-m" | "--mainline" | "-X" | "--strategy-option" | "--strategy"
        ) {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with("--mainline=")
            || arg.starts_with("--strategy=")
            || arg.starts_with("--strategy-option=")
            || arg == "--gpg-sign"
            || arg.starts_with("--gpg-sign=")
            || arg.starts_with("-m")
            || arg.starts_with("-X")
            || arg.starts_with("-S")
        {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if !arg.is_empty() {
            sources.push(arg);
        }
        idx += 1;
    }
    sources
}

fn revert_source_args(args: &[String]) -> Vec<&str> {
    let args = if args.first().is_some_and(|arg| arg == "revert") {
        &args[1..]
    } else {
        args
    };
    let mut sources = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            sources.extend(args[idx + 1..].iter().map(String::as_str));
            break;
        }
        if matches!(arg, "--abort" | "--continue" | "--quit" | "--skip") {
            return Vec::new();
        }
        if matches!(arg, "-m" | "--mainline") {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with("--mainline=")
            || arg == "--gpg-sign"
            || arg.starts_with("--gpg-sign=")
            || arg.starts_with("-S")
        {
            idx += 1;
            continue;
        }
        if matches!(arg, "-n" | "--no-commit" | "--no-edit" | "-e" | "--edit") {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if !arg.is_empty() {
            sources.push(arg);
        }
        idx += 1;
    }
    sources
}

fn cherry_pick_source_is_range(source: &str) -> bool {
    source.contains("..")
}

fn concretize_revision_expr(expr: &str, refs: &HashMap<String, String>) -> Option<String> {
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

fn resolve_cherry_pick_source_from_state(
    source: &str,
    refs: &HashMap<String, String>,
) -> Option<String> {
    if cherry_pick_source_is_range(source) {
        return None;
    }

    concretize_revision_expr(source, refs).filter(|oid| valid_non_zero_oid(oid))
}

fn split_revision_suffix(expr: &str) -> (&str, &str) {
    let idx = expr
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '~' | '^').then_some(idx))
        .unwrap_or(expr.len());
    expr.split_at(idx)
}

fn resolve_ref_from_state(name: &str, refs: &HashMap<String, String>) -> Option<String> {
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

fn is_hex_oid_prefix(value: &str) -> bool {
    (4..=64).contains(&value.len()) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn pull_reflog_action(cmd: &NormalizedCommand) -> String {
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

fn pull_command_args(args: &[String]) -> &[String] {
    if args.first().is_some_and(|arg| arg == "pull") {
        &args[1..]
    } else {
        args
    }
}

fn pull_reflog_message_prefixes(action: &str) -> Vec<String> {
    if action == "pull" {
        return vec!["pull:".to_string(), "pull (".to_string()];
    }
    vec![format!("{}:", action), format!("{} ", action)]
}

fn pull_reflog_action_state<'a>(message: &'a str, action: &str) -> Option<&'a str> {
    let rest = message.strip_prefix(action)?;
    let open = rest.find('(')?;
    let after_open = &rest[open + 1..];
    let close = after_open.find("):")?;
    Some(&after_open[..close])
}

fn pull_reflog_action_is(message: &str, action: &str, expected: &str) -> bool {
    pull_reflog_action_state(message, action).is_some_and(|state| state == expected)
}

fn pull_reflog_action_starts_new_command(message: &str, action: &str) -> bool {
    matches!(
        pull_reflog_action_state(message, action),
        Some("start" | "continue" | "skip" | "abort" | "quit" | "finish")
    )
}

fn clamp_seed_to_entry_containing_offset(
    entries: &[CursorEntry],
    offset: u64,
    message_prefixes: &[&str],
) -> Option<u64> {
    // A matching row after the offset means the offset is a real command-start
    // boundary before the current command's reflog write. Do not rewind into
    // older history in that case.
    if entries.iter().any(|entry| {
        entry.start_offset >= offset && message_matches(&entry.message, message_prefixes)
    }) {
        return None;
    }

    // If the asynchronous offset landed at EOF or in the middle of the command's
    // own branch-finish row, seed from the row start so the parser never begins
    // inside a reflog record.
    entries
        .iter()
        .rev()
        .find(|entry| {
            entry.start_offset < offset
                && offset <= entry.end_offset
                && message_matches(&entry.message, message_prefixes)
        })
        .map(|entry| entry.start_offset)
}

fn head_span_start_near_offset(
    entries: &[CursorEntry],
    offset: u64,
    message_prefixes: &[&str],
    expected: ExpectedTransition,
    limit: usize,
) -> Option<u64> {
    let mut contiguous = VecDeque::<&CursorEntry>::new();
    let mut latest_before_offset: Option<u64> = None;

    for entry in entries {
        if !message_matches(&entry.message, message_prefixes) {
            contiguous.clear();
            continue;
        }
        if contiguous
            .back()
            .is_some_and(|previous| previous.new != entry.old)
        {
            contiguous.clear();
        }
        contiguous.push_back(entry);
        while contiguous.len() > limit {
            contiguous.pop_front();
        }
        if !expected.matches(entry) {
            continue;
        }
        let Some(candidate) = contiguous.front() else {
            continue;
        };
        if candidate.start_offset >= offset {
            return None;
        }
        latest_before_offset = Some(candidate.start_offset);
    }

    latest_before_offset
}

fn pull_span_start_containing_offset(
    entries: &[CursorEntry],
    offset: u64,
    action: &str,
    expected: ExpectedTransition,
) -> Option<u64> {
    // If a rebase/pull start row exists after the offset, the offset is a true
    // boundary before the current span. Keep it as-is.
    if entries.iter().any(|entry| {
        entry.start_offset >= offset && pull_reflog_action_is(&entry.message, action, "start")
    }) {
        return None;
    }

    entries
        .iter()
        .enumerate()
        .rev()
        .find(|(idx, entry)| {
            entry.start_offset < offset
                && pull_reflog_action_is(&entry.message, action, "start")
                && expected.matches_span_boundary(entry)
                && pull_span_covers_offset(entries, *idx, offset, action)
        })
        .map(|(_, entry)| entry.start_offset)
}

fn pull_span_covers_offset(
    entries: &[CursorEntry],
    start_idx: usize,
    offset: u64,
    action: &str,
) -> bool {
    for entry in entries.iter().skip(start_idx + 1) {
        if entry.start_offset >= offset {
            return true;
        }
        if pull_reflog_action_starts_new_command(&entry.message, action) {
            return offset <= entry.end_offset;
        }
    }
    true
}

fn rebase_reflog_action(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("rebase")?;
    let open = rest.find('(')?;
    let after_open = &rest[open + 1..];
    let close = after_open.find("):")?;
    Some(&after_open[..close])
}

fn rebase_reflog_action_is(message: &str, expected: &str) -> bool {
    rebase_reflog_action(message).is_some_and(|action| action == expected)
}

fn rebase_reflog_action_starts_new_command(message: &str) -> bool {
    matches!(
        rebase_reflog_action(message),
        Some("start" | "continue" | "skip" | "abort" | "quit" | "finish")
    )
}

fn rebase_span_start_containing_offset(
    entries: &[CursorEntry],
    offset: u64,
    expected: ExpectedTransition,
) -> Option<u64> {
    // If a rebase start row exists after the offset, the offset is a true
    // boundary before the current span. Keep it as-is.
    if entries.iter().any(|entry| {
        entry.start_offset >= offset && rebase_reflog_action_is(&entry.message, "start")
    }) {
        return None;
    }

    entries
        .iter()
        .enumerate()
        .rev()
        .find(|(idx, entry)| {
            entry.start_offset < offset
                && rebase_reflog_action_is(&entry.message, "start")
                && expected.matches_span_boundary(entry)
                && rebase_span_covers_offset(entries, *idx, offset)
        })
        .map(|(_, entry)| entry.start_offset)
}

fn rebase_span_covers_offset(entries: &[CursorEntry], start_idx: usize, offset: u64) -> bool {
    for entry in entries.iter().skip(start_idx + 1) {
        if entry.start_offset >= offset {
            return true;
        }
        if rebase_reflog_action_starts_new_command(&entry.message) {
            return offset <= entry.end_offset;
        }
    }
    true
}

fn rebase_start_checkout_target_from_args(args: &[String]) -> Option<String> {
    let summary = summarize_rebase_args(args);
    if summary.is_control_mode {
        return None;
    }
    summary
        .onto_spec
        .or_else(|| summary.positionals.first().cloned())
}

fn rebase_start_message_targets(message: &str, target: &str) -> bool {
    message
        .strip_prefix("rebase (start): checkout ")
        .is_some_and(|message_target| message_target == target)
}

fn rebase_finish_returns_to_branch(message: &str, branch_ref: &str) -> bool {
    message == format!("rebase (finish): returning to {}", branch_ref)
}

fn rebase_finish_returned_branch(message: &str) -> Option<&str> {
    message.strip_prefix("rebase (finish): returning to ")
}

fn rebase_branch_finish_message_is(message: &str, branch_ref: &str) -> bool {
    message.starts_with(&format!("rebase (finish): {}", branch_ref))
}

fn pull_finish_returned_branch(message: &str, action: &str) -> Option<String> {
    message
        .strip_prefix(&format!("{action} (finish): returning to "))
        .map(ToOwned::to_owned)
}

fn latest_rebase_finish_for_branch<'a>(
    entries: &'a [CursorEntry],
    branch_ref: &str,
) -> Option<&'a CursorEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| rebase_finish_returns_to_branch(&entry.message, branch_ref))
}

fn rebase_start_marker_for_explicit_branch<'a>(
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

fn read_reflog_entries(
    key: String,
    path: &Path,
    reference: &str,
    start_offset: Option<u64>,
) -> Result<Vec<CursorEntry>, GitAiError> {
    let records = read_reflog_records(path, start_offset)?;
    Ok(records
        .into_iter()
        .filter(|record| record.old != record.new)
        .map(|record| CursorEntry {
            key: key.clone(),
            path: path.to_path_buf(),
            reference: reference.to_string(),
            old: record.old,
            new: record.new,
            message: record.message,
            timestamp_secs: record.timestamp_secs,
            start_offset: record.start_offset,
            end_offset: record.end_offset,
        })
        .collect())
}

fn read_reflog_entries_including_noops(
    key: String,
    path: &Path,
    reference: &str,
    start_offset: Option<u64>,
) -> Result<Vec<CursorEntry>, GitAiError> {
    let records = read_reflog_records(path, start_offset)?;
    Ok(records
        .into_iter()
        .map(|record| CursorEntry {
            key: key.clone(),
            path: path.to_path_buf(),
            reference: reference.to_string(),
            old: record.old,
            new: record.new,
            message: record.message,
            timestamp_secs: record.timestamp_secs,
            start_offset: record.start_offset,
            end_offset: record.end_offset,
        })
        .collect())
}

fn read_reflog_records(
    path: &Path,
    start_offset: Option<u64>,
) -> Result<Vec<ReflogRecord>, GitAiError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(GitAiError::IoError(error)),
    };
    let byte_len = file.metadata().map_err(GitAiError::IoError)?.len();
    let start = match start_offset {
        Some(offset) if offset > byte_len => 0,
        Some(offset) => offset,
        None => 0,
    };
    file.seek(SeekFrom::Start(start))
        .map_err(GitAiError::IoError)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(GitAiError::IoError)?;

    let mut entries = Vec::new();
    let mut offset = start;
    for raw_line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let line_start = offset;
        offset = offset.saturating_add(raw_line.len() as u64);
        if !raw_line.ends_with(b"\n") {
            continue;
        }
        let line = String::from_utf8_lossy(raw_line);
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(entry) = parse_reflog_line(line, line_start, offset) else {
            continue;
        };
        if entry.end_offset > line_start {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn read_reflog_record_ending_at(
    path: &Path,
    end_offset: u64,
) -> Result<Option<ReflogRecord>, GitAiError> {
    if end_offset == 0 {
        return Ok(None);
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(GitAiError::IoError(error)),
    };
    let byte_len = file.metadata().map_err(GitAiError::IoError)?.len();
    if end_offset > byte_len {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(end_offset.saturating_sub(1)))
        .map_err(GitAiError::IoError)?;
    let mut terminator = [0; 1];
    file.read_exact(&mut terminator)
        .map_err(GitAiError::IoError)?;
    if terminator[0] != b'\n' {
        return Ok(None);
    }

    let mut cursor = end_offset;
    let mut suffix = Vec::new();
    loop {
        let chunk_start = cursor.saturating_sub(8192);
        let chunk_len = (cursor - chunk_start) as usize;
        let mut chunk = vec![0; chunk_len];
        file.seek(SeekFrom::Start(chunk_start))
            .map_err(GitAiError::IoError)?;
        file.read_exact(&mut chunk).map_err(GitAiError::IoError)?;

        let search_end = if cursor == end_offset && chunk.last().is_some_and(|byte| *byte == b'\n')
        {
            chunk.len().saturating_sub(1)
        } else {
            chunk.len()
        };
        if let Some(index) = chunk[..search_end].iter().rposition(|byte| *byte == b'\n') {
            let line_start = chunk_start + index as u64 + 1;
            let mut line = chunk[index + 1..].to_vec();
            line.extend_from_slice(&suffix);
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            return Ok(parse_reflog_line(line, line_start, end_offset)
                .filter(|record| record.end_offset > line_start));
        }

        let mut line = chunk;
        line.extend_from_slice(&suffix);
        suffix = line;
        if chunk_start == 0 {
            let line = String::from_utf8_lossy(&suffix);
            let line = line.trim_end_matches(['\r', '\n']);
            return Ok(
                parse_reflog_line(line, 0, end_offset).filter(|record| record.end_offset > 0)
            );
        }
        cursor = chunk_start;
    }
}

fn parse_reflog_line(line: &str, start_offset: u64, end_offset: u64) -> Option<ReflogRecord> {
    let (head, message) = line.split_once('\t').unwrap_or((line, ""));
    let mut parts = head.split_whitespace();
    let old = parts.next()?.trim();
    let new = parts.next()?.trim();
    if !is_valid_git_oid(old) || !is_valid_git_oid(new) {
        return None;
    }
    Some(ReflogRecord {
        old: old.to_string(),
        new: new.to_string(),
        message: message.to_string(),
        timestamp_secs: parse_reflog_timestamp_secs(head),
        start_offset,
        end_offset,
    })
}

fn parse_reflog_timestamp_secs(head: &str) -> Option<i64> {
    let mut parts = head.split_whitespace().rev();
    let _timezone = parts.next()?;
    parts.next()?.parse().ok()
}

fn discover_reflog_refs(
    root: &Path,
    current: &Path,
    out: &mut Vec<String>,
) -> Result<(), GitAiError> {
    if !current.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            discover_reflog_refs(root, &path, out)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let reference = relative.to_string_lossy().replace('\\', "/");
        if reference == "ORIG_HEAD" || reference.starts_with("refs/") {
            out.push(reference);
        }
    }
    Ok(())
}

fn parse_update_ref_spec(args: &[String]) -> Result<Option<UpdateRefSpec>, GitAiError> {
    let mut positionals = Vec::new();
    let mut delete = false;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "update-ref" => {
                idx += 1;
            }
            "--stdin" | "--batch-updates" => {
                return Ok(None);
            }
            "-d" | "--delete" => {
                delete = true;
                idx += 1;
            }
            "-m" | "--message" => {
                if idx + 1 >= args.len() {
                    return Err(GitAiError::Generic(
                        "update-ref -m requires a message argument".to_string(),
                    ));
                }
                idx += 2;
            }
            "--create-reflog" | "--no-deref" => {
                idx += 1;
            }
            value if value.starts_with("--message=") => {
                idx += 1;
            }
            value if value.starts_with('-') => {
                return Err(GitAiError::Generic(format!(
                    "trace2 cursor does not support update-ref option '{}'",
                    value
                )));
            }
            value => {
                positionals.push(value.to_string());
                idx += 1;
            }
        }
    }

    if delete {
        return match positionals.as_slice() {
            [reference] => Ok(Some(UpdateRefSpec {
                reference: reference.to_string(),
                new_oid: zero_oid(),
                old_oid: None,
            })),
            [reference, old_oid] => Ok(Some(UpdateRefSpec {
                reference: reference.to_string(),
                new_oid: zero_oid(),
                old_oid: Some(old_oid.to_string()),
            })),
            _ => Err(GitAiError::Generic(
                "update-ref delete requires <ref> [<old-oid>]".to_string(),
            )),
        };
    }

    match positionals.as_slice() {
        [reference, new_oid] => Ok(Some(UpdateRefSpec {
            reference: reference.to_string(),
            new_oid: new_oid.to_string(),
            old_oid: None,
        })),
        [reference, new_oid, old_oid] => Ok(Some(UpdateRefSpec {
            reference: reference.to_string(),
            new_oid: new_oid.to_string(),
            old_oid: Some(old_oid.to_string()),
        })),
        _ => Err(GitAiError::Generic(
            "update-ref requires <ref> <new-oid> [<old-oid>]".to_string(),
        )),
    }
}

fn parse_branch_command_spec(args: &[String]) -> BranchCommandSpec {
    let args = branch_command_args(args);
    let mut delete = false;
    let mut remote_delete = false;
    let mut rename = false;
    let mut copy = false;
    let mut list_only = false;
    let mut config_only = false;
    let mut positionals = Vec::new();
    let mut idx = 0usize;

    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            positionals.extend(args[idx + 1..].iter().cloned());
            break;
        }

        match arg.as_str() {
            "-d" | "-D" | "--delete" => {
                delete = true;
                idx += 1;
            }
            "-m" | "-M" | "--move" => {
                rename = true;
                idx += 1;
            }
            "-c" | "-C" | "--copy" => {
                copy = true;
                idx += 1;
            }
            "-r" | "--remotes" => {
                remote_delete = true;
                list_only = true;
                idx += 1;
            }
            "-a" | "--all" | "--list" | "--show-current" | "--contains" | "--no-contains"
            | "--merged" | "--no-merged" => {
                list_only = true;
                idx += 1;
            }
            "--unset-upstream" | "--edit-description" | "--set-upstream" => {
                config_only = true;
                idx += 1;
            }
            "-u" | "--set-upstream-to" => {
                config_only = true;
                idx = idx.saturating_add(2);
            }
            "--points-at" | "--sort" | "--format" => {
                list_only = true;
                idx = idx.saturating_add(2);
            }
            "--color" | "--column" | "--abbrev" => {
                idx = idx.saturating_add(2);
            }
            "--track"
            | "--no-track"
            | "--create-reflog"
            | "--no-create-reflog"
            | "--recurse-submodules"
            | "--no-color"
            | "--no-column"
            | "--no-abbrev"
            | "--quiet"
            | "-q"
            | "--verbose"
            | "-v"
            | "-vv"
            | "-f"
            | "--force"
            | "-l" => {
                idx += 1;
            }
            value if value.starts_with("--set-upstream-to=") => {
                config_only = true;
                idx += 1;
            }
            value
                if value.starts_with("--points-at=")
                    || value.starts_with("--sort=")
                    || value.starts_with("--format=")
                    || value.starts_with("--contains=")
                    || value.starts_with("--no-contains=")
                    || value.starts_with("--merged=")
                    || value.starts_with("--no-merged=") =>
            {
                list_only = true;
                idx += 1;
            }
            value
                if value.starts_with("--track=")
                    || value.starts_with("--color=")
                    || value.starts_with("--column=")
                    || value.starts_with("--abbrev=") =>
            {
                idx += 1;
            }
            value if value.starts_with("--") => {
                idx += 1;
            }
            value if value.starts_with('-') => {
                apply_branch_short_options(
                    value,
                    &mut delete,
                    &mut remote_delete,
                    &mut rename,
                    &mut copy,
                    &mut list_only,
                );
                idx += branch_short_option_value_width(value);
            }
            value => {
                positionals.push(value.to_string());
                idx += 1;
            }
        }
    }

    if delete {
        let references = positionals
            .into_iter()
            .filter_map(|name| branch_ref_name(&name, remote_delete))
            .collect::<Vec<_>>();
        return if references.is_empty() {
            BranchCommandSpec::None
        } else {
            BranchCommandSpec::Delete { references }
        };
    }

    if rename {
        return match positionals.as_slice() {
            [new_name] => branch_ref_name(new_name, false)
                .map(|new_reference| BranchCommandSpec::Rename {
                    old_reference: None,
                    new_reference,
                })
                .unwrap_or(BranchCommandSpec::None),
            [old_name, new_name] => {
                match (
                    branch_ref_name(old_name, false),
                    branch_ref_name(new_name, false),
                ) {
                    (Some(old_reference), Some(new_reference)) => BranchCommandSpec::Rename {
                        old_reference: Some(old_reference),
                        new_reference,
                    },
                    _ => BranchCommandSpec::None,
                }
            }
            _ => BranchCommandSpec::None,
        };
    }

    if copy {
        return match positionals.as_slice() {
            [new_name] => branch_ref_name(new_name, false)
                .map(|new_reference| BranchCommandSpec::Copy {
                    old_reference: None,
                    new_reference,
                })
                .unwrap_or(BranchCommandSpec::None),
            [old_name, new_name] => {
                match (
                    branch_ref_name(old_name, false),
                    branch_ref_name(new_name, false),
                ) {
                    (Some(old_reference), Some(new_reference)) => BranchCommandSpec::Copy {
                        old_reference: Some(old_reference),
                        new_reference,
                    },
                    _ => BranchCommandSpec::None,
                }
            }
            _ => BranchCommandSpec::None,
        };
    }

    if config_only || list_only {
        return BranchCommandSpec::None;
    }

    positionals
        .first()
        .and_then(|name| branch_ref_name(name, false))
        .map(|reference| BranchCommandSpec::CreateOrReset { reference })
        .unwrap_or(BranchCommandSpec::None)
}

fn branch_command_args(args: &[String]) -> &[String] {
    if args.first().is_some_and(|arg| arg == "branch") {
        &args[1..]
    } else {
        args
    }
}

fn apply_branch_short_options(
    value: &str,
    delete: &mut bool,
    remote_delete: &mut bool,
    rename: &mut bool,
    copy: &mut bool,
    list_only: &mut bool,
) {
    for flag in value.trim_start_matches('-').chars() {
        match flag {
            'd' | 'D' => *delete = true,
            'r' => {
                *remote_delete = true;
                *list_only = true;
            }
            'm' | 'M' => *rename = true,
            'c' | 'C' => *copy = true,
            'a' => *list_only = true,
            _ => {}
        }
    }
}

fn branch_short_option_value_width(value: &str) -> usize {
    if value == "-u" { 2 } else { 1 }
}

fn branch_ref_name(name: &str, remote: bool) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "--" || trimmed.starts_with('-') {
        return None;
    }
    if trimmed.starts_with("refs/heads/") || trimmed.starts_with("refs/remotes/") {
        return Some(trimmed.to_string());
    }
    if trimmed.starts_with("refs/") {
        return None;
    }
    if remote {
        Some(format!("refs/remotes/{}", trimmed))
    } else {
        Some(format!("refs/heads/{}", trimmed))
    }
}

fn parse_branch_lifecycle_message(
    kind: BranchLifecycleKind,
    message: &str,
) -> Option<(String, String)> {
    let prefix = match kind {
        BranchLifecycleKind::Rename => "Branch: renamed ",
        BranchLifecycleKind::Copy => "Branch: copied ",
    };
    let rest = message.strip_prefix(prefix)?;
    let (old_reference, new_reference) = rest.split_once(" to ")?;
    Some((old_reference.to_string(), new_reference.to_string()))
}

fn working_log_base_oids(worktree: &Path) -> HashSet<String> {
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

fn checkout_is_path_checkout(cmd: &NormalizedCommand) -> bool {
    let args = command_args(cmd);
    args.iter().any(|arg| arg == "--")
        || args
            .iter()
            .any(|arg| arg.starts_with("--pathspec") || arg == "--ours" || arg == "--theirs")
}

fn checkout_or_switch_is_orphan(cmd: &NormalizedCommand) -> bool {
    matches!(cmd.primary_command.as_deref(), Some("checkout" | "switch"))
        && command_args(cmd).iter().any(|arg| arg == "--orphan")
}

fn enrich_orphan_head_transition(
    cmd: &mut NormalizedCommand,
    state: &FamilyState,
    command_start_refs: &HashMap<String, String>,
) {
    let worktree_head = cmd.worktree.as_ref().and_then(|worktree| {
        let canonical = worktree.canonicalize().unwrap_or_else(|_| worktree.clone());
        state
            .worktrees
            .get(&canonical)
            .or_else(|| state.worktrees.get(worktree))
            .and_then(|worktree| worktree.head.clone())
    });
    let old = command_start_refs
        .get("HEAD")
        .or_else(|| state.refs.get("HEAD"))
        .cloned()
        .or(worktree_head)
        .or_else(|| {
            let bases = cmd
                .worktree
                .as_deref()
                .map(working_log_base_oids)
                .unwrap_or_default()
                .into_iter()
                .filter(|oid| valid_non_zero_oid(oid))
                .collect::<Vec<_>>();
            (bases.len() == 1).then(|| bases[0].clone())
        });
    if let Some(old) = old.filter(|oid| valid_non_zero_oid(oid)) {
        cmd.ref_changes.push(RefChange {
            reference: "HEAD".to_string(),
            old,
            new: zero_oid(),
        });
    }
}

fn stash_command_args(args: &[String]) -> &[String] {
    if args.first().is_some_and(|arg| arg == "stash") {
        &args[1..]
    } else {
        args
    }
}

fn stash_target_arg<'a>(args: &'a [String], kind: &str) -> Option<&'a String> {
    let skip = if kind == "branch" { 2 } else { 1 };
    args.iter().skip(skip).find(|arg| !arg.starts_with('-'))
}

fn stash_store_target_arg(args: &[String]) -> Option<&String> {
    let mut index = 1;
    let mut target = None;
    while index < args.len() {
        match args[index].as_str() {
            "-m" | "--message" => index += 2,
            value if value.starts_with("--message=") => index += 1,
            value if value.starts_with('-') => index += 1,
            _ => {
                target = args.get(index);
                index += 1;
            }
        }
    }
    target
}

fn stash_push_message_from_args(args: &[String], kind: &str) -> Option<String> {
    if kind == "save" {
        let message = args
            .iter()
            .skip(1)
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        return (!message.is_empty()).then_some(message);
    }

    let mut idx = if kind == "push" { 1 } else { 0 };
    while idx < args.len() {
        match args[idx].as_str() {
            "-m" | "--message" => {
                return args.get(idx + 1).filter(|value| !value.is_empty()).cloned();
            }
            value if value.starts_with("--message=") => {
                return value
                    .strip_prefix("--message=")
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned);
            }
            _ => idx += 1,
        }
    }
    None
}

fn stash_reflog_message_matches(reflog_message: &str, stash_message: &str) -> bool {
    reflog_message == stash_message
        || reflog_message
            .strip_suffix(stash_message)
            .is_some_and(|prefix| prefix.ends_with(": "))
}

fn stash_target_index(target: Option<&String>) -> Option<usize> {
    let target = target.map(String::as_str).unwrap_or("stash@{0}");
    if matches!(target, "stash" | "refs/stash") {
        return Some(0);
    }
    target
        .strip_prefix("stash@{")
        .and_then(|value| value.strip_suffix('}'))
        .and_then(|value| value.parse::<usize>().ok())
}

#[cfg(test)]
mod stash_arg_tests {
    use super::{stash_store_target_arg, stash_target_arg};

    #[test]
    fn target_skips_restore_options() {
        let apply = ["apply", "--index", "--quiet", "stash@{2}"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            stash_target_arg(&apply, "apply").map(String::as_str),
            Some("stash@{2}")
        );

        let branch = ["branch", "recovered", "stash@{1}"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            stash_target_arg(&branch, "branch").map(String::as_str),
            Some("stash@{1}")
        );

        let store = [
            "store",
            "-m",
            "message",
            "0123456789012345678901234567890123456789",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            stash_store_target_arg(&store).map(String::as_str),
            Some("0123456789012345678901234567890123456789")
        );
    }
}

fn rebase_command_args(cmd: &NormalizedCommand) -> Vec<String> {
    let args = command_args(cmd);
    if args.first().is_some_and(|arg| arg == "rebase") {
        args[1..].to_vec()
    } else {
        args
    }
}

fn command_uses_ref_cursor(primary: &str) -> bool {
    matches!(
        primary,
        "am" | "commit"
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
            | "fast-import"
            | "filter-branch"
            | "filter-repo"
            | "symbolic-ref"
    )
}

fn command_can_move_refs_on_nonzero(primary: Option<&str>) -> bool {
    matches!(
        primary,
        Some(
            "am" | "checkout"
                | "switch"
                | "stash"
                | "rebase"
                | "pull"
                | "branch"
                | "cherry-pick"
                | "revert"
                | "fast-import"
                | "filter-branch"
                | "filter-repo"
        )
    )
}

fn command_can_clamp_non_authoritative_cold_seed(cmd: &NormalizedCommand) -> bool {
    matches!(cmd.primary_command.as_deref(), Some("am" | "cherry-pick"))
}

fn message_matches(message: &str, prefixes: &[&str]) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|prefix| message.starts_with(prefix))
}

fn valid_ref_transition(old: &str, new: &str) -> bool {
    is_valid_git_oid(old) && is_valid_git_oid(new) && old != new
}

fn valid_non_zero_oid(value: &str) -> bool {
    is_valid_git_oid(value) && !value.chars().all(|ch| ch == '0')
}

fn zero_oid() -> String {
    "0000000000000000000000000000000000000000".to_string()
}

fn direct_update_ref_change_from_argv(spec: &UpdateRefSpec) -> Option<RefChange> {
    let old = spec.old_oid.as_ref()?;
    if !is_valid_git_oid(old) || !is_valid_git_oid(&spec.new_oid) {
        return None;
    }
    Some(RefChange {
        reference: spec.reference.clone(),
        old: old.clone(),
        new: spec.new_oid.clone(),
    })
}

#[derive(Debug, Clone, Copy)]
struct ReflogTimestampWindow {
    start_secs: i64,
    end_secs: i64,
}

impl ReflogTimestampWindow {
    fn contains(self, timestamp_secs: i64) -> bool {
        timestamp_secs >= self.start_secs && timestamp_secs <= self.end_secs
    }
}

fn reflog_timestamp_window(cmd: &NormalizedCommand) -> ReflogTimestampWindow {
    let start = unix_nanos_to_reflog_secs(cmd.started_at_ns).saturating_sub(1);
    let end = unix_nanos_to_reflog_secs(cmd.finished_at_ns).saturating_add(1);
    ReflogTimestampWindow {
        start_secs: start.min(end),
        end_secs: start.max(end),
    }
}

fn unix_nanos_to_reflog_secs(value: u128) -> i64 {
    i64::try_from(value / 1_000_000_000).unwrap_or(i64::MAX)
}

fn entry_to_ref_change(entry: &CursorEntry) -> RefChange {
    RefChange {
        reference: entry.reference.clone(),
        old: entry.old.clone(),
        new: entry.new.clone(),
    }
}

fn dedup_ref_changes(changes: &mut Vec<RefChange>) {
    let mut seen = HashSet::new();
    changes.retain(|change| {
        seen.insert((
            change.reference.clone(),
            change.old.clone(),
            change.new.clone(),
        ))
    });
}

fn common_key(reference: &str) -> String {
    format!("common:{}", reference)
}

fn branch_arg_to_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{}", branch)
    }
}

fn head_key(git_dir: &Path) -> String {
    let normalized = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    format!("worktree:{}:HEAD", normalized)
}

fn sequenced_head_before_command(worktree: &Path, state: &FamilyState) -> Option<String> {
    let worktree = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    state
        .worktrees
        .get(&worktree)
        .and_then(|worktree| worktree.head.clone())
        .or_else(|| state.refs.get("HEAD").cloned())
        .filter(|oid| valid_non_zero_oid(oid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::domain::{
        CommandScope, Confidence, FamilyKey, FamilyState, NormalizedCommand, WatermarkState,
        WorktreeState,
    };
    use std::collections::HashMap;
    use std::fs;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const C: &str = "3333333333333333333333333333333333333333";
    const D: &str = "4444444444444444444444444444444444444444";
    const E: &str = "5555555555555555555555555555555555555555";
    const F: &str = "6666666666666666666666666666666666666666";
    const G: &str = "7777777777777777777777777777777777777777";

    fn create_git_dir(worktree: &Path) -> PathBuf {
        let git_dir = worktree.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        git_dir
    }

    #[test]
    fn commit_subject_matches_git_reflog_trailing_whitespace_cleanup() {
        assert_eq!(commit_subject("subject \t"), Some("subject".to_string()));
        assert_eq!(
            commit_subject("\nsubject \t\nbody"),
            Some("subject".to_string())
        );
        assert_eq!(
            commit_subject("subject\u{00a0}"),
            Some("subject\u{00a0}".to_string())
        );

        let args = vec![
            "commit".to_string(),
            "-m".to_string(),
            "subject \t".to_string(),
        ];
        assert!(commit_reflog_messages(&args, false).contains("commit: subject"));
        assert!(commit_reflog_messages(&args, true).contains("commit (amend): subject"));
    }

    #[test]
    fn fixup_and_squash_do_not_constrain_reflog_to_message_body() {
        for args in [
            vec!["commit", "--fixup=HEAD~1", "-m", "ignored body"],
            vec!["commit", "--squash", "HEAD~1", "-m", "ignored body"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(commit_reflog_messages(&args, false).is_empty());
        }
    }

    #[test]
    fn revert_source_args_do_not_treat_bare_gpg_sign_as_value_option() {
        assert_eq!(
            revert_source_args(&["--gpg-sign".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args(&["-S".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args(&["-Smy-key".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
    }

    #[test]
    fn cherry_pick_source_args_do_not_treat_bare_gpg_sign_as_value_option() {
        assert_eq!(
            cherry_pick_source_args(&["--gpg-sign".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args(&["-S".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args(&["-Smy-key".to_string(), "HEAD~1".to_string()]),
            vec!["HEAD~1"]
        );
    }

    fn family_state(family: &FamilyKey) -> FamilyState {
        FamilyState {
            family_key: family.clone(),
            refs: HashMap::new(),
            worktrees: HashMap::new(),
            last_error: None,
            applied_seq: 0,
            watermarks: WatermarkState::default(),
        }
    }

    fn command(family: &FamilyKey, args: &[&str]) -> NormalizedCommand {
        command_with_worktree(family, None, args)
    }

    fn command_with_worktree(
        family: &FamilyKey,
        worktree: Option<PathBuf>,
        args: &[&str],
    ) -> NormalizedCommand {
        NormalizedCommand {
            scope: CommandScope::Family(family.clone()),
            family_key: Some(family.clone()),
            worktree,
            root_sid: "sid".to_string(),
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
            index_snapshot_at_start: None,
            workspace_paths_at_start: Vec::new(),
            stash_target_oid: None,
            cherry_pick_source_oids: Vec::new(),
            revert_source_oids: Vec::new(),
            ref_changes: Vec::new(),
            confidence: Confidence::Low,
        }
    }

    #[test]
    fn orphan_checkout_and_switch_emit_head_to_unborn_transition_only_on_success() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let family = FamilyKey::new(worktree.join(".git").to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), A.to_string());
        let mut cursor = RefCursor::new(family.clone());

        for args in [
            &["checkout", "--orphan", "new-root"][..],
            &["switch", "--orphan", "new-root"][..],
        ] {
            let mut command = command_with_worktree(&family, Some(worktree.clone()), args);
            cursor.enrich_command(&mut command, &state).unwrap();
            assert_eq!(
                command.ref_changes,
                vec![RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: zero_oid(),
                }]
            );
        }

        let mut failed = command_with_worktree(
            &family,
            Some(worktree),
            &["checkout", "--orphan", "new-root"],
        );
        failed.exit_code = 128;
        cursor.enrich_command(&mut failed, &state).unwrap();
        assert!(failed.ref_changes.is_empty());
    }

    #[test]
    fn cold_start_late_ingress_offset_does_not_skip_commit_on_uninitialized_head_cursor() {
        // Regression for the concurrent-burst / rebase-patch-stack flake. Unlike
        // the initialized-cursor case below, here the worktree HEAD cursor is
        // COLD (first traced commit on a fresh linked worktree — the worktree's
        // own HEAD reflog was never seeded by a prior command in this family).
        //
        // The async ingress offset is captured LATE: after git appended this
        // commit's HEAD entry, with a trailing entry after it (so the offset is
        // NOT at EOF). Cold-start seeding used `command_start_offset_is_authoritative`
        // -> records-exist-after-offset == true -> seed the cursor at the late
        // offset, positioning it PAST the commit's own entry. find_entry_in_log
        // then reads from the late cursor, never sees the commit -> empty
        // ref_changes -> head_change None -> no CommitCreated -> AI attribution
        // silently lost.
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        // A→B: the worktree's creation checkout (untraced by this family's cursor).
        // B→C: this command's commit. C→D: a trailing entry (e.g. a later op) so
        // the late offset lands mid-reflog rather than at EOF.
        let create_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcheckout: moving to wt\n");
        let commit_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: burst commit\n");
        let trailing_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcheckout: moving back\n");
        let late_offset = (create_line.len() + commit_line.len()) as u64;
        fs::write(
            &head_log,
            format!("{create_line}{commit_line}{trailing_line}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        // Cold: cursor NOT initialized.
        let mut cursor = RefCursor::new(family.clone());

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "burst commit"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }],
            "cold-start late ingress offset must not seed the cursor past the commit's own entry"
        );
    }

    #[test]
    fn cold_start_late_ingress_offset_does_not_skip_commit_on_uninitialized_common_ref() {
        // Variant of the above for a `common:` branch ref (e.g. the branch a
        // linked worktree commits on). command_start_offset_is_authoritative
        // returns true UNCONDITIONALLY for cold `common:` keys, so a late offset
        // (captured after git appended the branch's commit entry) seeds the
        // branch-ref cursor at EOF, past the commit's branch entry. The commit's
        // branch transition is then never found.
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let reference = "refs/heads/feature";
        let head_log = git_dir.join("logs/HEAD");
        let branch_log = git_dir.join("logs").join(reference);
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();
        fs::create_dir_all(branch_log.parent().unwrap()).unwrap();

        // HEAD records the commit normally (B→C), found from offset 0.
        let head_commit_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: feature commit\n");
        fs::write(&head_log, &head_commit_line).unwrap();

        // The branch ref also records B→C for the commit. The late ingress offset
        // points at EOF (after the commit's branch entry).
        let branch_commit_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: feature commit\n");
        fs::write(&branch_log, &branch_commit_line).unwrap();
        let late_branch_offset = branch_commit_line.len() as u64;

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        state.refs.insert(reference.to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "feature commit"]);
        cmd.reflog_start_offsets
            .insert(common_key(reference), late_branch_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        // The branch ref change must be present (not skipped by the late cold seed).
        assert!(
            cmd.ref_changes
                .iter()
                .any(|change| change.reference == reference && change.old == B && change.new == C),
            "cold-start late ingress offset on a common branch ref must not skip the commit's branch entry; got {:?}",
            cmd.ref_changes
        );
    }

    #[test]
    fn cold_unstructured_ref_mutation_recovers_head_transition_before_late_sample() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let rewrite =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tfilter-branch: rewrite\n");
        fs::write(&head_log, &rewrite).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(
            &family,
            Some(worktree),
            &["filter-branch", "--msg-filter", "cat"],
        );
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), rewrite.len() as u64);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }],
            "the sequenced pre-command HEAD must recover a mutation that finished before sampling"
        );
    }

    #[test]
    fn cold_noop_merge_does_not_consume_an_older_merge_at_the_start_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let older_merge =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tmerge feature: Merge made\n");
        let later_checkout =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcheckout: moving later\n");
        let command_start = older_merge.len() as u64;
        fs::write(&head_log, format!("{older_merge}{later_checkout}")).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["merge", "feature"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), command_start);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert!(
            cmd.ref_changes.is_empty(),
            "a no-op merge must not claim the older merge before its captured start boundary"
        );
    }

    #[test]
    fn late_ingress_offset_does_not_advance_in_order_cursor_past_own_commit() {
        // Regression for the graphite/gt-create flake. The family actor keeps one
        // RefCursor across commands. A prior command (e.g. a `switch`) advanced the
        // in-order HEAD cursor to exactly before this command's commit entry. The
        // async daemon-ingress offset capture then races and reads the reflog AFTER
        // git appended both the commit entry and a following switch entry, so the
        // `reflog_start_offsets` hint points PAST this commit's own entry.
        //
        // The in-order cursor is the authoritative floor; the late hint finds no
        // matching entry at/after it, so selection falls back to the cursor's first
        // match — this commit's own entry. The commit keeps its attribution.
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        // A→B: prior switch onto the new branch (already consumed; cursor sits at
        // end of this line). B→C: this command's commit. C→D: the switch-back gt
        // issues right after committing.
        let switch_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcheckout: moving to branch\n");
        let commit_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: gt create\n");
        let switch_back_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcheckout: moving back\n");
        let in_order_offset = switch_line.len() as u64;
        // Ingress captured the reflog late: after the commit entry was written but
        // before the switch-back, so the hint points just PAST this commit's entry
        // while a later record (the switch-back) still exists.
        let late_offset = (switch_line.len() + commit_line.len()) as u64;
        fs::write(
            &head_log,
            format!("{switch_line}{commit_line}{switch_back_line}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor
            .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
            .unwrap();

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "gt create"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }],
            "late ingress offset must not skip the commit's own entry"
        );
    }

    #[test]
    fn ingress_offset_hint_skips_untraced_duplicate_message_commit() {
        // The dual of the graphite case: an UNTRACED commit sharing a message sits
        // between the in-order cursor and this command's own commit. Here the
        // ingress offset was captured at the command's true start — after the
        // untraced commit — so it correctly biases selection to the later (traced)
        // entry. The hint must be honored, not ignored.
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        // A→B: prior traced base (consumed; cursor at end). B→C: an untraced commit
        // the daemon never saw, sharing the message. C→D: this command's commit.
        let base_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
        let untraced_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let traced_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let in_order_offset = base_line.len() as u64;
        // Hint captured at the true command start: after the untraced commit.
        let hint_offset = (base_line.len() + untraced_line.len()) as u64;
        fs::write(
            &head_log,
            format!("{base_line}{untraced_line}{traced_line}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor
            .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
            .unwrap();

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), hint_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }],
            "ingress hint must skip the untraced duplicate-message commit"
        );
    }

    #[test]
    fn late_ingress_offset_skips_untraced_duplicate_message_commit() {
        // Same duplicate-message shape as above, but the async ingress capture
        // raced behind git and observed the reflog after this command's commit
        // was already appended. With no candidate at/after the late hint, the
        // cursor must choose the latest matching entry before the hint rather
        // than the older untraced duplicate.
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let base_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
        let untraced_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let traced_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let in_order_offset = base_line.len() as u64;
        let late_hint_offset = (base_line.len() + untraced_line.len() + traced_line.len()) as u64;
        fs::write(
            &head_log,
            format!("{base_line}{untraced_line}{traced_line}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor
            .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
            .unwrap();

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_hint_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }],
            "late ingress hint must select the traced duplicate-message commit"
        );
    }

    #[test]
    fn commit_reflog_subject_rewritten_by_hook_falls_back_to_hinted_entry() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let base_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: traced base\n");
        let untraced_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: untraced\n");
        let hooked_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: hooked: raw subject\n");
        let in_order_offset = base_line.len() as u64;
        let hint_offset = (base_line.len() + untraced_line.len()) as u64;
        fs::write(
            &head_log,
            format!("{base_line}{untraced_line}{hooked_line}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor
            .initialize_reflog_cursor(&head_key(&git_dir), in_order_offset)
            .unwrap();

        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "raw subject"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), hint_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }],
            "commit-msg hooks may rewrite the final subject used by git's reflog"
        );
    }

    #[test]
    fn commit_reflog_subject_rewrite_fallback_does_not_guess_without_hint() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (A, B, "commit: untraced"),
                (B, C, "commit: hooked: raw subject"),
            ],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "raw subject"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert!(
            cmd.ref_changes.is_empty(),
            "without an exact message or command-start hint, multiple commit entries are ambiguous"
        );
    }

    #[test]
    fn amend_without_message_does_not_match_plain_commit_reflog_entry() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (A, B, "commit: older plain commit"),
                (B, C, "commit (amend): older plain commit"),
            ],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "--amend", "--no-edit"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );
    }

    #[test]
    fn commit_with_exact_reflog_message_ignores_stale_daemon_head() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let initial_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: initial\n");
        let raw_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: raw unseen\n");
        let traced_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: traced after raw\n");
        fs::write(&head_log, format!("{initial_line}{raw_line}{traced_line}")).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(
            &family,
            Some(worktree),
            &["commit", "-m", "traced after raw"],
        );

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn commit_reflog_boundary_skips_untraced_duplicate_message() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let initial_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\tcommit: initial\n");
        let raw_line =
            format!("{B} {C} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let traced_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcommit: same message\n");
        let start_offset = (initial_line.len() + raw_line.len()) as u64;
        fs::write(&head_log, format!("{initial_line}{raw_line}{traced_line}")).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["commit", "-m", "same message"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), start_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn first_observed_head_boundary_skips_prior_reset_history() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let old_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\treset: moving to old\n");
        let start_offset = old_line.len() as u64;
        let current_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\treset: moving to {D}\n");
        fs::write(&head_log, format!("{old_line}{current_line}")).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), A.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["reset", "--hard", D]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), start_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn reset_late_reflog_offset_uses_command_message_not_stale_state() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();

        let stale_line =
            format!("{A} {B} Test User <test@example.com> 0 +0000\treset: moving to stale\n");
        let current_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\treset: moving to {D}\n");
        let late_offset = (stale_line.len() + current_line.len()) as u64;
        fs::write(&head_log, format!("{stale_line}{current_line}")).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), A.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["reset", "--soft", D]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn first_observed_common_boundary_skips_prior_update_ref_history() {
        let temp = tempfile::tempdir().unwrap();
        let reference = "refs/heads/main";
        let log_path = temp.path().join("logs").join(reference);
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();

        let old_line = format!("{A} {B} Test User <test@example.com> 0 +0000\told stdin update\n");
        let start_offset = old_line.len() as u64;
        let current_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\tcurrent stdin update\n");
        fs::write(&log_path, format!("{old_line}{current_line}")).unwrap();

        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command(&family, &["update-ref", "--stdin"]);
        cmd.reflog_start_offsets
            .insert(common_key(reference), start_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: reference.to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn direct_branch_update_ref_uses_argv_transition_when_reflog_cursor_starts_too_late() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let reference = "refs/heads/feature";
        fs::create_dir_all(git_dir.join("logs").join("refs/heads")).unwrap();
        fs::create_dir_all(git_dir.join("logs")).unwrap();

        let branch_line = format!("{C} {D} Test User <test@example.com> 0 +0000\t\n");
        let head_line = format!("{C} {D} Test User <test@example.com> 0 +0000\t\n");
        let branch_log = git_dir.join("logs").join(reference);
        let head_log = git_dir.join("logs").join("HEAD");
        fs::write(&branch_log, &branch_line).unwrap();
        fs::write(&head_log, &head_line).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["update-ref", reference, D, C]);
        cmd.reflog_start_offsets
            .insert(common_key(reference), branch_line.len() as u64);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), head_line.len() as u64);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: reference.to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn direct_branch_update_ref_does_not_treat_stale_head_reflog_match_as_current_head_move() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let reference = "refs/heads/feature";
        fs::create_dir_all(git_dir.join("logs").join("refs/heads")).unwrap();
        fs::create_dir_all(git_dir.join("logs")).unwrap();

        let stale_branch_line = format!("{C} {D} Test User <test@example.com> 0 +0000\t\n");
        let stale_head_line = format!("{C} {D} Test User <test@example.com> 0 +0000\t\n");
        let branch_log = git_dir.join("logs").join(reference);
        let head_log = git_dir.join("logs").join("HEAD");
        fs::write(&branch_log, &stale_branch_line).unwrap();
        fs::write(&head_log, &stale_head_line).unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["update-ref", reference, D, C]);
        cmd.started_at_ns = 2_000_000_000;
        cmd.finished_at_ns = 2_000_000_000;
        cmd.reflog_start_offsets
            .insert(common_key(reference), stale_branch_line.len() as u64);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), stale_head_line.len() as u64);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: reference.to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn direct_update_ref_consumes_matching_reflog_entry_before_later_unstructured_update_ref() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(temp.path(), "refs/heads/main", &[(A, B, "")]);
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut direct = command(&family, &["update-ref", "refs/heads/main", B, A]);
        cursor.enrich_command(&mut direct, &state).unwrap();
        assert_eq!(
            direct.ref_changes,
            vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }]
        );

        let mut later = command(&family, &["update-ref", "--stdin"]);
        cursor.enrich_command(&mut later, &state).unwrap();

        assert!(
            later.ref_changes.is_empty(),
            "later unstructured update-ref must not replay reflog entry already represented by argv: {:?}",
            later.ref_changes
        );
    }

    #[test]
    fn direct_branch_update_ref_consumes_head_mirror_before_later_unstructured_update_ref() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let reference = "refs/heads/feature";
        append_reflog(&git_dir, reference, &[(A, B, "")]);
        append_reflog(&git_dir, "HEAD", &[(A, B, "")]);
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut direct = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["update-ref", reference, B, A],
        );
        cursor.enrich_command(&mut direct, &state).unwrap();
        assert_eq!(
            direct.ref_changes,
            vec![
                RefChange {
                    reference: reference.to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
            ]
        );

        let mut later = command_with_worktree(&family, Some(worktree), &["update-ref", "--stdin"]);
        cursor.enrich_command(&mut later, &state).unwrap();

        assert!(
            later.ref_changes.is_empty(),
            "later unstructured update-ref must not replay HEAD or branch reflog entries already represented by argv: {:?}",
            later.ref_changes
        );
    }

    #[test]
    fn direct_head_update_ref_uses_argv_and_late_cursor_branch_mirror_once() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let reference = "refs/heads/feature";
        append_reflog(&git_dir, reference, &[(A, B, "")]);
        append_reflog(&git_dir, "HEAD", &[(A, B, "")]);
        let branch_len = fs::metadata(git_dir.join("logs").join(reference))
            .unwrap()
            .len();
        let head_len = fs::metadata(git_dir.join("logs").join("HEAD"))
            .unwrap()
            .len();
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut direct = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["update-ref", "HEAD", B, A],
        );
        direct
            .reflog_start_offsets
            .insert(common_key(reference), branch_len);
        direct
            .reflog_start_offsets
            .insert(head_key(&git_dir), head_len);
        cursor.enrich_command(&mut direct, &state).unwrap();
        assert_eq!(
            direct.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
                RefChange {
                    reference: reference.to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
            ]
        );

        let mut later = command_with_worktree(&family, Some(worktree), &["update-ref", "--stdin"]);
        cursor.enrich_command(&mut later, &state).unwrap();

        assert!(
            later.ref_changes.is_empty(),
            "later unstructured update-ref must not replay branch mirror already represented by direct HEAD update-ref: {:?}",
            later.ref_changes
        );
    }

    #[test]
    fn direct_head_update_ref_uses_known_worktree_branch_when_other_branch_matches_same_second() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let current = "refs/heads/main";
        let other = "refs/heads/other";
        append_reflog(&git_dir, current, &[(A, B, "")]);
        append_reflog(&git_dir, other, &[(A, B, "")]);
        append_reflog(&git_dir, "HEAD", &[(A, B, "")]);
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.worktrees.insert(
            worktree.canonicalize().unwrap(),
            WorktreeState {
                head: Some(A.to_string()),
                branch: Some(current.to_string()),
                detached: false,
                last_updated_ns: 0,
            },
        );
        let mut cursor = RefCursor::new(family.clone());

        let mut direct = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["update-ref", "HEAD", B, A],
        );
        cursor.enrich_command(&mut direct, &state).unwrap();
        assert_eq!(
            direct.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
                RefChange {
                    reference: current.to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
            ]
        );

        let mut later =
            command_with_worktree(&family, Some(worktree), &["update-ref", other, B, A]);
        cursor.enrich_command(&mut later, &state).unwrap();

        assert_eq!(
            later.ref_changes,
            vec![RefChange {
                reference: other.to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }]
        );
    }

    #[test]
    fn direct_head_update_ref_without_known_branch_does_not_guess_ambiguous_branch_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        append_reflog(&git_dir, "refs/heads/main", &[(A, B, "")]);
        append_reflog(&git_dir, "refs/heads/other", &[(A, B, "")]);
        append_reflog(&git_dir, "HEAD", &[(A, B, "")]);
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut direct =
            command_with_worktree(&family, Some(worktree), &["update-ref", "HEAD", B, A]);
        cursor.enrich_command(&mut direct, &state).unwrap();

        assert_eq!(
            direct.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }]
        );
    }

    #[test]
    fn direct_branch_update_ref_does_not_attach_head_when_state_names_different_branch() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let current = "refs/heads/main";
        let updated = "refs/heads/feature";
        append_reflog(&git_dir, updated, &[(A, B, "")]);
        append_reflog(&git_dir, "HEAD", &[(A, B, "")]);
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.worktrees.insert(
            worktree.canonicalize().unwrap(),
            WorktreeState {
                head: Some(A.to_string()),
                branch: Some(current.to_string()),
                detached: false,
                last_updated_ns: 0,
            },
        );
        let mut cursor = RefCursor::new(family.clone());

        let mut direct =
            command_with_worktree(&family, Some(worktree), &["update-ref", updated, B, A]);
        cursor.enrich_command(&mut direct, &state).unwrap();

        assert_eq!(
            direct.ref_changes,
            vec![RefChange {
                reference: updated.to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }]
        );
    }

    #[test]
    fn common_ref_discovery_excludes_worktree_head_log() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(temp.path(), "HEAD", &[(A, B, "")]);
        append_reflog(temp.path(), "refs/heads/main", &[(A, B, "")]);
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let cursor = RefCursor::new(family);

        assert_eq!(
            cursor.discover_common_refs().unwrap(),
            vec!["refs/heads/main".to_string()]
        );
    }

    fn append_reflog(common_dir: &Path, reference: &str, entries: &[(&str, &str, &str)]) {
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

    fn reflog_line(old: &str, new: &str, message: &str) -> String {
        format!("{old} {new} Test User <test@example.com> 0 +0000\t{message}")
    }

    #[test]
    fn reflog_reader_ignores_trailing_unterminated_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("HEAD");
        let complete = format!("{}\n", reflog_line(A, B, "commit: complete"));
        let partial = reflog_line(B, C, "commit: partial");
        fs::write(&path, format!("{complete}{partial}")).unwrap();

        let records = read_reflog_records(&path, None).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].old, A);
        assert_eq!(records[0].new, B);

        fs::write(&path, format!("{complete}{partial}\n")).unwrap();
        let records = read_reflog_records(&path, None).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].old, B);
        assert_eq!(records[1].new, C);
    }

    #[test]
    fn reflog_anchor_rejects_non_newline_end_offset() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("HEAD");
        let complete = format!("{}\n", reflog_line(A, B, "commit: complete"));
        let partial = reflog_line(B, C, "commit: partial");
        fs::write(&path, format!("{complete}{partial}")).unwrap();

        let complete_record = read_reflog_record_ending_at(&path, complete.len() as u64)
            .unwrap()
            .expect("complete newline-terminated record should be readable");
        assert_eq!(complete_record.old, A);
        assert_eq!(complete_record.new, B);

        let partial_end = (complete.len() + partial.len()) as u64;
        assert!(
            read_reflog_record_ending_at(&path, partial_end)
                .unwrap()
                .is_none(),
            "an offset inside an unterminated reflog record must not become an anchor"
        );
    }

    #[test]
    fn rebase_span_stops_at_new_rebase_start_before_finish() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "rebase (start): checkout main"),
                (C, D, "rebase (pick): First rebase commit"),
                (D, E, "rebase (start): checkout other"),
                (E, F, "rebase (pick): Later rebase commit"),
            ],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn rebase_span_continuation_skips_stale_abort_before_selected_start() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        let head_log = git_dir.join("logs/HEAD");
        let branch_log = git_dir.join("logs/refs/heads/feature");
        fs::create_dir_all(head_log.parent().unwrap()).unwrap();
        fs::create_dir_all(branch_log.parent().unwrap()).unwrap();

        let failed_start = format!(
            "{B} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n"
        );
        let stale_abort = format!(
            "{C} {B} Test User <test@example.com> 0 +0000\trebase (abort): returning to refs/heads/stale-topic\n"
        );
        let checkout_feature = format!(
            "{B} {A} Test User <test@example.com> 0 +0000\tcheckout: moving from stale-topic to feature\n"
        );
        let feature_commit =
            format!("{A} {D} Test User <test@example.com> 0 +0000\tcommit: feature ai\n");
        let rebase_start = format!(
            "{D} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n"
        );
        let rebase_pick =
            format!("{C} {E} Test User <test@example.com> 0 +0000\trebase (pick): feature ai\n");
        let rebase_finish = format!(
            "{E} {E} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/feature\n"
        );
        let failed_start_offset = failed_start.len() as u64;
        let abort_offset = failed_start_offset + stale_abort.len() as u64;
        let checkout_offset = abort_offset + checkout_feature.len() as u64;
        let commit_offset = checkout_offset + feature_commit.len() as u64;
        fs::write(
            &head_log,
            format!(
                "{failed_start}{stale_abort}{checkout_feature}{feature_commit}{rebase_start}{rebase_pick}{rebase_finish}"
            ),
        )
        .unwrap();

        fs::write(
            &branch_log,
            format!(
                "{ZERO} {A} Test User <test@example.com> 0 +0000\tbranch: Created from {A}\n\
                 {A} {D} Test User <test@example.com> 0 +0000\tcommit: feature ai\n\
                 {D} {E} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/feature onto main\n",
                ZERO = zero_oid(),
            ),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/main".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/stale-topic".to_string(), B.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor
            .initialize_reflog_cursor(&head_key(&git_dir), failed_start_offset)
            .unwrap();

        let mut checkout = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["checkout", "-b", "feature", A],
        );
        checkout
            .reflog_start_offsets
            .insert(head_key(&git_dir), abort_offset);
        cursor.enrich_command(&mut checkout, &state).unwrap();
        for change in &checkout.ref_changes {
            state
                .refs
                .insert(change.reference.clone(), change.new.clone());
        }

        let mut commit = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["commit", "-m", "feature ai"],
        );
        commit
            .reflog_start_offsets
            .insert(head_key(&git_dir), checkout_offset);
        cursor.enrich_command(&mut commit, &state).unwrap();
        for change in &commit.ref_changes {
            state
                .refs
                .insert(change.reference.clone(), change.new.clone());
        }

        let mut rebase = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
        rebase
            .reflog_start_offsets
            .insert(head_key(&git_dir), commit_offset);
        cursor.enrich_command(&mut rebase, &state).unwrap();

        assert_eq!(
            rebase.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: D.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: E.to_string(),
                },
                RefChange {
                    reference: "refs/heads/feature".to_string(),
                    old: D.to_string(),
                    new: E.to_string(),
                },
            ],
            "rebase continuation must follow the selected start, not a stale untraced abort row before it"
        );
    }

    #[test]
    fn skipped_reflog_entry_remains_available_for_later_sequenced_command() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(
            temp.path(),
            "refs/heads/main",
            &[
                (A, B, "ordered second command"),
                (B, C, "ordered first command"),
            ],
        );
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut first = command(&family, &["update-ref", "refs/heads/main", C, B]);
        cursor.enrich_command(&mut first, &state).unwrap();
        assert_eq!(
            first.ref_changes,
            vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );

        let mut second = command(&family, &["update-ref", "refs/heads/main", B, A]);
        cursor.enrich_command(&mut second, &state).unwrap();
        assert_eq!(
            second.ref_changes,
            vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }]
        );
    }

    #[test]
    fn reflog_generation_reset_with_same_byte_length_clears_sparse_consumption() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(
            temp.path(),
            "refs/heads/main",
            &[
                (A, B, "ordered second command"),
                (B, C, "ordered first command"),
            ],
        );
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        let mut first = command(&family, &["update-ref", "refs/heads/main", C, B]);
        cursor.enrich_command(&mut first, &state).unwrap();
        assert_eq!(
            first.ref_changes,
            vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );

        let old_len = fs::metadata(temp.path().join("logs/refs/heads/main"))
            .unwrap()
            .len();
        append_reflog(
            temp.path(),
            "refs/heads/main",
            &[
                (A, B, "ordered second command"),
                (B, C, "ordered third command"),
            ],
        );
        assert_eq!(
            fs::metadata(temp.path().join("logs/refs/heads/main"))
                .unwrap()
                .len(),
            old_len
        );

        let mut second = command(&family, &["update-ref", "refs/heads/main", C, B]);
        cursor.enrich_command(&mut second, &state).unwrap();
        assert_eq!(
            second.ref_changes,
            vec![RefChange {
                reference: "refs/heads/main".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );
    }

    #[test]
    fn update_ref_stdin_is_reconstructed_from_reflog_delta() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(temp.path(), "refs/heads/main", &[(A, B, "stdin update")]);
        append_reflog(temp.path(), "refs/heads/topic", &[(A, C, "stdin update")]);
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command(&family, &["update-ref", "--stdin"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();
        cmd.ref_changes
            .sort_by(|left, right| left.reference.cmp(&right.reference));

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "refs/heads/main".to_string(),
                    old: A.to_string(),
                    new: B.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic".to_string(),
                    old: A.to_string(),
                    new: C.to_string(),
                },
            ]
        );
    }

    #[test]
    fn rebase_does_not_consume_adjacent_checkout_head_entry() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (A, B, "checkout: moving from topic-1 to topic-2"),
                (B, C, "rebase (start): checkout topic-1"),
                (C, D, "rebase (pick): Topic 2"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic-2",
            &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic-2".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn failed_explicit_branch_rebase_consumes_noop_start_marker_before_continue() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "rebase (pick): stale rebase from another branch"),
                (C, C, "rebase (finish): returning to refs/heads/stale-topic"),
                (A, A, "rebase (start): checkout master"),
                (A, E, "rebase (continue): Topic"),
                (E, E, "rebase (finish): returning to refs/heads/topic"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic",
            &[
                (A, D, "commit: Topic"),
                (D, E, "rebase (finish): refs/heads/topic onto main"),
            ],
        );

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut failed = command_with_worktree(
            &family,
            Some(worktree.clone()),
            &["rebase", "master", "topic"],
        );
        failed.exit_code = 1;

        cursor.enrich_command(&mut failed, &state).unwrap();

        assert_eq!(
            failed.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: A.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic".to_string(),
                    old: D.to_string(),
                    new: D.to_string(),
                },
            ]
        );

        let mut continued =
            command_with_worktree(&family, Some(worktree), &["rebase", "--continue"]);

        cursor.enrich_command(&mut continued, &state).unwrap();

        assert_eq!(
            continued.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: A.to_string(),
                    new: E.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic".to_string(),
                    old: D.to_string(),
                    new: E.to_string(),
                },
            ]
        );
    }

    #[test]
    fn failed_rebase_continue_does_not_steal_final_continue_entry() {
        // Two-conflict rebase: the FIRST `rebase --continue` resolves conflict
        // one with an empty/kept-side resolution and stops again on conflict
        // two (exit != 0). Its only reflog evidence so far is the start entry
        // (already consumed by the initial failed `rebase <branch>`); the
        // continue entry that later appears belongs to the SECOND, successful
        // `rebase --continue`. The failed continue must not consume it — doing
        // so leaves the final continue with no entry, no ref_changes, and the
        // conflict-resolution working log is never attributed (the
        // two-conflicts attribution flake).
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (A, B, "rebase (start): checkout main"),
                (B, C, "rebase (continue): Add Rust joke"),
                (C, C, "rebase (finish): returning to refs/heads/topic"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic",
            &[(D, C, "rebase (finish): refs/heads/topic onto main")],
        );

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());

        // Initial rebase stops on the first conflict: consumes the start entry.
        let mut initial =
            command_with_worktree(&family, Some(worktree.clone()), &["rebase", "main"]);
        initial.exit_code = 1;
        cursor.enrich_command(&mut initial, &state).unwrap();
        assert_eq!(
            initial.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: A.to_string(),
                new: B.to_string(),
            }],
            "initial failed rebase should consume only the start entry"
        );

        // First --continue resolves conflict one and stops on conflict two.
        // It produced no reflog entry of its own and must consume nothing.
        let mut stopped_continue =
            command_with_worktree(&family, Some(worktree.clone()), &["rebase", "--continue"]);
        stopped_continue.exit_code = 1;
        cursor
            .enrich_command(&mut stopped_continue, &state)
            .unwrap();
        assert_eq!(
            stopped_continue.ref_changes,
            Vec::<RefChange>::new(),
            "a --continue that stops on the next conflict must not steal the final continue's entry"
        );

        // Final --continue finishes the rebase: its continue + finish entries
        // must still be available so the branch transition is established.
        let mut final_continue =
            command_with_worktree(&family, Some(worktree), &["rebase", "--continue"]);
        cursor.enrich_command(&mut final_continue, &state).unwrap();
        assert!(
            final_continue
                .ref_changes
                .iter()
                .any(|change| change.reference == "refs/heads/topic"
                    && change.old == D
                    && change.new == C),
            "final --continue must recover the branch transition; got {:?}",
            final_continue.ref_changes
        );
    }

    #[test]
    fn cold_rebase_late_ingress_offset_still_recovers_start_and_branch_finish() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();

        let start_line = format!(
            "{B} {C} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n"
        );
        let pick_line =
            format!("{C} {D} Test User <test@example.com> 0 +0000\trebase (pick): Local commit\n");
        let finish_line = format!(
            "{D} {D} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
        );
        fs::write(
            git_dir.join("logs/HEAD"),
            format!("{start_line}{pick_line}{finish_line}"),
        )
        .unwrap();
        let late_head_offset = start_line.len() as u64;

        let branch_line = format!(
            "{B} {D} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
        );
        fs::write(git_dir.join("logs/refs/heads/topic"), &branch_line).unwrap();
        let late_branch_offset = branch_line.len() as u64;

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/topic".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_head_offset);
        cmd.reflog_start_offsets
            .insert(common_key("refs/heads/topic"), late_branch_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ],
            "cold late rebase enrichment must preserve the non-fast-forward local-tip to rebased-tip pair"
        );
    }

    #[test]
    fn cold_rebase_true_boundary_does_not_replay_older_rebase_span() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();

        let old_start = format!(
            "{A} {B} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n"
        );
        let old_pick =
            format!("{B} {C} Test User <test@example.com> 0 +0000\trebase (pick): Old commit\n");
        let old_finish = format!(
            "{C} {C} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
        );
        let current_start = format!(
            "{D} {E} Test User <test@example.com> 0 +0000\trebase (start): checkout main\n"
        );
        let current_pick = format!(
            "{E} {F} Test User <test@example.com> 0 +0000\trebase (pick): Current commit\n"
        );
        let current_finish = format!(
            "{F} {F} Test User <test@example.com> 0 +0000\trebase (finish): returning to refs/heads/topic\n"
        );
        let true_head_boundary = (old_start.len() + old_pick.len() + old_finish.len()) as u64;
        fs::write(
            git_dir.join("logs/HEAD"),
            format!(
                "{old_start}{old_pick}{old_finish}{current_start}{current_pick}{current_finish}"
            ),
        )
        .unwrap();

        let old_branch = format!(
            "{A} {C} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
        );
        let current_branch = format!(
            "{D} {F} Test User <test@example.com> 0 +0000\trebase (finish): refs/heads/topic onto main\n"
        );
        let true_branch_boundary = old_branch.len() as u64;
        fs::write(
            git_dir.join("logs/refs/heads/topic"),
            format!("{old_branch}{current_branch}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), D.to_string());
        state
            .refs
            .insert("refs/heads/topic".to_string(), D.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "main"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), true_head_boundary);
        cmd.reflog_start_offsets
            .insert(common_key("refs/heads/topic"), true_branch_boundary);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: D.to_string(),
                    new: E.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: E.to_string(),
                    new: F.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic".to_string(),
                    old: D.to_string(),
                    new: F.to_string(),
                },
            ],
            "true command-start boundary must not rewind into an older rebase span"
        );
    }

    #[test]
    fn rebase_span_stops_before_later_rebase_after_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "rebase (start): checkout topic-1"),
                (C, D, "rebase (pick): Topic 2"),
                (D, E, "checkout: moving from topic-2 to topic-3"),
                (E, F, "rebase (start): checkout topic-2"),
                (F, G, "rebase (pick): Topic 3"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic-2",
            &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic-3",
            &[(E, G, "rebase (finish): refs/heads/topic-3 onto topic-2")],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic-2".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn rebase_does_not_attach_unrelated_branch_with_same_new_tip() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "rebase (start): checkout topic-1"),
                (C, D, "rebase (pick): Topic 2"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic-2",
            &[(B, D, "rebase (finish): refs/heads/topic-2 onto topic-1")],
        );
        append_reflog(
            &git_dir,
            "refs/heads/unrelated",
            &[(E, D, "rebase (finish): refs/heads/unrelated onto topic-1")],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-1"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic-2".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn rebase_prefers_start_entry_when_expected_state_matches_pick() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "rebase (start): checkout topic-2"),
                (C, D, "rebase (pick): Topic 3"),
                (D, D, "rebase (finish): returning to refs/heads/topic-3"),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/topic-3",
            &[(B, D, "rebase (finish): refs/heads/topic-3 onto topic-2")],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/topic-2".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["rebase", "topic-2"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/topic-3".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn cherry_pick_span_starts_at_first_pick_when_expected_state_matches_second_pick() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "cherry-pick: Pick one"),
                (C, D, "cherry-pick: Pick two"),
            ],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/intermediate".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        cursor.pending_cherry_pick_source_oids = vec![A.to_string(), E.to_string()];
        let mut cmd =
            command_with_worktree(&family, Some(worktree), &["cherry-pick", "--continue"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn revert_span_starts_at_first_revert_when_expected_state_matches_second_revert() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[(B, C, "revert: Revert one"), (C, D, "revert: Revert two")],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/intermediate".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["revert", A, E]);
        let expected = ExpectedTransition::from_state_and_working_logs(&cmd, &state);

        cursor
            .consume_head_span_for_command_limited(&mut cmd, &state, &["revert:"], expected, 2)
            .unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn revert_continue_does_not_consume_following_plain_commit() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (B, C, "commit: Revert one"),
                (C, D, "commit: Unrelated follow-up"),
            ],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        cursor.pending_revert_source_oids = vec![A.to_string()];
        let mut continued =
            command_with_worktree(&family, Some(worktree.clone()), &["revert", "--continue"]);

        cursor.enrich_command(&mut continued, &state).unwrap();

        assert_eq!(
            continued.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );

        let mut ordinary = command_with_worktree(
            &family,
            Some(worktree),
            &["commit", "-m", "Unrelated follow-up"],
        );
        cursor.enrich_command(&mut ordinary, &state).unwrap();
        assert_eq!(
            ordinary.ref_changes,
            vec![RefChange {
                reference: "HEAD".to_string(),
                old: C.to_string(),
                new: D.to_string(),
            }]
        );
    }

    #[test]
    fn pull_reflog_action_uses_expanded_command_for_zero_arg_alias() {
        let family = FamilyKey::new("/repo/.git".to_string());
        let mut cmd = command_with_worktree(&family, None, &["up"]);
        cmd.primary_command = Some("pull".to_string());
        cmd.invoked_command = Some("pull".to_string());
        cmd.invoked_args.clear();

        assert_eq!(pull_reflog_action(&cmd), "pull");
    }

    #[test]
    fn pull_rebase_span_starts_at_start_entry_when_expected_state_matches_pick() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();
        append_reflog(
            &git_dir,
            "HEAD",
            &[
                (
                    B,
                    C,
                    "pull --rebase origin main (start): checkout origin/main",
                ),
                (C, D, "pull --rebase origin main (pick): Local commit"),
                (
                    D,
                    D,
                    "pull --rebase origin main (finish): returning to refs/heads/main",
                ),
            ],
        );
        append_reflog(
            &git_dir,
            "refs/heads/main",
            &[(
                B,
                D,
                "pull --rebase origin main (finish): refs/heads/main onto origin/main",
            )],
        );
        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/main".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(
            &family,
            Some(worktree),
            &["pull", "--rebase", "origin", "main"],
        );

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/main".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ]
        );
    }

    #[test]
    fn cold_pull_rebase_late_ingress_offset_still_recovers_start_and_branch_finish() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();

        let start_line = format!(
            "{B} {C} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout {C}\n"
        );
        let pick_line = format!(
            "{C} {D} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Local commit\n"
        );
        let finish_line = format!(
            "{D} {D} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
        );
        fs::write(
            git_dir.join("logs/HEAD"),
            format!("{start_line}{pick_line}{finish_line}"),
        )
        .unwrap();
        let late_head_offset = start_line.len() as u64;

        let branch_line = format!(
            "{B} {D} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto {C}\n"
        );
        fs::write(git_dir.join("logs/refs/heads/main"), &branch_line).unwrap();
        let late_branch_offset = branch_line.len() as u64;

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), C.to_string());
        state
            .refs
            .insert("refs/heads/main".to_string(), C.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["pull", "--rebase"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), late_head_offset);
        cmd.reflog_start_offsets
            .insert(common_key("refs/heads/main"), late_branch_offset);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: B.to_string(),
                    new: C.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: C.to_string(),
                    new: D.to_string(),
                },
                RefChange {
                    reference: "refs/heads/main".to_string(),
                    old: B.to_string(),
                    new: D.to_string(),
                },
            ],
            "cold late pull-rebase enrichment must preserve the non-fast-forward local-tip to rebased-tip pair"
        );
    }

    #[test]
    fn cold_pull_rebase_true_boundary_does_not_replay_older_pull_span() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("repo");
        let git_dir = create_git_dir(&worktree);
        fs::create_dir_all(git_dir.join("logs/refs/heads")).unwrap();

        let old_start = format!(
            "{A} {B} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout main\n"
        );
        let old_pick = format!(
            "{B} {C} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Old commit\n"
        );
        let old_finish = format!(
            "{C} {C} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
        );
        let current_start = format!(
            "{D} {E} Test User <test@example.com> 0 +0000\tpull --rebase (start): checkout main\n"
        );
        let current_pick = format!(
            "{E} {F} Test User <test@example.com> 0 +0000\tpull --rebase (pick): Current commit\n"
        );
        let current_finish = format!(
            "{F} {F} Test User <test@example.com> 0 +0000\tpull --rebase (finish): returning to refs/heads/main\n"
        );
        let true_head_boundary = (old_start.len() + old_pick.len() + old_finish.len()) as u64;
        fs::write(
            git_dir.join("logs/HEAD"),
            format!(
                "{old_start}{old_pick}{old_finish}{current_start}{current_pick}{current_finish}"
            ),
        )
        .unwrap();

        let old_branch = format!(
            "{A} {C} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto main\n"
        );
        let current_branch = format!(
            "{D} {F} Test User <test@example.com> 0 +0000\tpull --rebase (finish): refs/heads/main onto main\n"
        );
        let true_branch_boundary = old_branch.len() as u64;
        fs::write(
            git_dir.join("logs/refs/heads/main"),
            format!("{old_branch}{current_branch}"),
        )
        .unwrap();

        let family = FamilyKey::new(git_dir.to_string_lossy().to_string());
        let mut state = family_state(&family);
        state.refs.insert("HEAD".to_string(), D.to_string());
        state
            .refs
            .insert("refs/heads/main".to_string(), D.to_string());
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command_with_worktree(&family, Some(worktree), &["pull", "--rebase"]);
        cmd.reflog_start_offsets
            .insert(head_key(&git_dir), true_head_boundary);
        cmd.reflog_start_offsets
            .insert(common_key("refs/heads/main"), true_branch_boundary);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![
                RefChange {
                    reference: "HEAD".to_string(),
                    old: D.to_string(),
                    new: E.to_string(),
                },
                RefChange {
                    reference: "HEAD".to_string(),
                    old: E.to_string(),
                    new: F.to_string(),
                },
                RefChange {
                    reference: "refs/heads/main".to_string(),
                    old: D.to_string(),
                    new: F.to_string(),
                },
            ],
            "true command-start boundary must not rewind into an older pull-rebase span"
        );
    }

    #[test]
    fn cold_stash_push_uses_message_to_skip_raw_stash_history() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(
            temp.path(),
            "refs/stash",
            &[
                (A, B, "On main: old raw stash"),
                (B, C, "On main: current ai stash"),
            ],
        );
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command(&family, &["stash", "push", "-m", "current ai stash"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "refs/stash".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );
        assert_eq!(cursor.stash_stack, vec![C.to_string()]);
    }

    #[test]
    fn cold_stash_push_uses_command_reflog_boundary_without_message() {
        let temp = tempfile::tempdir().unwrap();
        let old_line = format!("{A} {B} Test User <test@example.com> 0 +0000\tWIP on main\n");
        let old_history_len = old_line.len() as u64;
        let current_line = format!("{B} {C} Test User <test@example.com> 0 +0000\tWIP on main\n");
        let path = temp.path().join("logs/refs/stash");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{old_line}{current_line}")).unwrap();
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command(&family, &["stash", "push"]);
        cmd.reflog_start_offsets
            .insert(common_key("refs/stash"), old_history_len);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "refs/stash".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );
        assert_eq!(cursor.stash_stack, vec![C.to_string()]);
    }

    #[test]
    fn cold_stash_save_uses_message_to_skip_raw_stash_history() {
        let temp = tempfile::tempdir().unwrap();
        append_reflog(
            temp.path(),
            "refs/stash",
            &[
                (A, B, "On main: old raw stash"),
                (B, C, "On main: current save stash"),
            ],
        );
        let family = FamilyKey::new(temp.path().to_string_lossy().to_string());
        let state = family_state(&family);
        let mut cursor = RefCursor::new(family.clone());
        let mut cmd = command(&family, &["stash", "save", "current", "save", "stash"]);

        cursor.enrich_command(&mut cmd, &state).unwrap();

        assert_eq!(
            cmd.ref_changes,
            vec![RefChange {
                reference: "refs/stash".to_string(),
                old: B.to_string(),
                new: C.to_string(),
            }]
        );
        assert_eq!(cursor.stash_stack, vec![C.to_string()]);
    }
}
