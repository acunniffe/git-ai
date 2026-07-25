use crate::model::checkpoint_delivery::CheckpointDelivery;
use crate::model::daemon_control::ControlResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveFallbackClass {
    Transport,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFallback {
    pub delivery_id: String,
    pub class: LiveFallbackClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationFailureClass {
    DurablePublication,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationFailure {
    pub delivery_id: String,
    pub class: PublicationFailureClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct CheckpointDeliveryReport {
    pub acknowledged: usize,
    pub published: usize,
    pub live_fallback: Option<LiveFallback>,
    pub publication_failures: Vec<PublicationFailure>,
}

/// Delivers an already-authorized checkpoint batch in capture order.
///
/// Live delivery stops at the first transport failure or negative
/// acknowledgement. That delivery and every later delivery are then published
/// individually to durable storage. Callback error values and daemon error
/// messages are deliberately discarded so the report cannot expose checkpoint
/// contents, paths, or session metadata.
pub fn deliver_checkpoint_batch<SendLive, PublishDurably, LiveError, PublicationError>(
    deliveries: &[CheckpointDelivery],
    mut send_live: SendLive,
    mut publish_durably: PublishDurably,
) -> CheckpointDeliveryReport
where
    SendLive: FnMut(&CheckpointDelivery) -> Result<ControlResponse, LiveError>,
    PublishDurably: FnMut(&CheckpointDelivery) -> Result<(), PublicationError>,
{
    let mut report = CheckpointDeliveryReport {
        acknowledged: 0,
        published: 0,
        live_fallback: None,
        publication_failures: Vec::new(),
    };
    let mut first_unacknowledged = deliveries.len();

    for (index, delivery) in deliveries.iter().enumerate() {
        match send_live(delivery) {
            Ok(response) if response.ok => {
                report.acknowledged += 1;
            }
            Ok(_) => {
                first_unacknowledged = index;
                report.live_fallback = Some(LiveFallback {
                    delivery_id: delivery.delivery_id.clone(),
                    class: LiveFallbackClass::Rejected,
                });
                break;
            }
            Err(_) => {
                first_unacknowledged = index;
                report.live_fallback = Some(LiveFallback {
                    delivery_id: delivery.delivery_id.clone(),
                    class: LiveFallbackClass::Transport,
                });
                break;
            }
        }
    }

    for delivery in &deliveries[first_unacknowledged..] {
        match publish_durably(delivery) {
            Ok(()) => report.published += 1,
            Err(_) => report.publication_failures.push(PublicationFailure {
                delivery_id: delivery.delivery_id.clone(),
                class: PublicationFailureClass::DurablePublication,
            }),
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;

    fn deliveries(ids: &[&str]) -> Vec<CheckpointDelivery> {
        let requests = ids
            .iter()
            .map(|id| CheckpointRequest {
                trace_id: format!("trace-{id}"),
                checkpoint_kind: CheckpointKind::Human,
                agent_id: None,
                files: Vec::new(),
                path_role: PreparedPathRole::Edited,
                stream_source: None,
                metadata: HashMap::new(),
            })
            .collect();
        let mut deliveries = CheckpointDelivery::from_requests_at(requests, 42);
        for (delivery, id) in deliveries.iter_mut().zip(ids) {
            delivery.delivery_id = (*id).to_string();
        }
        deliveries
    }

    #[test]
    fn checkpoint_delivery_batch_acknowledges_each_live_response_with_ok_true() {
        let deliveries = deliveries(&["one", "two", "three"]);
        let mut live_ids = Vec::new();
        let mut published_ids = Vec::new();

        let report = deliver_checkpoint_batch(
            &deliveries,
            |delivery| {
                live_ids.push(delivery.delivery_id.clone());
                Ok::<_, ()>(ControlResponse::ok(None, None))
            },
            |delivery| {
                published_ids.push(delivery.delivery_id.clone());
                Ok::<_, ()>(())
            },
        );

        assert_eq!(live_ids, ["one", "two", "three"]);
        assert!(published_ids.is_empty());
        assert_eq!(
            report,
            CheckpointDeliveryReport {
                acknowledged: 3,
                published: 0,
                live_fallback: None,
                publication_failures: Vec::new(),
            }
        );
    }

    #[test]
    fn checkpoint_delivery_batch_publishes_rejected_delivery_and_remaining_suffix() {
        let deliveries = deliveries(&["acknowledged", "rejected", "remaining"]);
        let mut live_ids = Vec::new();
        let mut published_ids = Vec::new();

        let report = deliver_checkpoint_batch(
            &deliveries,
            |delivery| {
                live_ids.push(delivery.delivery_id.clone());
                if delivery.delivery_id == "rejected" {
                    Ok::<_, ()>(ControlResponse::err("sensitive daemon response"))
                } else {
                    Ok(ControlResponse::ok(None, None))
                }
            },
            |delivery| {
                published_ids.push(delivery.delivery_id.clone());
                Ok::<_, ()>(())
            },
        );

        assert_eq!(live_ids, ["acknowledged", "rejected"]);
        assert_eq!(published_ids, ["rejected", "remaining"]);
        assert_eq!(report.acknowledged, 1);
        assert_eq!(report.published, 2);
        assert_eq!(
            report.live_fallback,
            Some(LiveFallback {
                delivery_id: "rejected".to_string(),
                class: LiveFallbackClass::Rejected,
            })
        );
        assert!(report.publication_failures.is_empty());
    }

    #[test]
    fn checkpoint_delivery_batch_publishes_current_and_remaining_after_transport_error() {
        let deliveries = deliveries(&["transport-error", "remaining"]);
        let mut live_ids = Vec::new();
        let mut published_ids = Vec::new();

        let report = deliver_checkpoint_batch(
            &deliveries,
            |delivery| {
                live_ids.push(delivery.delivery_id.clone());
                Err::<ControlResponse, _>("sensitive transport details")
            },
            |delivery| {
                published_ids.push(delivery.delivery_id.clone());
                Ok::<_, ()>(())
            },
        );

        assert_eq!(live_ids, ["transport-error"]);
        assert_eq!(published_ids, ["transport-error", "remaining"]);
        assert_eq!(report.acknowledged, 0);
        assert_eq!(report.published, 2);
        assert_eq!(
            report.live_fallback,
            Some(LiveFallback {
                delivery_id: "transport-error".to_string(),
                class: LiveFallbackClass::Transport,
            })
        );
    }

    #[test]
    fn checkpoint_delivery_batch_continues_after_individual_publication_failure() {
        let deliveries = deliveries(&["acknowledged", "fails", "published"]);
        let mut published_ids = Vec::new();

        let report = deliver_checkpoint_batch(
            &deliveries,
            |delivery| {
                if delivery.delivery_id == "acknowledged" {
                    Ok::<_, &str>(ControlResponse::ok(None, None))
                } else {
                    Err("live transport stopped")
                }
            },
            |delivery| {
                published_ids.push(delivery.delivery_id.clone());
                if delivery.delivery_id == "fails" {
                    Err("sensitive path and session details")
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(published_ids, ["fails", "published"]);
        assert_eq!(report.acknowledged, 1);
        assert_eq!(report.published, 1);
        assert_eq!(
            report.publication_failures,
            vec![PublicationFailure {
                delivery_id: "fails".to_string(),
                class: PublicationFailureClass::DurablePublication,
            }]
        );
        let rendered = format!("{report:?}");
        assert!(!rendered.contains("sensitive"));
        assert!(!rendered.contains("session"));
        assert!(!rendered.contains("path"));
    }

    #[test]
    fn checkpoint_delivery_batch_never_republishes_earlier_acknowledged_deliveries() {
        let deliveries = deliveries(&["first", "second", "third"]);
        let mut live_attempt = 0;
        let mut published_ids = Vec::new();

        let report = deliver_checkpoint_batch(
            &deliveries,
            |_| {
                live_attempt += 1;
                if live_attempt < 3 {
                    Ok::<_, ()>(ControlResponse::ok(None, None))
                } else {
                    Ok(ControlResponse::err("rejected"))
                }
            },
            |delivery| {
                published_ids.push(delivery.delivery_id.clone());
                Ok::<_, ()>(())
            },
        );

        assert_eq!(published_ids, ["third"]);
        assert_eq!(report.acknowledged, 2);
        assert_eq!(report.published, 1);
    }
}
