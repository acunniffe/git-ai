pub mod bundle;
pub mod cas;
pub mod client;
pub mod error;
pub mod logs;
pub mod metrics;
pub mod notes;
mod response;

pub use crate::model::api_types::*;
pub use client::{ApiClient, ApiContext};
pub use error::ApiError;
pub use logs::daemon_logs_upload_allowed;
pub use metrics::{metrics_upload_allowed, upload_metrics_with_retry};

pub(super) fn first_non_hex<'a>(values: &[&'a str]) -> Option<&'a str> {
    values
        .iter()
        .copied()
        .find(|value| !value.chars().all(|character| character.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::first_non_hex;

    #[test]
    fn hex_query_validation_accepts_existing_valid_inputs() {
        assert_eq!(first_non_hex(&["", "abc123", "ABCDEF"]), None);
    }

    #[test]
    fn hex_query_validation_returns_the_first_invalid_input() {
        assert_eq!(
            first_non_hex(&["abc123", "not-hex", "also-invalid"]),
            Some("not-hex")
        );
    }
}
