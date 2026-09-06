//! Snapshot of the daemon's attribution pipeline, served by the
//! `status.daemon` control request (`git-ai bg status`) so a fenced or stalled
//! family is inspectable without the daemon log. Counts come from atomics and
//! `try_lock`ed maps: a contended map is reported as `snapshot_partial` rather
//! than waited on; fence state is observed through the drain's own
//! classification without releasing anything, with the reflog and liveness
//! observations made after every lock is dropped; and nothing here touches git.

use super::{
    ActorDaemonCoordinator, FAMILY_CAUSAL_FENCE_HARD_CAP_MULTIPLIER,
    FAMILY_WRITTEN_ROOT_FENCE_CAP_MULTIPLIER, FamilySequencerEntry, IngestLossSnapshot, RootFence,
    RootFenceProbe, now_unix_nanos,
};
use crate::api::types::DaemonLogFieldValue;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Pending work older than this is a stall even under a long causal grace.
const SEQUENCER_STALL_FLOOR: Duration = Duration::from_secs(10);
/// Families detailed in the heartbeat (oldest pending work first). With the
/// aggregates this keeps a heartbeat under the 64-field daemon-log contract.
pub const HEARTBEAT_FAMILY_DETAIL_LIMIT: usize = 2;

/// Open mutating roots observed (a reflog stat each) per snapshot. Beyond
/// this many concurrently open commands the snapshot is reported partial
/// rather than doing unbounded per-root work on the control path.
pub(crate) const HEALTH_ROOT_OBSERVE_LIMIT: usize = 32;

/// One family's pending attribution work, oldest first in
/// [`DaemonHealthSnapshot::families`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub(crate) struct FamilyHealth {
    pub key: String,
    pub entries: usize,
    pub entries_commands: usize,
    pub entries_applied: usize,
    pub entries_checkpoints: usize,
    pub entries_untraced_scans: usize,
    /// Age of the entry that has been ready the longest.
    pub oldest_entry_age_ms: u64,
    pub front_kind: Option<&'static str>,
    /// The front entry is waiting for an older open trace root.
    pub fenced: bool,
    pub inflight_effects: usize,
    pub side_effect_errors: usize,
}

/// What the snapshot needs about a family's front entry to judge its fence.
struct FrontEntry {
    started_at_ns: u128,
    waited: Duration,
    root_sid: Option<String>,
}

