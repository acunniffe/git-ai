use super::*;

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
        }
    }

    pub(super) fn consume_unique_direct_common_ref_matching_timestamp(
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

    pub(super) fn find_direct_ref_transition_entry(
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

    pub(super) fn direct_ref_transition_entries(
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

    pub(super) fn find_common_ref_entry(
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

    pub(super) fn find_common_ref_entry_without_hint(
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

    pub(super) fn find_entry_in_log(
        &mut self,
        key: String,
        path: &Path,
        reference: &str,
        expected: ExpectedTransition,
        message_prefixes: &[&str],
    ) -> Result<Option<CursorEntry>, GitAiError> {
        self.find_entry_in_log_with_hint(key, path, reference, expected, message_prefixes, true)
    }

    pub(super) fn find_entry_in_log_with_hint(
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

    pub(super) fn find_head_entry_in_command_window(
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
    pub(super) fn select_candidate_with_hint<I>(
        &self,
        key: &str,
        candidates: I,
    ) -> Option<CursorEntry>
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

    pub(super) fn consume_common_refs_matching_transition(
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

    pub(super) fn discover_common_refs(&self) -> Result<Vec<String>, GitAiError> {
        let logs = self.common_dir().join("logs");
        let mut refs = Vec::new();
        discover_reflog_refs(&logs, &logs, &mut refs)?;
        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    pub(super) fn entry_consumed(&self, entry: &CursorEntry) -> bool {
        self.consumed_offsets
            .get(&entry.key)
            .is_some_and(|offsets| offsets.contains(&entry.end_offset))
            && self
                .consumed_anchors
                .get(&entry.key)
                .and_then(|anchors| anchors.get(&entry.end_offset))
                .is_some_and(|anchor| anchor == &ReflogAnchor::from(entry))
    }

    pub(super) fn reflog_start_offset(
        &mut self,
        key: &str,
        path: &Path,
    ) -> Result<Option<u64>, GitAiError> {
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

    pub(super) fn initialize_reflog_cursor(
        &mut self,
        key: &str,
        offset: u64,
    ) -> Result<(), GitAiError> {
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

    pub(super) fn command_start_offset_is_authoritative(
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

    pub(super) fn reflog_has_records_after_offset(
        &self,
        key: &str,
        offset: u64,
    ) -> Result<bool, GitAiError> {
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

    pub(super) fn reflog_path_for_key(&self, key: &str) -> Option<PathBuf> {
        if let Some(reference) = key.strip_prefix("common:") {
            return Some(self.common_dir().join("logs").join(reference));
        }
        let git_dir = key
            .strip_prefix("worktree:")
            .and_then(|value| value.strip_suffix(":HEAD"))?;
        Some(PathBuf::from(git_dir).join("logs").join("HEAD"))
    }

    pub(super) fn ref_tip_at_cursor_start(&self, key: &str) -> Option<String> {
        self.anchors
            .get(key)
            .map(|anchor| anchor.new.clone())
            .filter(|oid| valid_non_zero_oid(oid))
    }

    pub(super) fn consume_entry(&mut self, entry: &CursorEntry) -> Result<(), GitAiError> {
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

    pub(super) fn advance_cursor_to_entry(&mut self, entry: &CursorEntry) {
        self.offsets.insert(entry.key.clone(), entry.end_offset);
        self.anchors
            .insert(entry.key.clone(), ReflogAnchor::from(entry));
        self.consumed_offsets.remove(&entry.key);
        self.consumed_anchors.remove(&entry.key);
    }

    pub(super) fn compact_consumed_entries(
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

    pub(super) fn common_dir(&self) -> PathBuf {
        PathBuf::from(&self.family.0)
    }

    pub(super) fn head_expected_transition(
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

    pub(super) fn head_cursor_initialized(&self, worktree: Option<&Path>) -> bool {
        let Some(worktree) = worktree else {
            return false;
        };
        let Some(git_dir) = git_dir_for_worktree(worktree) else {
            return false;
        };
        self.offsets.contains_key(&head_key(&git_dir))
    }

    pub(super) fn clear_ref_cursor(&mut self, key: &str) {
        self.offsets.remove(key);
        self.anchors.remove(key);
        self.consumed_offsets.remove(key);
        self.consumed_anchors.remove(key);
    }
}
