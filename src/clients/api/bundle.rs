use crate::clients::api::client::ApiClient;
use crate::clients::api::response::ResponseEnvelope;
use crate::error::GitAiError;
use crate::model::api_types::{CreateBundleRequest, CreateBundleResponse};

/// Bundle API endpoints
impl ApiClient {
    /// Create a new bundle by posting to /api/bundle
    ///
    /// # Arguments
    /// * `request` - The bundle creation request
    ///
    /// # Returns
    /// * `Ok(CreateBundleResponse)` - Success response with bundle ID and URL
    /// * `Err(GitAiError)` - Error response
    ///
    /// # Errors
    /// * Returns `GitAiError::Api` for HTTP errors (use `ApiError::retryability()` to classify)
    /// * Returns `GitAiError::JsonError` for JSON parsing errors
    pub fn create_bundle(
        &self,
        request: CreateBundleRequest,
    ) -> Result<CreateBundleResponse, GitAiError> {
        let response = self.context().post_json("/api/bundles", &request)?;
        ResponseEnvelope::read(&response)?.decode_json(200, "bundle create", "unexpected error")
    }
}
