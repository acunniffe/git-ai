//! Metrics API endpoints

use crate::api::client::ApiClient;
use crate::api::types::ApiErrorResponse;
use crate::error::GitAiError;
use crate::metrics::MetricsBatch;
use crate::observability::log_error;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Retry delay in seconds: single retry after 60s
const RETRY_DELAYS_SECS: [u64; 1] = [60];
const METRICS_UPLOAD_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
static LAST_METRICS_UPLOAD_STARTED_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// Returns whether metrics are allowed to upload for the current API context.
///
/// Hosted Git AI Cloud (`usegitai.com`) is always blocked. Custom API URLs still
/// require authentication (API key or OAuth login).
pub fn metrics_upload_allowed(api_base_url: &str, client: &ApiClient) -> bool {
    if crate::http::is_git_ai_cloud_url(api_base_url) {
        return false;
    }
    client.is_logged_in() || client.has_api_key()
}

fn wait_for_metrics_upload_rate_limit() -> Result<(), GitAiError> {
    let limiter = LAST_METRICS_UPLOAD_STARTED_AT.get_or_init(|| Mutex::new(None));
    let mut last_started_at = limiter.lock().map_err(|_| {
        GitAiError::Generic("metrics upload rate limiter lock poisoned".to_string())
    })?;

    wait_for_metrics_upload_rate_limit_with(&mut last_started_at, Instant::now, std::thread::sleep);
    Ok(())
}

fn wait_for_metrics_upload_rate_limit_with<Now, Sleep>(
    last_started_at: &mut Option<Instant>,
    mut now: Now,
    mut sleep: Sleep,
) where
    Now: FnMut() -> Instant,
    Sleep: FnMut(Duration),
{
    let started_at = now();
    if let Some(previous_started_at) = *last_started_at {
        let elapsed = started_at.saturating_duration_since(previous_started_at);
        if let Some(remaining) = METRICS_UPLOAD_MIN_REQUEST_INTERVAL.checked_sub(elapsed)
            && !remaining.is_zero()
        {
            sleep(remaining);
        }
    }

    *last_started_at = Some(now());
}

/// Error for a single event in the batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadError {
    /// Index of the failed event in the request
    pub index: usize,
    /// Error message
    pub error: String,
}

/// Response from metrics upload endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadResponse {
    /// List of errors (only failed events, empty = all success)
    pub errors: Vec<MetricsUploadError>,
}

impl MetricsUploadResponse {
    /// Validate that all failed-event indices refer to events in this batch.
    pub fn validate_error_indices(&self, batch_size: usize) -> Result<(), GitAiError> {
        if let Some(error) = self.errors.iter().find(|error| error.index >= batch_size) {
            return Err(GitAiError::Generic(format!(
                "Metrics upload response error index {} is out of bounds for batch size {}",
                error.index, batch_size
            )));
        }
        Ok(())
    }

    /// Get indices of successfully uploaded events
    pub fn successful_indices(&self, batch_size: usize) -> Vec<usize> {
        let error_indices: std::collections::HashSet<_> =
            self.errors.iter().map(|e| e.index).collect();
        (0..batch_size)
            .filter(|i| !error_indices.contains(i))
            .collect()
    }
}

/// Upload metrics batch with retry logic.
///
/// Returns Ok(response) on success (200 response, even with partial errors).
/// Returns Err on failure after all retries exhausted.
///
/// Partial errors (200 + errors array) are logged to Sentry and returned so
/// callers can mark only the failed rows as permanently undeliverable.
pub fn upload_metrics_with_retry(
    client: &ApiClient,
    batch: &MetricsBatch,
    operation: &str,
) -> Result<MetricsUploadResponse, GitAiError> {
    // First attempt (no delay), then retry with delays
    for (attempt, delay_secs) in std::iter::once(&0u64)
        .chain(RETRY_DELAYS_SECS.iter())
        .enumerate()
    {
        if attempt > 0 {
            eprintln!(
                "[metrics] Retrying upload after {}s delay (attempt {}/{})",
                delay_secs,
                attempt + 1,
                RETRY_DELAYS_SECS.len() + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(*delay_secs));
        }

        match client.upload_metrics(batch) {
            Ok(response) => {
                // 200 response - log any validation errors to Sentry
                for error in &response.errors {
                    log_error(
                        &GitAiError::Generic(format!(
                            "Metrics {} error at index {}: {}",
                            operation, error.index, error.error
                        )),
                        Some(serde_json::json!({
                            "operation": operation,
                            "error_index": error.index
                        })),
                    );
                }
                return Ok(response);
            }
            Err(e) => {
                // Non-200 - will retry if attempts remain
                if attempt == RETRY_DELAYS_SECS.len() {
                    eprintln!("[metrics] All retries exhausted, giving up");
                    return Err(e);
                }
                eprintln!("[metrics] Upload failed: {}, will retry...", e);
            }
        }
    }

    Err(GitAiError::Generic(
        "All upload retries exhausted".to_string(),
    ))
}

