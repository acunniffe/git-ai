use super::*;

#[derive(Debug)]
pub struct RefCursor {
    pub(super) family: FamilyKey,
    pub(super) offsets: HashMap<String, u64>,
    pub(super) anchors: HashMap<String, ReflogAnchor>,
    pub(super) consumed_offsets: HashMap<String, HashSet<u64>>,
    pub(super) consumed_anchors: HashMap<String, HashMap<u64, ReflogAnchor>>,
    /// Per-command soft hints from the daemon-ingress reflog offset capture.
    /// Populated at the start of each command's enrichment and cleared when the
    /// next command's enrichment begins. Unlike `offsets` (the authoritative
    /// in-order cursor), these are captured asynchronously and may race ahead of
    /// or behind the command's own reflog entry, so they only *bias* entry
    /// selection — they never move the cursor. See `select_entry_for_hint`.
    pub(super) command_start_hints: HashMap<String, u64>,
    pub(super) stash_stack: Vec<String>,
    pub(super) pending_cherry_pick_source_oids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct CursorEntry {
    pub(super) key: String,
    pub(super) path: PathBuf,
    pub(super) reference: String,
    pub(super) old: String,
    pub(super) new: String,
    pub(super) message: String,
    pub(super) timestamp_secs: Option<i64>,
    pub(super) start_offset: u64,
    pub(super) end_offset: u64,
}

#[derive(Debug, Clone)]
pub(super) struct UpdateRefSpec {
    pub(super) reference: String,
    pub(super) new_oid: String,
    pub(super) old_oid: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum BranchCommandSpec {
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
pub(super) enum BranchLifecycleKind {
    Rename,
    Copy,
}

#[derive(Debug, Clone)]
pub(super) struct BranchLifecycleRecord {
    pub(super) old_reference: String,
    pub(super) oid: String,
}

#[derive(Debug, Clone)]
pub(super) struct ReflogRecord {
    pub(super) old: String,
    pub(super) new: String,
    pub(super) message: String,
    pub(super) timestamp_secs: Option<i64>,
    pub(super) start_offset: u64,
    pub(super) end_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReflogAnchor {
    pub(super) old: String,
    pub(super) new: String,
    pub(super) message: String,
    pub(super) end_offset: u64,
}

pub(super) const CHERRY_PICK_REFLOG_PREFIXES: &[&str] = &["cherry-pick:", "commit (cherry-pick):"];

pub(super) enum ColdSeedMatchSpec {
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
pub(super) struct ExpectedTransition {
    pub(super) old_oids: HashSet<String>,
    pub(super) new_oid: Option<String>,
    pub(super) messages: HashSet<String>,
}

impl ExpectedTransition {
    pub(super) fn with_reflog_messages(mut self, messages: HashSet<String>) -> Self {
        self.messages = messages;
        self
    }

    pub(super) fn without_reflog_message_constraint(mut self) -> Self {
        self.messages.clear();
        self
    }

    pub(super) fn without_old_oid_constraint(mut self) -> Self {
        self.old_oids.clear();
        self
    }

    pub(super) fn from_state_and_working_logs(
        cmd: &NormalizedCommand,
        state: &FamilyState,
    ) -> Self {
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

    pub(super) fn matches(&self, entry: &CursorEntry) -> bool {
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

    pub(super) fn matches_span_boundary(&self, entry: &CursorEntry) -> bool {
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

#[derive(Debug, Clone, Copy)]
pub(super) struct ReflogTimestampWindow {
    pub(super) start_secs: i64,
    pub(super) end_secs: i64,
}

impl ReflogTimestampWindow {
    pub(super) fn contains(self, timestamp_secs: i64) -> bool {
        timestamp_secs >= self.start_secs && timestamp_secs <= self.end_secs
    }
}
