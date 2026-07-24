use crate::model::checkpoint_request::CheckpointRequest;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const CHECKPOINT_DELIVERY_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDelivery {
    pub schema_version: u16,
    pub delivery_id: String,
    pub batch_id: String,
    pub batch_ordinal: u32,
    pub captured_at_unix_ms: u64,
    pub producer_version: String,
    pub request: CheckpointRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointDeliveryError {
    UnsupportedSchema { found: u16, supported: u16 },
    EmptyIdentifier { field: &'static str },
    UnsafeIdentifier { field: &'static str },
    PathMustBeAbsolute { field: &'static str },
}

impl fmt::Display for CheckpointDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "checkpoint delivery schema {} is newer than supported schema {}",
                found, supported
            ),
            Self::EmptyIdentifier { field } => {
                write!(f, "checkpoint delivery {} must not be empty", field)
            }
            Self::UnsafeIdentifier { field } => {
                write!(
                    f,
                    "checkpoint delivery {} contains unsafe characters",
                    field
                )
            }
            Self::PathMustBeAbsolute { field } => {
                write!(f, "checkpoint delivery {} must be absolute", field)
            }
        }
    }
}

impl std::error::Error for CheckpointDeliveryError {}

impl CheckpointDelivery {
    pub fn from_requests(requests: Vec<CheckpointRequest>) -> Vec<Self> {
        let captured_at_unix_ms =
            u64::try_from(crate::model::clock::now_millis()).unwrap_or(u64::MAX);
        Self::from_requests_at(requests, captured_at_unix_ms)
    }

    pub fn from_requests_at(
        requests: Vec<CheckpointRequest>,
        captured_at_unix_ms: u64,
    ) -> Vec<Self> {
        let batch_id = crate::uuid::generate_v4();
        requests
            .into_iter()
            .enumerate()
            .map(|(batch_ordinal, request)| Self {
                schema_version: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
                delivery_id: crate::uuid::generate_v4(),
                batch_id: batch_id.clone(),
                batch_ordinal: u32::try_from(batch_ordinal).unwrap_or(u32::MAX),
                captured_at_unix_ms,
                producer_version: env!("CARGO_PKG_VERSION").to_string(),
                request,
            })
            .collect()
    }

    pub fn validate(&self) -> Result<(), CheckpointDeliveryError> {
        if self.schema_version != CHECKPOINT_DELIVERY_SCHEMA_VERSION {
            return Err(CheckpointDeliveryError::UnsupportedSchema {
                found: self.schema_version,
                supported: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
            });
        }
        validate_identifier("delivery_id", &self.delivery_id)?;
        validate_identifier("batch_id", &self.batch_id)?;
        if self.producer_version.is_empty() {
            return Err(CheckpointDeliveryError::EmptyIdentifier {
                field: "producer_version",
            });
        }
        for file in &self.request.files {
            if !file.path.is_absolute() {
                return Err(CheckpointDeliveryError::PathMustBeAbsolute { field: "file.path" });
            }
            if !file.repo_work_dir.is_absolute() {
                return Err(CheckpointDeliveryError::PathMustBeAbsolute {
                    field: "file.repo_work_dir",
                });
            }
        }
        if let Some(stream_source) = &self.request.stream_source
            && !stream_source.path.is_absolute()
        {
            return Err(CheckpointDeliveryError::PathMustBeAbsolute {
                field: "stream_source.path",
            });
        }
        Ok(())
    }

    pub fn captured_at_unix_ns(&self) -> u128 {
        u128::from(self.captured_at_unix_ms)
            .checked_mul(1_000_000)
            .expect("u64 milliseconds always fit in u128 nanoseconds")
    }
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), CheckpointDeliveryError> {
    if value.is_empty() {
        return Err(CheckpointDeliveryError::EmptyIdentifier { field });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(CheckpointDeliveryError::UnsafeIdentifier { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;
    use std::collections::HashMap;

    fn request(trace_id: &str) -> CheckpointRequest {
        CheckpointRequest {
            trace_id: trace_id.to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: Vec::new(),
            path_role: PreparedPathRole::Edited,
            stream_source: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn batch_assigns_stable_unique_delivery_identity() {
        let deliveries = CheckpointDelivery::from_requests_at(
            vec![request("a"), request("b")],
            1_725_000_123_456,
        );

        assert_eq!(deliveries.len(), 2);
        assert_eq!(
            deliveries[0].schema_version,
            CHECKPOINT_DELIVERY_SCHEMA_VERSION
        );
        assert_eq!(deliveries[0].batch_id, deliveries[1].batch_id);
        assert_ne!(deliveries[0].delivery_id, deliveries[1].delivery_id);
        assert_eq!(deliveries[0].batch_ordinal, 0);
        assert_eq!(deliveries[1].batch_ordinal, 1);
        assert_eq!(deliveries[0].captured_at_unix_ms, 1_725_000_123_456);
        assert!(!deliveries[0].producer_version.is_empty());
    }

    #[test]
    fn cbor_round_trip_preserves_delivery_id_and_request() {
        let delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1_725_000_123_456)
                .remove(0);
        let mut encoded = Vec::new();
        ciborium::into_writer(&delivery, &mut encoded).unwrap();
        let decoded: CheckpointDelivery = ciborium::from_reader(encoded.as_slice()).unwrap();

        decoded.validate().unwrap();
        assert_eq!(decoded.delivery_id, delivery.delivery_id);
        assert_eq!(decoded.request.trace_id, "trace-1");
    }

    #[test]
    fn forward_schema_is_rejected() {
        let mut delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1).remove(0);
        delivery.schema_version = CHECKPOINT_DELIVERY_SCHEMA_VERSION + 1;

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::UnsupportedSchema {
                found: CHECKPOINT_DELIVERY_SCHEMA_VERSION + 1,
                supported: CHECKPOINT_DELIVERY_SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn capture_milliseconds_convert_to_nanoseconds_explicitly() {
        let delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1_725_000_123_456)
                .remove(0);

        assert_eq!(delivery.captured_at_unix_ns(), 1_725_000_123_456_000_000);
    }

    #[test]
    fn unsafe_identifier_is_rejected_before_it_can_become_a_filename() {
        let mut delivery =
            CheckpointDelivery::from_requests_at(vec![request("trace-1")], 1).remove(0);
        delivery.delivery_id = "../escape".to_string();

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::UnsafeIdentifier {
                field: "delivery_id",
            })
        );
    }

    #[test]
    fn relative_snapshot_paths_are_rejected() {
        use crate::model::checkpoint_request::{BaseCommit, CheckpointFile};
        use std::path::PathBuf;

        let mut value = request("trace-1");
        value.files.push(CheckpointFile {
            path: PathBuf::from("relative.rs"),
            content: Some("content".to_string()),
            repo_work_dir: PathBuf::from("/repo"),
            base_commit: BaseCommit::Initial,
        });
        let delivery = CheckpointDelivery::from_requests_at(vec![value], 1).remove(0);

        assert_eq!(
            delivery.validate(),
            Err(CheckpointDeliveryError::PathMustBeAbsolute { field: "file.path" })
        );
    }
}
