#[allow(unused_imports)]
use super::*;
use crate::error::GitAiError;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::operations::daemon::git_backend::GitBackend;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::oneshot;

impl ActorDaemonCoordinator {
    pub(crate) async fn apply_trace_payload_to_state(
        &self,
        payload: Value,
    ) -> Result<TracePayloadApplyOutcome, GitAiError> {
        let payload_root_sid = Self::trace_payload_root_sid(&payload);
        let event = payload
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if event == TRACE_CONNECTION_CLOSED_EVENT {
            let Some(root_sid) = payload_root_sid.as_deref() else {
                return Ok(TracePayloadApplyOutcome::None);
            };
            {
                let mut normalizer = self.normalizer.lock().await;
                let _ = normalizer.sweep_orphans_for_roots(&[root_sid.to_string()]);
            }
            let replaced_family = self
                .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
                .await?;
            let outcome = if replaced_family.is_some() {
                TracePayloadApplyOutcome::QueuedFamily
            } else {
                TracePayloadApplyOutcome::None
            };
            self.clear_trace_root_tracking(root_sid)?;
            self.drain_ready_family_sequencers_after_root_cleared(replaced_family)
                .await?;
            return Ok(outcome);
        }

        self.maybe_append_pending_root_from_trace_payload(&payload)?;
        let emitted = {
            let mut normalizer = self.normalizer.lock().await;
            normalizer.ingest_payload(&payload)?
        };
        let Some(command) = emitted else {
            if is_terminal_root_trace_event(
                &event,
                payload
                    .get("sid")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                payload_root_sid.as_deref().unwrap_or_default(),
            ) && let Some(root_sid) = payload_root_sid.as_deref()
                && let Some(family) = self
                    .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
                    .await?
            {
                self.clear_trace_root_tracking(root_sid)?;
                self.drain_ready_family_sequencers_after_root_cleared(Some(family))
                    .await?;
                return Ok(TracePayloadApplyOutcome::QueuedFamily);
            }
            return Ok(TracePayloadApplyOutcome::None);
        };
        let root_sid = command.root_sid.clone();

        let mut family_to_drain_after_clear = None;
        let outcome = if let Some(family) = self
            .replace_pending_root_entry(
                &root_sid,
                FamilySequencerEntry::ReadyCommand(Box::new(command.clone())),
            )
            .await?
        {
            self.cache_commit_file_timestamp_snapshots_for_command(&command)?;
            family_to_drain_after_clear = Some(family);
            TracePayloadApplyOutcome::QueuedFamily
        } else if let Some(family) = command.family_key.as_ref().map(|family| family.0.clone())
            && Self::trace_invocation_participates_in_family_sequencer(
                command.primary_command.as_deref(),
                &command.raw_argv,
            )
        {
            self.cache_commit_file_timestamp_snapshots_for_command(&command)?;
            self.append_ready_command_entry(&family, command).await?;
            family_to_drain_after_clear = Some(family);
            TracePayloadApplyOutcome::QueuedFamily
        } else {
            match self.coordinator.route_command(command).await {
                Ok(applied) => TracePayloadApplyOutcome::Applied(Box::new(applied)),
                Err(error) => {
                    let _ = self.clear_trace_root_tracking(&root_sid);
                    return Err(error);
                }
            }
        };
        self.clear_trace_root_tracking(&root_sid)?;
        self.drain_ready_family_sequencers_after_root_cleared(family_to_drain_after_clear)
            .await?;
        Ok(outcome)
    }

    pub(crate) async fn ingest_trace_payload_fast(
        self: Arc<Self>,
        payload: Value,
    ) -> Result<(), GitAiError> {
        if !is_trace_payload(&payload) {
            return Ok(());
        }
        match self.apply_trace_payload_to_state(payload).await? {
            TracePayloadApplyOutcome::None | TracePayloadApplyOutcome::QueuedFamily => {}
            TracePayloadApplyOutcome::Applied(applied) => {
                if let Some(family) = applied.command.family_key.as_ref().map(|key| key.0.clone()) {
                    self.begin_family_effect(&family)?;
                    let mut commit_file_timestamp_snapshots =
                        Self::start_commit_file_timestamp_snapshots_for_command(&applied.command);
                    let result = self
                        .maybe_apply_side_effects_for_applied_command(
                            Some(&family),
                            &applied,
                            &mut commit_file_timestamp_snapshots,
                        )
                        .await;
                    let _ = self.end_family_effect(&family);
                    if let Err(error) = &result {
                        let _ = self.record_side_effect_error(&family, applied.seq, error);
                        tracing::error!(
                            %error,
                            %family,
                            seq = applied.seq,
                            "async side-effect error"
                        );
                    }
                    if let Err(error) =
                        self.append_command_completion_log(&family, &applied, &result, applied.seq)
                    {
                        let _ = self.record_side_effect_error(&family, applied.seq, &error);
                        tracing::error!(
                            %error,
                            %family,
                            seq = applied.seq,
                            "async completion log write failed"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn ingest_checkpoint_payload(
        &self,
        request: CheckpointRequest,
    ) -> Result<ControlResponse, GitAiError> {
        if request.files.is_empty() {
            return Ok(ControlResponse::ok(None, None));
        }

        let repo_work_dir = request.files[0].repo_work_dir.clone();
        let family = self.backend.resolve_family(&repo_work_dir)?;

        let (respond_to, response) = oneshot::channel();
        self.append_checkpoint_to_family_sequencer(&family.0, request, Some(respond_to))
            .await?;
        response
            .await
            .map_err(|_| GitAiError::Generic("checkpoint response channel closed".to_string()))??;
        Ok(ControlResponse::ok(None, None))
    }

    pub(crate) async fn ingest_checkpoint_delivery(
        &self,
        delivery: crate::model::checkpoint_delivery::CheckpointDelivery,
    ) -> Result<ControlResponse, GitAiError> {
        delivery
            .validate()
            .map_err(|error| GitAiError::PresetError(error.to_string()))?;
        // Stamp the envelope identity onto the request so the applied
        // checkpoint records it; at-least-once outbox replay of the same
        // delivery then deduplicates instead of applying twice.
        let mut request = delivery.request;
        request.delivery_id = Some(delivery.delivery_id);
        self.ingest_validated_checkpoint_control_payload(request)
            .await
    }

    pub(crate) async fn ingest_checkpoint_control_payload(
        &self,
        request: CheckpointRequest,
    ) -> Result<ControlResponse, GitAiError> {
        crate::model::checkpoint_delivery::validate_checkpoint_request_bounds(&request)
            .map_err(|error| GitAiError::PresetError(error.to_string()))?;
        self.ingest_validated_checkpoint_control_payload(request)
            .await
    }

    async fn ingest_validated_checkpoint_control_payload(
        &self,
        mut request: CheckpointRequest,
    ) -> Result<ControlResponse, GitAiError> {
        crate::operations::daemon::checkpoint_stream_authority::authorize_checkpoint_stream_source(
            &mut request,
        )?;
        self.notify_stream_worker_checkpoint(&request);
        self.ingest_checkpoint_payload(request).await
    }
}