/// One open mutating trace root as sampled under the ingress lock.
struct RootSample {
    probe: RootFenceProbe,
    started_at_ns: Option<u128>,
    family: Option<String>,
    /// Whether the probe's observations ran; a root beyond the observation
    /// limit is counted but its fence cannot be judged.
    observed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DaemonHealthSnapshot {
    pub uptime_seconds: u64,
    pub causal_grace_ms: u64,
    /// A map was contended when sampled; its counts read as zero here.
    pub snapshot_partial: bool,
    pub checkpoints_outstanding: usize,
    pub checkpoints_outstanding_bytes: usize,
    pub checkpoints_unadmitted: usize,
    pub checkpoint_receipt_seq_next: u64,
    pub checkpoint_receipt_seq_processed: u64,
    pub trace_payloads_queued: usize,
    pub trace_ingest_seq_lag: u64,
    pub trace_roots_open_mutating: usize,
    /// Roots whose final frame the reader has read but the worker has not yet
    /// processed. These hold their fence outright and are not re-judged.
    pub trace_roots_finishing: usize,
    /// Still-running roots that have already written their worktree `HEAD`.
    pub trace_roots_written: usize,
    pub trace_root_oldest_open_age_ms: Option<u64>,
    pub trace_connections_unidentified: usize,
    pub sequencer_families: usize,
    pub sequencer_entries_total: usize,
    pub sequencer_entries_commands: usize,
    pub sequencer_entries_applied: usize,
    pub sequencer_entries_checkpoints: usize,
    pub sequencer_oldest_entry_age_ms: Option<u64>,
    pub sequencer_fenced_families: usize,
    /// Pending work older than the fence's hard cap (and at least
    /// [`SEQUENCER_STALL_FLOOR`]): nothing legitimate holds work that long.
    pub sequencer_stalled: bool,
    pub effects_inflight_families: usize,
    pub effects_inflight_total: usize,
    pub side_effect_error_families: usize,
    pub side_effect_errors_total: usize,
    pub causal_grace_expirations: u64,
    pub causal_fence_hard_cap_releases: u64,
    pub sequencer_entries_untraced_scans: usize,
    /// Commits the untraced-commit fixup attributed (no trace2 ever saw them).
    pub untraced_commits_fixed: u64,
    /// Reflog records the fixup settled without a claim (rewrites, replays,
    /// detached commits, already-noted commits).
    pub untraced_commits_skipped: u64,
    /// Fixup cursors that no longer matched their reflog and restarted at its end.
    pub untraced_cursor_reseeds: u64,
    pub untraced_scan_errors: u64,
    #[serde(flatten)]
    pub losses: IngestLossSnapshot,
    pub families: Vec<FamilyHealth>,
}

impl DaemonHealthSnapshot {
    pub(crate) fn capture(coordinator: &ActorDaemonCoordinator) -> Self {
        let mut snapshot_partial = false;
        let mut families: BTreeMap<String, FamilyHealth> = BTreeMap::new();
        let mut fronts: Vec<(String, FrontEntry)> = Vec::new();
        match coordinator.family_sequencers_by_family.try_lock() {
            Ok(sequencers) => {
                for (key, state) in sequencers.iter() {
                    let Some((order, front)) = state.entries.first_key_value() else {
                        continue;
                    };
                    let (root_sid, kind) =
                        ActorDaemonCoordinator::sequencer_entry_identity(&front.entry);
                    fronts.push((
                        key.clone(),
                        FrontEntry {
                            started_at_ns: order.started_at_ns,
                            waited: front.enqueued_at.elapsed(),
                            root_sid: root_sid.map(str::to_string),
                        },
                    ));
                    let health = families.entry(key.clone()).or_insert_with(|| FamilyHealth {
                        key: key.clone(),
                        front_kind: Some(kind),
                        ..FamilyHealth::default()
                    });
                    for slot in state.entries.values() {
                        match &slot.entry {
                            FamilySequencerEntry::ReadyCommand(_) => health.entries_commands += 1,
                            FamilySequencerEntry::AppliedSideEffects { .. } => {
                                health.entries_applied += 1
                            }
                            FamilySequencerEntry::UntracedCommitScan { .. } => {
                                health.entries_untraced_scans += 1
                            }
                            FamilySequencerEntry::Checkpoint { .. } => {
                                health.entries_checkpoints += 1
                            }
                        }
                        health.oldest_entry_age_ms = health
                            .oldest_entry_age_ms
                            .max(slot.enqueued_at.elapsed().as_millis() as u64);
                    }
                    health.entries = health.entries_commands
                        + health.entries_applied
                        + health.entries_checkpoints;
                }
            }
            Err(_) => snapshot_partial = true,
        }

        let mut effects_inflight_total = 0;
        match coordinator.inflight_effects_by_family.try_lock() {
            Ok(effects) => {
                for (key, count) in effects.iter().filter(|(_, count)| **count > 0) {
                    families
                        .entry(key.clone())
                        .or_insert_with(|| FamilyHealth {
                            key: key.clone(),
                            ..FamilyHealth::default()
                        })
                        .inflight_effects = *count;
                    effects_inflight_total += *count;
                }
            }
            Err(_) => snapshot_partial = true,
        }

        let mut side_effect_error_families = 0;
        let mut side_effect_errors_total = 0;
        match coordinator.side_effect_errors_by_family.try_lock() {
            Ok(errors) => {
                for (key, family_errors) in errors.iter().filter(|(_, e)| !e.is_empty()) {
                    side_effect_error_families += 1;
                    side_effect_errors_total += family_errors.len();
                    families
                        .entry(key.clone())
                        .or_insert_with(|| FamilyHealth {
                            key: key.clone(),
                            ..FamilyHealth::default()
                        })
                        .side_effect_errors = family_errors.len();
                }
            }
            Err(_) => snapshot_partial = true,
        }

        let mut trace_connections_unidentified = 0;
        let mut trace_root_oldest_open_age_ms: Option<u64> = None;
        let mut samples: Vec<RootSample> = Vec::new();
        match coordinator.trace_ingress_state.try_lock() {
            Ok(ingress) => {
                let now = now_unix_nanos();
                trace_connections_unidentified = ingress.unidentified_open_connections;
                for root in ingress.root_open_connections.keys() {
                    if !ActorDaemonCoordinator::open_root_may_mutate_family(&ingress, root, None) {
                        continue;
                    }
                    let started_at_ns = ingress.root_started_at_ns.get(root).copied();
                    if let Some(started) = started_at_ns {
                        let age_ms = (now.saturating_sub(started) / 1_000_000) as u64;
                        trace_root_oldest_open_age_ms =
                            Some(trace_root_oldest_open_age_ms.map_or(age_ms, |m| m.max(age_ms)));
                    }
                    samples.push(RootSample {
                        probe: ActorDaemonCoordinator::root_fence_probe(
                            &ingress,
                            root,
                            Duration::ZERO,
                        ),
                        started_at_ns,
                        family: ingress.root_families.get(root).cloned(),
                        observed: false,
                    });
                }
            }
            Err(_) => snapshot_partial = true,
        }
        // Observations (a reflog stat and a liveness probe per root) run with
        // every lock dropped, oldest roots first, and never for more roots
        // than the limit.
        let grace = coordinator.causal_grace;
        samples.sort_by_key(|sample| sample.started_at_ns.unwrap_or(u128::MAX));
        if samples.len() > HEALTH_ROOT_OBSERVE_LIMIT {
            snapshot_partial = true;
        }
        for sample in samples.iter_mut().take(HEALTH_ROOT_OBSERVE_LIMIT) {
            sample.probe.observe_all();
            sample.observed = true;
        }
        let trace_roots_open_mutating = samples.len();
        let trace_roots_finishing = samples.iter().filter(|s| s.probe.finishing).count();
        let trace_roots_written = samples
            .iter()
            .filter(|s| s.probe.wrote_refs == Some(true))
            .count();

        let hard_cap = grace * FAMILY_CAUSAL_FENCE_HARD_CAP_MULTIPLIER;
        let written_cap = grace * FAMILY_WRITTEN_ROOT_FENCE_CAP_MULTIPLIER;
        let mut sequencer_stalled = false;
        for (key, front) in &fronts {
            let Some(health) = families.get_mut(key) else {
                continue;
            };
            let fence = front_fence(coordinator, &samples, key, front);
            health.fenced = fence.held;
            // A family whose side-effect pass is running is busy, not stuck:
            // its queued entries wait for that pass, not for a fence.
            if health.inflight_effects > 0 {
                continue;
            }
            // Work fenced by a root that has written may legitimately wait as
            // long as that root runs; anything else is bounded by the hard cap.
            let bound = if fence.behind_written_root {
                written_cap
            } else {
                hard_cap
            };
            if Duration::from_millis(health.oldest_entry_age_ms) > bound.max(SEQUENCER_STALL_FLOOR)
            {
                sequencer_stalled = true;
            }
        }

        let mut families: Vec<FamilyHealth> = families.into_values().collect();
        families.sort_by(|a, b| {
            b.oldest_entry_age_ms
                .cmp(&a.oldest_entry_age_ms)
                .then_with(|| a.key.cmp(&b.key))
        });
        let sequencer_oldest_entry_age_ms = families
            .iter()
            .filter(|f| f.entries > 0)
            .map(|f| f.oldest_entry_age_ms)
            .max();
        let (checkpoints_outstanding, checkpoints_outstanding_bytes) =
            coordinator.outstanding_checkpoint_state();
        let sum = |pick: fn(&FamilyHealth) -> usize| families.iter().map(pick).sum::<usize>();

        Self {
            uptime_seconds: coordinator.started_at.elapsed().as_secs(),
            causal_grace_ms: grace.as_millis() as u64,
            snapshot_partial,
            checkpoints_outstanding,
            checkpoints_outstanding_bytes,
            checkpoints_unadmitted: coordinator.unadmitted_checkpoints.load(Ordering::Acquire),
            checkpoint_receipt_seq_next: coordinator
                .next_checkpoint_receipt_seq
                .load(Ordering::Acquire) as u64,
            checkpoint_receipt_seq_processed: coordinator
                .processed_checkpoint_receipt_seq
                .load(Ordering::Acquire) as u64,
            trace_payloads_queued: coordinator.queued_trace_payloads.load(Ordering::Acquire),
            trace_ingest_seq_lag: coordinator
                .next_trace_ingest_seq
                .load(Ordering::Acquire)
                .saturating_sub(
                    coordinator
                        .processed_trace_ingest_seq
                        .load(Ordering::Acquire),
                ) as u64,
            trace_roots_open_mutating,
            trace_roots_finishing,
            trace_roots_written,
            trace_root_oldest_open_age_ms,
            trace_connections_unidentified,
            sequencer_families: families.iter().filter(|f| f.entries > 0).count(),
            sequencer_entries_total: sum(|f| f.entries),
            sequencer_entries_commands: sum(|f| f.entries_commands),
            sequencer_entries_applied: sum(|f| f.entries_applied),
            sequencer_entries_checkpoints: sum(|f| f.entries_checkpoints),
            sequencer_oldest_entry_age_ms,
            sequencer_fenced_families: families.iter().filter(|f| f.fenced).count(),
            sequencer_stalled,
            effects_inflight_families: families.iter().filter(|f| f.inflight_effects > 0).count(),
            effects_inflight_total,
            side_effect_error_families,
            side_effect_errors_total,
            causal_grace_expirations: coordinator.causal_grace_expirations.load(Ordering::Relaxed),
            causal_fence_hard_cap_releases: coordinator
                .causal_fence_hard_cap_releases
                .load(Ordering::Relaxed),
            sequencer_entries_untraced_scans: sum(|f| f.entries_untraced_scans),
            untraced_commits_fixed: coordinator.untraced_commits_fixed.load(Ordering::Relaxed),
            untraced_commits_skipped: coordinator.untraced_commits_skipped.load(Ordering::Relaxed),
            untraced_cursor_reseeds: coordinator.untraced_cursor_reseeds.load(Ordering::Relaxed),
            untraced_scan_errors: coordinator.untraced_scan_errors.load(Ordering::Relaxed),
            losses: IngestLossSnapshot::capture(coordinator),
            families,
        }
    }
}

/// How the causal fence stands for a family's front entry, judged from the
/// sampled roots exactly as the drain would (without applying releases).
struct FrontFence {
    held: bool,
    /// The fence is held by a root that has already written refs.
    behind_written_root: bool,
}

fn front_fence(
    coordinator: &ActorDaemonCoordinator,
    samples: &[RootSample],
    family: &str,
    front: &FrontEntry,
) -> FrontFence {
    let mut fence = FrontFence {
        held: false,
        behind_written_root: false,
    };
    for sample in samples {
        if front.root_sid.as_deref() == Some(sample.probe.root_sid.as_str())
            || sample.family.as_deref().is_some_and(|f| f != family)
            || sample
                .started_at_ns
                .is_some_and(|root_started| root_started > front.started_at_ns)
        {
            continue;
        }
        // An unobserved root cannot be judged: report it as legitimately
        // holding rather than invent a stall from a partial snapshot, though
        // no root holds past the written-root cap.
        if !sample.observed {
            let written_cap = coordinator.causal_grace * FAMILY_WRITTEN_ROOT_FENCE_CAP_MULTIPLIER;
            if front.waited < written_cap {
                fence.held = true;
                fence.behind_written_root = true;
            }
            continue;
        }
        let mut probe = sample.probe.clone();
        probe.waited = front.waited;
        if matches!(coordinator.classify_root_fence(&probe), RootFence::Held(_)) {
            fence.held = true;
            fence.behind_written_root |= probe.wrote_refs == Some(true);
        }
    }
    fence
}

impl DaemonHealthSnapshot {
    /// Flattens the snapshot into primitive heartbeat fields: every aggregate
    /// under its own name, plus the [`HEARTBEAT_FAMILY_DETAIL_LIMIT`] families
    /// with the oldest pending work as `family_<i>_<field>`. Family keys are
    /// local repository paths and never leave the machine: the heartbeat
    /// carries a stable `family_<i>_id` digest instead, enough to correlate
    /// consecutive heartbeats and daemon log lines that carry the same digest.
    pub fn heartbeat_fields(&self) -> BTreeMap<String, DaemonLogFieldValue> {
        let mut fields = BTreeMap::new();
        let Ok(Value::Object(snapshot)) = serde_json::to_value(self) else {
            return fields;
        };
        for (key, value) in &snapshot {
            if let Some(value) = primitive_field(value) {
                fields.insert(key.clone(), value);
            }
        }
        for (index, family) in self
            .families
            .iter()
            .filter(|family| family.entries > 0)
            .take(HEARTBEAT_FAMILY_DETAIL_LIMIT)
            .enumerate()
        {
            let Ok(Value::Object(family_fields)) = serde_json::to_value(family) else {
                continue;
            };
            fields.insert(
                format!("family_{index}_id"),
                DaemonLogFieldValue::from(family_id(&family.key)),
            );
            for (key, value) in &family_fields {
                if key == "key" {
                    continue;
                }
                if let Some(value) = primitive_field(value) {
                    fields.insert(format!("family_{index}_{key}"), value);
                }
            }
        }
        fields
    }
}

/// A stable, non-reversible digest of a family key (a local path).
fn family_id(family_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(family_key.as_bytes());
    digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn primitive_field(value: &Value) -> Option<DaemonLogFieldValue> {
    match value {
        Value::Null => Some(DaemonLogFieldValue::Null),
        Value::Bool(value) => Some(DaemonLogFieldValue::Bool(*value)),
        Value::Number(value) => Some(DaemonLogFieldValue::Number(value.clone())),
        Value::String(value) => Some(DaemonLogFieldValue::String(value.clone())),
        Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::DaemonLogFieldValue;

    /// The daemon-log upload contract keeps at most this many fields per event
    /// (see `daemon_log_layer::MAX_FIELDS_PER_EVENT`); the heartbeat must stay
    /// under it even with the per-family detail attached.
    const MAX_FIELDS_PER_EVENT: usize = 64;

    fn family(key: &str, oldest_entry_age_ms: u64) -> FamilyHealth {
        FamilyHealth {
            key: key.to_string(),
            oldest_entry_age_ms,
            entries: 1,
            entries_checkpoints: 1,
            front_kind: Some("checkpoint"),
            fenced: true,
            ..FamilyHealth::default()
        }
    }

    fn snapshot(families: Vec<FamilyHealth>) -> DaemonHealthSnapshot {
        DaemonHealthSnapshot {
            uptime_seconds: 1,
            causal_grace_ms: 1_000,
            snapshot_partial: false,
            checkpoints_outstanding: 0,
            checkpoints_outstanding_bytes: 0,
            checkpoints_unadmitted: 0,
            checkpoint_receipt_seq_next: 0,
            checkpoint_receipt_seq_processed: 0,
            trace_payloads_queued: 0,
            trace_ingest_seq_lag: 0,
            trace_roots_open_mutating: 2,
            trace_roots_finishing: 0,
            trace_roots_written: 1,
            trace_root_oldest_open_age_ms: Some(42),
            trace_connections_unidentified: 0,
            sequencer_families: families.len(),
            sequencer_entries_total: families.len(),
            sequencer_entries_commands: 0,
            sequencer_entries_applied: 0,
            sequencer_entries_checkpoints: families.len(),
            sequencer_oldest_entry_age_ms: families.iter().map(|f| f.oldest_entry_age_ms).max(),
            sequencer_fenced_families: families.len(),
            sequencer_stalled: true,
            effects_inflight_families: 0,
            effects_inflight_total: 0,
            side_effect_error_families: 0,
            side_effect_errors_total: 0,
            causal_grace_expirations: 3,
            causal_fence_hard_cap_releases: 0,
            sequencer_entries_untraced_scans: 0,
            untraced_commits_fixed: 0,
            untraced_commits_skipped: 0,
            untraced_cursor_reseeds: 0,
            untraced_scan_errors: 0,
            losses: IngestLossSnapshot::default(),
            families,
        }
    }

    #[test]
    fn heartbeat_health_fields_flatten_aggregates_and_worst_families() {
        let fields = snapshot(vec![
            family("/repos/slow/.git", 90_000),
            family("/repos/fast/.git", 5),
        ])
        .heartbeat_fields();

        assert_eq!(
            fields.get("sequencer_stalled"),
            Some(&DaemonLogFieldValue::from(true))
        );
        assert_eq!(
            fields.get("trace_roots_open_mutating"),
            Some(&DaemonLogFieldValue::from(2_u64))
        );
        assert_eq!(
            fields.get("trace_root_oldest_open_age_ms"),
            Some(&DaemonLogFieldValue::from(42_u64))
        );
        assert_eq!(
            fields.get("causal_grace_expirations"),
            Some(&DaemonLogFieldValue::from(3_u64))
        );
        assert!(
            !fields.contains_key("families"),
            "nested values are not primitive heartbeat fields"
        );
        assert!(
            fields.values().all(|value| {
                !matches!(value, DaemonLogFieldValue::String(text) if text.contains("/repos/"))
            }),
            "repository paths never leave the machine: {fields:?}"
        );
        assert_eq!(
            fields.get("family_0_id"),
            Some(&DaemonLogFieldValue::from(family_id("/repos/slow/.git"))),
            "families are detailed oldest pending work first, by digest"
        );
        assert_eq!(
            fields.get("family_0_oldest_entry_age_ms"),
            Some(&DaemonLogFieldValue::from(90_000_u64))
        );
        assert_eq!(
            fields.get("family_0_front_kind"),
            Some(&DaemonLogFieldValue::from("checkpoint"))
        );
        assert_eq!(
            fields.get("family_0_fenced"),
            Some(&DaemonLogFieldValue::from(true))
        );
        assert_eq!(
            fields.get("family_1_id"),
            Some(&DaemonLogFieldValue::from(family_id("/repos/fast/.git")))
        );
        assert!(!fields.contains_key("family_2_id"));
        assert_eq!(
            fields.get("trace_payloads_dropped_queue_full"),
            Some(&DaemonLogFieldValue::from(0_u64)),
            "loss counters are flattened under their wire keys"
        );
    }

    #[test]
    fn heartbeat_health_fields_stay_within_daemon_log_field_cap() {
        let families = (0..20)
            .map(|index| family(&format!("/repos/{index}/.git"), 20 - index))
            .collect();
        let fields = snapshot(families).heartbeat_fields();

        assert!(
            fields.len() <= MAX_FIELDS_PER_EVENT,
            "{} heartbeat fields exceed the {MAX_FIELDS_PER_EVENT}-field contract",
            fields.len()
        );
        let detailed_families = fields.keys().filter(|key| key.ends_with("_id")).count();
        assert_eq!(detailed_families, HEARTBEAT_FAMILY_DETAIL_LIMIT);
        assert_eq!(
            fields.get("family_0_id"),
            Some(&DaemonLogFieldValue::from(family_id("/repos/0/.git")))
        );
    }

    #[test]
    fn heartbeat_health_fields_detail_only_families_with_pending_work() {
        let idle = FamilyHealth {
            key: "/repos/idle/.git".to_string(),
            inflight_effects: 1,
            ..FamilyHealth::default()
        };
        let fields = snapshot(vec![idle, family("/repos/pending/.git", 5)]).heartbeat_fields();
        assert_eq!(
            fields.get("family_0_id"),
            Some(&DaemonLogFieldValue::from(family_id("/repos/pending/.git"))),
            "a family with only in-flight effects is not pending work"
        );
        assert!(!fields.contains_key("family_1_id"));
    }
}
