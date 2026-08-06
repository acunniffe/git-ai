use super::*;

impl RefCursor {
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
        if !command_uses_ref_cursor(primary) {
            return Ok(command_start_refs);
        }

        match primary {
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
                } else {
                    self.consume_head_transition_for_command(
                        cmd,
                        state,
                        &["checkout:"],
                        self.head_expected_transition(cmd, state),
                    )
                }
            }
            "switch" => self.consume_head_transition_for_command(
                cmd,
                state,
                &["checkout:", "switch:"],
                self.head_expected_transition(cmd, state),
            ),
            "merge" => self.consume_head_transition_for_command(
                cmd,
                state,
                &["merge"],
                self.head_expected_transition(cmd, state),
            ),
            "cherry-pick" => self.enrich_cherry_pick(cmd, state),
            "rebase" => self.consume_rebase_transition(cmd, state),
            "pull" => self.consume_pull_transition(cmd, state),
            "branch" => self.enrich_branch(cmd, state),
            "stash" => self.enrich_stash(cmd, state),
            "update-ref" => self.enrich_update_ref(cmd, state),
            _ => Ok(()),
        }?;

        if !cmd.ref_changes.is_empty() {
            cmd.confidence = Confidence::High;
        }
        Ok(command_start_refs)
    }

    pub(super) fn initialize_from_command_reflog_start_offsets(
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
    pub(super) fn clamp_seed_to_own_entry(
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

    pub(super) fn clamp_seed_to_pull_span_entry(
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

    pub(super) fn clamp_seed_to_rebase_span_entry(
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
    pub(super) fn cold_seed_match_spec(
        &self,
        cmd: &NormalizedCommand,
    ) -> Option<ColdSeedMatchSpec> {
        // Only commands with enough reflog structure to distinguish their own
        // rows should clamp a cold asynchronous seed backward. Single-entry
        // commits use their subject-specific reflog message; rebase/pull use
        // their start/pick/finish span shape.
        let args = command_args(cmd);
        match cmd.primary_command.as_deref()? {
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
                let args = parsed_invocation_for_normalized_command(cmd).command_args;
                if args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"))
                    || args.iter().any(|arg| arg == "--no-commit" || arg == "-n")
                {
                    return None;
                }
                let source_args = cherry_pick_source_args_from_command_args(&args);
                let limit = if source_args
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
            _ => None,
        }
    }

    pub(super) fn enrich_commit(
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
        let Some(entry) = self.find_commit_head_entry(cmd, prefixes, expected)? else {
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
        // Keep the argv-derived subject as the normal path. A commit-msg hook
        // can rewrite that subject before Git records it in the reflog, so only
        // an exact-match miss may use the bounded command-window fallback.
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

    pub(super) fn consume_head_entry_for_command(
        &mut self,
        cmd: &mut NormalizedCommand,
        entry: CursorEntry,
    ) -> Result<(), GitAiError> {
        self.consume_entry(&entry)?;
        let old = entry.old.clone();
        let new = entry.new.clone();
        let mut changes = vec![entry_to_ref_change(&entry)];
        self.consume_common_refs_matching_transition(&old, &new, &mut changes)?;
        dedup_ref_changes(&mut changes);
        cmd.ref_changes = changes;
        Ok(())
    }

    pub(super) fn consume_head_transition_for_command(
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

    pub(super) fn consume_head_span_for_command_limited(
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

    // Span commands can append several contiguous HEAD rows. If command ingress
    // captured a late reflog offset, prefer the first matching span at/after that
    // hint, then fall back to the latest matching span before the hint when the
    // hint landed after the command's own rows.
    pub(super) fn find_head_span_start_entry(
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

    pub(super) fn find_head_entry(
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

    pub(super) fn find_head_entry_without_hint(
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

    pub(super) fn find_head_entry_after(
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
}
