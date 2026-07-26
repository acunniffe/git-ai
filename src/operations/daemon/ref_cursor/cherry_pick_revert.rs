use super::*;

impl RefCursor {
    pub(super) fn enrich_cherry_pick(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = parsed_invocation_for_normalized_command(cmd).command_args;
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
            cherry_pick_source_args_from_command_args(&args)
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

        let source_limit = if unresolved_explicit_sources {
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

    pub(super) fn enrich_revert(
        &mut self,
        cmd: &mut NormalizedCommand,
        state: &FamilyState,
    ) -> Result<(), GitAiError> {
        let args = parsed_invocation_for_normalized_command(cmd).command_args;
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"))
        {
            return Ok(());
        }

        let is_no_commit = args.iter().any(|arg| arg == "--no-commit" || arg == "-n");
        let is_continue = args.iter().any(|arg| arg == "--continue");
        let is_skip = args.iter().any(|arg| arg == "--skip");
        // DEFERRED (code-review #13): on `revert --continue` (resuming after a
        // conflict) the original source OIDs are not on the command line and we
        // carry no `pending_revert_source_oids` from the interrupted revert, so
        // revert_source_oids ends up empty. handle_revert_commit then falls back
        // to first-parent, which is only exact for `git revert HEAD`; a
        // multi-commit `revert A B` resumed via --continue can reconstruct the
        // wrong source base. A precise fix needs the daemon to persist the
        // pending source OIDs across the conflict pause and replay them here.
        let source_args = if is_continue || is_skip {
            Vec::new()
        } else {
            revert_source_args_from_command_args(&args)
        };
        let explicit_sources = if source_args.is_empty() {
            Some(Vec::new())
        } else {
            resolve_cherry_pick_source_oids_from_sources(cmd, state, &source_args)?
        };
        let unresolved_explicit_sources = !source_args.is_empty() && explicit_sources.is_none();
        cmd.revert_source_oids = explicit_sources.unwrap_or_default();

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
            &["revert:"],
            self.head_expected_transition(cmd, state),
            source_limit,
        )
    }
}
