//! Copilot session-state model resolution.
//!
//! Legacy Copilot chat session state is a single JSON document whose
//! `requests` array grows with conversation length, so extraction streams it
//! through a bounded deserializer instead of materializing the whole
//! document: each request collapses into at most one model candidate as it
//! is read. Model-state edge cases: the latest request with model evidence
//! wins, `copilot/auto` resolves through the request's resolved model when
//! present, literal `unknown` is rejected, and the input state's selected
//! model is the fallback when no request carries evidence.

use crate::model::stream_types::StreamError;
use crate::operations::streams::model_extraction::normalize_model;
use serde::Deserialize;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
enum CopilotModelEvidence {
    Concrete(String),
    Auto,
}

impl CopilotModelEvidence {
    fn from_model(model: &str) -> Option<Self> {
        let model = normalize_model(model)?;
        if model.eq_ignore_ascii_case("copilot/auto") {
            Some(Self::Auto)
        } else if model.eq_ignore_ascii_case("unknown") {
            None
        } else {
            Some(Self::Concrete(model))
        }
    }

    fn into_model(self) -> String {
        match self {
            Self::Concrete(model) => model,
            Self::Auto => "copilot/auto".to_string(),
        }
    }
}

#[derive(Default)]
struct CopilotModelCandidates {
    latest_request: Option<CopilotModelEvidence>,
    selected: Option<CopilotModelEvidence>,
}

impl CopilotModelCandidates {
    fn record_request(&mut self, request: CopilotRequestModel) {
        let request_model = request
            .model_id
            .as_deref()
            .and_then(CopilotModelEvidence::from_model);
        let resolved_model = request
            .result
            .metadata
            .resolved_model
            .as_deref()
            .and_then(CopilotModelEvidence::from_model);

        let latest_request = match request_model {
            Some(CopilotModelEvidence::Auto) => resolved_model.or(Some(CopilotModelEvidence::Auto)),
            Some(model) => Some(model),
            None => resolved_model,
        };
        if latest_request.is_some() {
            self.latest_request = latest_request;
        }
    }

    fn record_selected(&mut self, model: Option<&str>) {
        if let Some(model) = model.and_then(CopilotModelEvidence::from_model) {
            self.selected = Some(model);
        }
    }

    fn best(self) -> Option<CopilotModelEvidence> {
        self.latest_request.or(self.selected)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotSessionState {
    input_state: CopilotInputState,
    requests: CopilotRequestCandidates,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotInputState {
    selected_model: CopilotSelectedModel,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotSelectedModel {
    identifier: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotRequestModel {
    model_id: Option<String>,
    result: CopilotRequestResult,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CopilotRequestResult {
    metadata: CopilotRequestMetadata,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CopilotRequestMetadata {
    resolved_model: Option<String>,
}

/// The `requests` array deserialized as a running fold: each request is read
/// into a small candidate struct and immediately collapsed, so the array's
/// size never affects memory.
#[derive(Default)]
struct CopilotRequestCandidates {
    latest: Option<CopilotModelEvidence>,
}

impl<'de> Deserialize<'de> for CopilotRequestCandidates {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RequestVisitor;

        impl<'de> serde::de::Visitor<'de> for RequestVisitor {
            type Value = CopilotRequestCandidates;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Copilot request array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut candidates = CopilotModelCandidates::default();
                while let Some(request) = sequence.next_element::<CopilotRequestModel>()? {
                    candidates.record_request(request);
                }
                Ok(CopilotRequestCandidates {
                    latest: candidates.latest_request,
                })
            }
        }

        deserializer.deserialize_seq(RequestVisitor)
    }
}

pub(crate) fn extract_model_from_copilot_session_json(
    path: &Path,
) -> Result<Option<String>, StreamError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };

    let mut candidates = CopilotModelCandidates::default();
    if let Ok(state) = serde_json::from_reader::<_, CopilotSessionState>(BufReader::new(file)) {
        candidates.record_selected(state.input_state.selected_model.identifier.as_deref());
        if state.requests.latest.is_some() {
            candidates.latest_request = state.requests.latest;
        }
    }

    Ok(candidates.best().map(CopilotModelEvidence::into_model))
}

#[cfg(test)]
mod tests {
    use crate::operations::streams::model_extraction::extract_model;
    use crate::operations::streams::sweep::StreamFormat;
    use std::io::Write;

    fn session_json_file(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn extract(content: &str) -> Option<String> {
        let file = session_json_file(content);
        extract_model(file.path(), StreamFormat::CopilotSessionJson, None).unwrap()
    }

    #[test]
    fn test_copilot_session_latest_request_model_wins() {
        let result = extract(
            r#"{"requests":[
                {"modelId":"copilot/gpt-4.1"},
                {"modelId":"copilot/claude-sonnet-4"}
            ]}"#,
        );
        assert_eq!(result, Some("copilot/claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_copilot_session_requests_without_model_keep_earlier_evidence() {
        let result = extract(
            r#"{"requests":[
                {"modelId":"copilot/claude-sonnet-4"},
                {"result":{"timings":{"totalElapsed":10}}}
            ]}"#,
        );
        assert_eq!(result, Some("copilot/claude-sonnet-4".to_string()));
    }

    #[test]
    fn test_copilot_session_auto_model_resolves_through_result_metadata() {
        let result = extract(
            r#"{"requests":[
                {"modelId":"copilot/auto","result":{"metadata":{"resolvedModel":"copilot/gpt-5-mini"}}}
            ]}"#,
        );
        assert_eq!(result, Some("copilot/gpt-5-mini".to_string()));

        let unresolved = extract(r#"{"requests":[{"modelId":"copilot/auto"}]}"#);
        assert_eq!(unresolved, Some("copilot/auto".to_string()));
    }

    #[test]
    fn test_copilot_session_rejects_unknown_model() {
        let result = extract(r#"{"requests":[{"modelId":"unknown"}]}"#);
        assert_eq!(result, None);
    }

    #[test]
    fn test_copilot_session_falls_back_to_selected_model() {
        let result = extract(
            r#"{"requests":[{"result":{}}],"inputState":{"selectedModel":{"identifier":"copilot/gpt-4"}}}"#,
        );
        assert_eq!(result, Some("copilot/gpt-4".to_string()));
    }

    #[test]
    fn test_copilot_session_streams_large_documents() {
        // A session whose per-request payloads dwarf the model fields: the
        // streaming deserializer must resolve the model without materializing
        // the document as a parsed value tree.
        let padding = "x".repeat(64 * 1024);
        let mut content = String::from(r#"{"requests":["#);
        for idx in 0..100 {
            if idx > 0 {
                content.push(',');
            }
            content.push_str(&format!(
                r#"{{"modelId":"copilot/model-{idx}","result":{{"metadata":{{"renderedUserMessage":"{padding}"}}}}}}"#
            ));
        }
        content.push_str("]}");
        assert!(content.len() > 6 * 1024 * 1024);

        let result = extract(&content);
        assert_eq!(result, Some("copilot/model-99".to_string()));
    }

    #[test]
    fn test_copilot_session_missing_and_malformed_files() {
        let missing = std::path::PathBuf::from("/nonexistent/session.json");
        let result = extract_model(&missing, StreamFormat::CopilotSessionJson, None).unwrap();
        assert_eq!(result, None);

        assert_eq!(extract("not json"), None);
        assert_eq!(extract(r#"{"requests":"not-an-array"}"#), None);
    }
}
