use super::{CheckpointNotification, Priority, StreamWorker};
use crate::model::checkpoint_request::StreamSource as CheckpointStreamSource;
use crate::operations::streams::sweep::DiscoveredSession;
use std::collections::HashSet;
use std::path::PathBuf;

impl StreamWorker {
    /// Revalidate a checkpoint stream against host authority before enqueueing it.
    pub(super) async fn handle_checkpoint_notification(
        &mut self,
        notification: CheckpointNotification,
    ) {
        let Some(agent) = crate::operations::streams::agent::get_agent(&notification.tool) else {
            tracing::warn!(tool = %notification.tool, "checkpoint stream agent is not supported");
            return;
        };
        let source = CheckpointStreamSource {
            path: notification.stream_path.clone(),
            format: notification.stream_format,
            session_id: notification.session_id.clone(),
            external_session_id: notification.external_session_id.clone(),
            external_parent_session_id: notification.external_parent_session_id.clone(),
        };
        let Ok(validated) = agent.validate_checkpoint_stream(&source) else {
            tracing::warn!(
                tool = %notification.tool,
                "checkpoint stream source changed or is no longer authorized"
            );
            return;
        };
        self.handle_validated_checkpoint_notification(notification, validated);
    }

    pub(super) fn handle_validated_checkpoint_notification(
        &mut self,
        mut notification: CheckpointNotification,
        validated: DiscoveredSession,
    ) {
        notification.session_id = validated.session_id;
        notification.stream_path = validated.stream_path.clone();
        notification.external_session_id = validated.external_session_id;
        notification.external_parent_session_id = validated.external_parent_session_id;
        let canonical_path = validated.stream_path;

        let mut enqueued: HashSet<(PathBuf, String)> = HashSet::new();
        let tasks = self.enqueue_streams_for_session(
            &notification.tool,
            &canonical_path,
            Priority::Immediate,
            Some(notification.trace_id.clone()),
            notification.tool_use_id.clone(),
            Some(notification.external_session_id.as_str()),
            notification.external_parent_session_id.as_deref(),
            notification.repo_work_dir.as_deref(),
            &notification.session_id,
            &mut enqueued,
        );

        for task in tasks {
            self.priority_queue.push(task);
        }

        if notification.tool == "claude" {
            self.sweep_subagents_for_session(&notification);
        }
    }
}