/// Metrics API endpoints
impl ApiClient {
    /// Upload metrics batch to the server (max 1000 events)
    ///
    /// # Arguments
    /// * `batch` - The metrics batch to upload
    ///
    /// # Returns
    /// * `Ok(MetricsUploadResponse)` - Response with errors (empty = all success)
    /// * `Err(GitAiError)` - Request failed
    pub fn upload_metrics(
        &self,
        batch: &MetricsBatch,
    ) -> Result<MetricsUploadResponse, GitAiError> {
        wait_for_metrics_upload_rate_limit()?;
        let response = self.context().post_json("/worker/metrics/upload", batch)?;
        let status_code = response.status_code;

        let body = response
            .as_str()
            .map_err(|e| GitAiError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => {
                let metrics_response: MetricsUploadResponse =
                    serde_json::from_str(body).map_err(GitAiError::JsonError)?;
                Ok(metrics_response)
            }
            400 => {
                let error_response: ApiErrorResponse =
                    serde_json::from_str(body).unwrap_or_else(|_| ApiErrorResponse {
                        error: "Invalid request body".to_string(),
                        details: Some(serde_json::Value::String(body.to_string())),
                    });
                Err(GitAiError::Generic(format!(
                    "Bad Request: {}",
                    error_response.error
                )))
            }
            401 => Err(GitAiError::Generic("Unauthorized".to_string())),
            500 => {
                let error_response: ApiErrorResponse =
                    serde_json::from_str(body).unwrap_or_else(|_| ApiErrorResponse {
                        error: "Internal server error".to_string(),
                        details: None,
                    });
                Err(GitAiError::Generic(format!(
                    "Internal Server Error: {}",
                    error_response.error
                )))
            }
            _ => Err(GitAiError::Generic(format!(
                "Unexpected status code {}: {}",
                status_code, body
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn test_successful_indices() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 3,
                    error: "error".to_string(),
                },
            ],
        };

        let successful = response.successful_indices(5);
        assert_eq!(successful, vec![0, 2, 4]);
    }

    #[test]
    fn test_successful_indices_empty_errors() {
        let response = MetricsUploadResponse { errors: vec![] };
        let successful = response.successful_indices(3);
        assert_eq!(successful, vec![0, 1, 2]);
    }

    #[test]
    fn test_successful_indices_all_errors() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 0,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
            ],
        };
        let successful = response.successful_indices(2);
        assert!(successful.is_empty());
    }

    #[test]
    fn metrics_upload_rate_limiter_enforces_half_second_spacing() {
        let base = Instant::now();
        let current = Cell::new(base);
        let sleeps = RefCell::new(Vec::new());
        let mut last_started_at = None;

        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert!(sleeps.borrow().is_empty());
        assert_eq!(last_started_at, Some(base));

        current.set(base + Duration::from_millis(100));
        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert_eq!(&*sleeps.borrow(), &[Duration::from_millis(400)]);
        assert_eq!(last_started_at, Some(base + Duration::from_millis(500)));

        current.set(base + Duration::from_millis(1000));
        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert_eq!(sleeps.borrow().len(), 1);
        assert_eq!(last_started_at, Some(base + Duration::from_millis(1000)));
    }

    #[test]
    fn test_validate_error_indices_rejects_out_of_bounds_index() {
        let response = MetricsUploadResponse {
            errors: vec![MetricsUploadError {
                index: 2,
                error: "error".to_string(),
            }],
        };

        assert!(response.validate_error_indices(2).is_err());
    }
}
