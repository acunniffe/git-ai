//! Daemon diagnostics upload API.

use crate::clients::api::client::ApiClient;
use crate::clients::api::metrics::metrics_upload_allowed;
use crate::clients::api::response::ResponseEnvelope;
use crate::error::GitAiError;
use crate::model::api_types::{DaemonLogsUploadRequest, DaemonLogsUploadResponse};

/// Returns whether daemon log uploads are allowed for the current API context.
///
/// This intentionally matches metrics delivery: the hosted API requires either
/// OAuth login or an API key, while custom API URLs are assumed to be deliberate.
pub fn daemon_logs_upload_allowed(api_base_url: &str, client: &ApiClient) -> bool {
    metrics_upload_allowed(api_base_url, client)
}

impl ApiClient {
    /// Upload a batch of daemon diagnostics to the server.
    pub fn upload_daemon_logs(
        &self,
        request: &DaemonLogsUploadRequest,
    ) -> Result<DaemonLogsUploadResponse, GitAiError> {
        let response = self.context().post_json("/worker/logs/upload", request)?;
        ResponseEnvelope::read(&response)?.decode_json(
            200,
            "daemon logs upload",
            "unexpected error",
        )
    }
}
