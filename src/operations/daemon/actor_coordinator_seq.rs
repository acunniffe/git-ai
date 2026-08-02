use super::daemon_config::test_completion_log_path;
#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::repository::error::PersistenceError;
use crate::operations::git::repo_state::common_dir_for_worktree;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;

impl ActorDaemonCoordinator {
    pub(crate) fn trace_invocation_participates_in_family_sequencer(
        primary_command: Option<&str>,
        argv: &[String],
    ) -> bool {
        primary_command.is_some_and(|cmd| {
            crate::operations::git::command_classification::git_invocation_participates_in_family_sequencer(
                cmd,
                &trace_invocation_command_args(Some(cmd), argv),
            )
        })
    }

    pub(crate) fn append_pending_root_entry(
        &self,
        family: &str,
        root_sid: &str,
        started_at_ns: u128,
    ) -> Result<(), GitAiError> {
        {
            let pending_slots = self.pending_root_slots_by_root.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "pending root slots map",
                }
            })?;
            if pending_slots.contains_key(root_sid) {
                return Ok(());
            }
        }

        let order = {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            let state =
                sequencers
                    .entry(family.to_string())
                    .or_insert_with(|| FamilySequencerState {
                        next_ordinal: 1,
                        entries: BTreeMap::new(),
                    });
            let order = FamilySequencerOrder {
                started_at_ns,
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state
                .entries
                .insert(order, FamilySequencerEntry::PendingRoot);
            order
        };

        self.pending_root_slots_by_root
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned {
                what: "pending root slots map",
            })?
            .insert(
                root_sid.to_string(),
                PendingRootSlot {
                    family: family.to_string(),
                    order,
                },
            );
        Ok(())
    }

    pub(crate) fn take_pending_root_slot(
        &self,
        root_sid: &str,
    ) -> Result<Option<PendingRootSlot>, GitAiError> {
        self.pending_root_slots_by_root
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned {
                what: "pending root slots map",
            })
            .map_err(Into::into)
            .map(|mut slots| slots.remove(root_sid))
    }

    pub(crate) fn maybe_append_pending_root_from_trace_payload(
        &self,
        payload: &Value,
    ) -> Result<(), GitAiError> {
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event == TRACE_CONNECTION_CLOSED_EVENT {
            return Ok(());
        }

        let Some(sid) = payload.get("sid").and_then(Value::as_str) else {
            return Ok(());
        };
        let root_sid = trace_root_sid(sid);
        if root_sid != sid {
            return Ok(());
        }

        let argv = trace_payload_effective_argv(payload);
        let primary_command =
            trace_payload_primary_command(payload).or_else(|| trace_argv_primary_command(&argv));
        if !Self::trace_invocation_participates_in_family_sequencer(
            primary_command.as_deref(),
            &argv,
        ) {
            return Ok(());
        }

        let Some(worktree) = trace_payload_worktree_hint(payload) else {
            return Ok(());
        };
        let Some(common_dir) = common_dir_for_worktree(&worktree) else {
            return Ok(());
        };
        let started_at_ns = trace_payload_root_started_at_ns(payload)
            .or_else(|| trace_payload_time_ns(payload))
            .unwrap_or_else(now_unix_nanos);
        let family = crate::operations::git::canonicalize::canonicalize_or_self(&common_dir)
            .to_string_lossy()
            .to_string();
        self.append_pending_root_entry(&family, root_sid, started_at_ns)
    }

    pub(crate) async fn append_ready_command_entry(
        &self,
        family: &str,
        command: crate::model::domain::NormalizedCommand,
    ) -> Result<(), GitAiError> {
        let exec_lock = self.side_effect_exec_lock(family)?;
        let _guard = exec_lock.lock().await;
        {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            let state =
                sequencers
                    .entry(family.to_string())
                    .or_insert_with(|| FamilySequencerState {
                        next_ordinal: 1,
                        entries: BTreeMap::new(),
                    });
            let order = FamilySequencerOrder {
                started_at_ns: command.started_at_ns,
                ordinal: state.next_ordinal,
            };
            state.next_ordinal = state.next_ordinal.saturating_add(1);
            state
                .entries
                .insert(order, FamilySequencerEntry::ReadyCommand(Box::new(command)));
        }
        self.drain_ready_family_sequencer_entries_locked(family)
            .await
    }

    pub(crate) async fn drain_ready_family_sequencer_entries(
        &self,
        family: &str,
    ) -> Result<(), GitAiError> {
        let exec_lock = self.side_effect_exec_lock(family)?;
        let _guard = exec_lock.lock().await;
        self.drain_ready_family_sequencer_entries_locked(family)
            .await
    }

    pub(crate) async fn drain_all_ready_family_sequencers(&self) -> Result<(), GitAiError> {
        let families = {
            let map = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            map.keys().cloned().collect::<Vec<_>>()
        };
        for family in families {
            self.drain_ready_family_sequencer_entries(&family).await?;
        }
        Ok(())
    }

    pub(crate) async fn drain_ready_family_sequencers_after_root_cleared(
        &self,
        family: Option<String>,
    ) -> Result<(), GitAiError> {
        if let Some(family) = family {
            self.drain_ready_family_sequencer_entries(&family).await
        } else {
            self.drain_all_ready_family_sequencers().await
        }
    }

    pub(crate) async fn replace_pending_root_entry(
        &self,
        root_sid: &str,
        replacement: FamilySequencerEntry,
    ) -> Result<Option<String>, GitAiError> {
        let Some(slot) = self.take_pending_root_slot(root_sid)? else {
            return Ok(None);
        };
        let family = slot.family.clone();
        let exec_lock = self.side_effect_exec_lock(&family)?;
        let _guard = exec_lock.lock().await;
        {
            let mut sequencers = self.family_sequencers_by_family.lock().map_err(|_| {
                PersistenceError::LockPoisoned {
                    what: "family sequencer map",
                }
            })?;
            let state = sequencers
                .entry(family.clone())
                .or_insert_with(|| FamilySequencerState {
                    next_ordinal: 1,
                    entries: BTreeMap::new(),
                });
            let Some(entry) = state.entries.get_mut(&slot.order) else {
                return Err(GitAiError::Generic(format!(
                    "missing pending root sequencer entry for sid={} family={} order={:?}",
                    root_sid, family, slot.order
                )));
            };
            match entry {
                FamilySequencerEntry::PendingRoot => {
                    *entry = replacement;
                }
                _ => {
                    return Err(GitAiError::Generic(format!(
                        "sequencer entry for sid={} family={} order={:?} was not pending",
                        root_sid, family, slot.order
                    )));
                }
            }
        }
        self.drain_ready_family_sequencer_entries_locked(&family)
            .await?;
        Ok(Some(family))
    }

    pub(crate) fn family_entry_blocked_by_prior_open_trace_root(
        &self,
        family: &str,
        started_at_ns: u128,
        entry_root_sid: Option<&str>,
    ) -> Result<bool, GitAiError> {
        let ingress =
            self.trace_ingress_state
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "trace ingress state",
                })?;

        for (root_sid, open_count) in &ingress.root_open_connections {
            if *open_count == 0 || entry_root_sid == Some(root_sid.as_str()) {
                continue;
            }
            if ingress.root_definitely_read_only.contains(root_sid) {
                continue;
            }
            if !ingress.root_mutating.get(root_sid).copied().unwrap_or(true) {
                continue;
            }
            if ingress
                .root_started_at_ns
                .get(root_sid)
                .copied()
                .is_some_and(|root_started| root_started > started_at_ns)
            {
                continue;
            }
            if ingress
                .root_families
                .get(root_sid)
                .is_none_or(|root_family| root_family == family)
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub(crate) fn record_side_effect_error(
        &self,
        family: &str,
        seq: u64,
        error: &GitAiError,
    ) -> Result<(), GitAiError> {
        let mut map = self.side_effect_errors_by_family.lock().map_err(|_| {
            PersistenceError::LockPoisoned {
                what: "side effect errors map",
            }
        })?;
        let family_errors = map.entry(family.to_string()).or_insert_with(BTreeMap::new);
        family_errors.insert(seq, error.to_string());
        while family_errors.len() > 256 {
            if let Some(oldest) = family_errors.keys().next().copied() {
                family_errors.remove(&oldest);
            } else {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn latest_side_effect_error(
        &self,
        family: &str,
    ) -> Result<Option<String>, GitAiError> {
        let map = self.side_effect_errors_by_family.lock().map_err(|_| {
            PersistenceError::LockPoisoned {
                what: "side effect errors map",
            }
        })?;
        Ok(map
            .get(family)
            .and_then(|errors| errors.iter().next_back().map(|(_, error)| error.clone())))
    }

    pub(crate) fn record_recent_replay_prerequisite(
        &self,
        family: &str,
        prerequisite: RecentReplayPrerequisite,
    ) -> Result<(), GitAiError> {
        const MAX_RECENT_REPLAY_PREREQUISITES_PER_FAMILY: usize = 256;

        let mut map = self
            .recent_replay_prerequisites_by_family
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned {
                what: "recent replay prerequisites map",
            })?;
        let entries = map.entry(family.to_string()).or_insert_with(VecDeque::new);
        entries.push_back(prerequisite);
        while entries.len() > MAX_RECENT_REPLAY_PREREQUISITES_PER_FAMILY {
            let _ = entries.pop_front();
        }
        Ok(())
    }

    pub(crate) fn maybe_append_test_completion_log(
        &self,
        family: &str,
        entry: &TestCompletionLogEntry,
    ) -> Result<(), GitAiError> {
        let Some(dir) = self.test_completion_log_dir.as_ref() else {
            return Ok(());
        };
        let _guard =
            self.test_completion_log_lock
                .lock()
                .map_err(|_| PersistenceError::LockPoisoned {
                    what: "test completion log",
                })?;

        fs::create_dir_all(dir)?;
        let path = test_completion_log_path(dir, family);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(entry).map_err(GitAiError::from)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    pub(crate) fn append_command_completion_log(
        &self,
        family: &str,
        applied: &crate::model::domain::AppliedCommand,
        result: &Result<(), GitAiError>,
        error_order: u64,
    ) -> Result<(), GitAiError> {
        let sync_tracked =
            crate::operations::daemon::test_sync::tracks_primary_command_for_test_sync(
                applied.command.primary_command.as_deref(),
                &applied.command.invoked_args,
            );
        let test_sync_session =
            crate::operations::daemon::test_sync::test_sync_session_from_invocation(
                &parsed_invocation_for_normalized_command(&applied.command),
            );
        let events = &applied.analysis.events;
        let log_entry = TestCompletionLogEntry {
            seq: applied.seq,
            family_key: family.to_string(),
            kind: "command".to_string(),
            primary_command: applied.command.primary_command.clone(),
            test_sync_session,
            exit_code: Some(applied.command.exit_code),
            sync_tracked,
            status: if result.is_ok() {
                "ok".to_string()
            } else {
                "error".to_string()
            },
            error: result.as_ref().err().map(|error| error.to_string()),
            semantic_events: events.iter().map(semantic_event_kind).collect(),
            commit_shas: commit_created_shas(events),
            commit_skip_reason: commit_skip_reason(
                applied.command.primary_command.as_deref(),
                events,
            ),
        };
        if let Err(error) = self.maybe_append_test_completion_log(family, &log_entry) {
            let _ = self.record_side_effect_error(family, error_order, &error);
            return Err(error);
        }
        Ok(())
    }
}

/// Variant name of a `SemanticEvent`, e.g. `"CommitCreated"` or
/// `"OpaqueCommand"`. Diagnostic-only (populates `TestCompletionLogEntry`):
/// derived from `Debug` so newly added `SemanticEvent` variants are covered
/// automatically instead of silently falling through a hand-maintained match.
fn semantic_event_kind(event: &crate::model::domain::SemanticEvent) -> String {
    let debug = format!("{event:?}");
    debug
        .split(['{', '('])
        .next()
        .unwrap_or(&debug)
        .trim()
        .to_string()
}

/// `new_head` SHAs from `CommitCreated`/`CommitAmended` events, in event
/// order. Non-empty here means the analyzer resolved a HEAD transition for
/// the command and post-commit note generation was attempted for that SHA
/// (subject to repo allow-listing / rebase deferral inside
/// `handle_commit_created`/`handle_commit_amended`).
fn commit_created_shas(events: &[crate::model::domain::SemanticEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            crate::model::domain::SemanticEvent::CommitCreated { new_head, .. }
            | crate::model::domain::SemanticEvent::CommitAmended { new_head, .. } => {
                Some(new_head.clone())
            }
            _ => None,
        })
        .collect()
}

/// `Some("opaque_command")` when the analyzer's only event was
/// `OpaqueCommand` -- i.e. it fell back to the no-op default because
/// ref-change enrichment (`RefCursor::enrich_command`) produced no
/// HEAD/branch transition for a command that HistoryAnalyzer expected one
/// from (commit/reset/rebase/cherry-pick/merge/revert/update-ref). For a
/// command that actually completed (exit 0) this is the reflog-cursor race
/// documented in the daemon-trace2-ingestion spec, not a note-write or
/// filesystem-visibility problem.
///
/// `pub(crate)`: also reused by `actor_coordinator_side_effects` as the
/// fail-loud gate for exit-0 commits where enrichment still found no HEAD
/// transition, so both sites recognize the exact same condition instead of
/// drifting apart.
pub(crate) fn commit_skip_reason(
    primary_command: Option<&str>,
    events: &[crate::model::domain::SemanticEvent],
) -> Option<String> {
    // Only commit-family commands can "skip" note generation; for anything else
    // an OpaqueCommand outcome is routine (e.g. `git branch` with no enriched
    // ref change) and stamping a reason here would let the diagnostics-focused
    // tests flake on the very race this field exists to diagnose.
    (primary_command == Some("commit")
        && matches!(events, [crate::model::domain::SemanticEvent::OpaqueCommand]))
    .then(|| "opaque_command".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::domain::SemanticEvent;

    #[test]
    fn semantic_event_kind_names_struct_and_unit_variants() {
        assert_eq!(
            semantic_event_kind(&SemanticEvent::CommitCreated {
                base: None,
                new_head: "abc123".to_string(),
            }),
            "CommitCreated"
        );
        assert_eq!(
            semantic_event_kind(&SemanticEvent::OpaqueCommand),
            "OpaqueCommand"
        );
    }

    #[test]
    fn commit_created_shas_collects_new_head_from_commit_and_amend_events_only() {
        let events = vec![
            SemanticEvent::CommitCreated {
                base: Some("base1".to_string()),
                new_head: "head1".to_string(),
            },
            SemanticEvent::CommitAmended {
                old_head: "old2".to_string(),
                new_head: "head2".to_string(),
            },
        ];
        assert_eq!(commit_created_shas(&events), vec!["head1", "head2"]);
        assert!(commit_created_shas(&[SemanticEvent::OpaqueCommand]).is_empty());
    }

    #[test]
    fn commit_skip_reason_flags_opaque_only_event_list() {
        assert_eq!(
            commit_skip_reason(Some("commit"), &[SemanticEvent::OpaqueCommand]),
            Some("opaque_command".to_string())
        );
    }

    #[test]
    fn commit_skip_reason_is_none_when_commit_created_present_or_events_absent() {
        let commit_created = vec![SemanticEvent::CommitCreated {
            base: None,
            new_head: "head1".to_string(),
        }];
        assert_eq!(commit_skip_reason(Some("commit"), &commit_created), None);
        assert_eq!(commit_skip_reason(Some("commit"), &[]), None);
        // Non-commit commands never carry a skip reason, even when opaque.
        assert_eq!(
            commit_skip_reason(Some("branch"), &[SemanticEvent::OpaqueCommand]),
            None
        );
        assert_eq!(
            commit_skip_reason(None, &[SemanticEvent::OpaqueCommand]),
            None
        );

        // OpaqueCommand alongside another event should never happen in
        // practice (HistoryAnalyzer only pushes it when `events` started
        // empty), but the classification stays conservative if it ever did.
        let mixed = vec![
            SemanticEvent::OpaqueCommand,
            SemanticEvent::CommitCreated {
                base: None,
                new_head: "head1".to_string(),
            },
        ];
        assert_eq!(commit_skip_reason(Some("commit"), &mixed), None);
    }
}
