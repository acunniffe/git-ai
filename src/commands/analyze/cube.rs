//! Minimal Cube API client for `git-ai analyze`.
//!
//! Talks to the Next.js cube proxy at `<api_base_url>/api/cube/*` using the
//! org API key in the `x-api-key` header. We build requests directly over
//! [`crate::http`] rather than going through `ApiContext::post_json` so that we
//! send ONLY the API key to the cube proxy — never a `git-ai login` OAuth
//! bearer, which the proxy does not expect on this route.

use crate::config;
use crate::http;
use serde_json::{Map, Value, json};

/// Cube can take a while to compile + run large queries; be generous.
const CUBE_TIMEOUT_SECS: u64 = 120;

/// A configured client pointed at one org's cube data (scoping is derived from
/// the API key by the proxy — we never pass an org id).
pub struct CubeClient {
    base_url: String,
    api_key: String,
    /// Built once and reused so successive cube calls share a connection pool
    /// (and skip rebuilding the native-TLS connector per request).
    agent: ureq::Agent,
}

#[derive(Debug)]
pub enum CubeError {
    /// No API key configured (env `GIT_AI_API_KEY` or config `api_key`).
    MissingApiKey,
    /// Transport-level failure (network, DNS, TLS).
    Transport(String),
    /// Auth/proxy error (401/403, "Path not supported", etc.).
    Http { status: u16, body: String },
    /// Cube returned an `error` field (e.g. `UserError`: bad measure/dimension).
    Cube(String),
    /// Response body was not valid JSON.
    Json(String),
}

impl std::fmt::Display for CubeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CubeError::MissingApiKey => write!(
                f,
                "no API key configured. Set GIT_AI_API_KEY or add \"api_key\" to \
                 ~/.git-ai/config.json (the key needs organization:admin:read; \
                 create one in your Git AI org settings)."
            ),
            CubeError::Transport(e) => write!(f, "request failed: {}", e),
            CubeError::Http { status, body } => {
                let hint = match status {
                    401 => " (unauthorized — check the API key)",
                    403 => " (forbidden — the key needs organization:admin:read)",
                    _ => "",
                };
                write!(f, "HTTP {}{}: {}", status, hint, body.trim())
            }
            CubeError::Cube(msg) => write!(f, "cube error: {}", msg),
            CubeError::Json(e) => write!(f, "invalid JSON response: {}", e),
        }
    }
}

impl CubeClient {
    /// Build a client from the resolved config. Returns `MissingApiKey` if no
    /// API key is available.
    pub fn from_config() -> Result<Self, CubeError> {
        let cfg = config::Config::fresh();
        let api_key = cfg
            .api_key()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .ok_or(CubeError::MissingApiKey)?;
        Ok(Self {
            base_url: cfg.api_base_url().trim_end_matches('/').to_string(),
            api_key,
            agent: http::build_agent(Some(CUBE_TIMEOUT_SECS)),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/cube/{}", self.base_url, path)
    }

    /// POST a JSON body to a cube path and parse the response.
    fn post(&self, path: &str, body: &Value) -> Result<Value, CubeError> {
        let request = self
            .agent
            .post(&self.url(path))
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json");
        let body_str = serde_json::to_string(body).map_err(|e| CubeError::Json(e.to_string()))?;
        let response = http::send_with_body(request, &body_str).map_err(CubeError::Transport)?;
        Self::parse(response)
    }

    /// GET a cube path and parse the response.
    fn get(&self, path: &str) -> Result<Value, CubeError> {
        let request = self
            .agent
            .get(&self.url(path))
            .header("x-api-key", &self.api_key);
        let response = http::send(request).map_err(CubeError::Transport)?;
        Self::parse(response)
    }

    fn parse(response: http::Response) -> Result<Value, CubeError> {
        let status = response.status_code;
        let text = response
            .as_str()
            .map_err(|e| CubeError::Json(e.to_string()))?
            .to_string();
        // Empty body with an error status: surface the status alone.
        let parsed: Value = if text.trim().is_empty() {
            if status >= 400 {
                return Err(CubeError::Http {
                    status,
                    body: String::new(),
                });
            }
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(|_| CubeError::Http {
                status,
                body: text.clone(),
            })?
        };
        // Auth/permission failures come from the proxy with a 401/403; surface
        // them as Http so the "check the API key / needs organization:admin:read"
        // hint is shown, even though the body also carries an `error` field.
        if status == 401 || status == 403 {
            let body = parsed
                .get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
                .unwrap_or(text);
            return Err(CubeError::Http { status, body });
        }
        // Cube signals query failures via an `error` field (often with HTTP 200);
        // surface it regardless of status so the agent sees the real message.
        if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
            let kind = parsed
                .get("type")
                .and_then(|t| t.as_str())
                .map(|t| format!("{}: ", t))
                .unwrap_or_default();
            return Err(CubeError::Cube(format!("{}{}", kind, err)));
        }
        if status >= 400 {
            return Err(CubeError::Http { status, body: text });
        }
        Ok(parsed)
    }

    /// Run a query against `/api/cube/load`. `query` is the inner Cube query
    /// object (we wrap it in `{"query": ...}`). Returns the full response value.
    pub fn load(&self, query: &Value) -> Result<Value, CubeError> {
        self.post("load", &json!({ "query": query }))
    }

    /// Run a query against `/api/cube/load` and return just the `data` rows.
    pub fn load_rows(&self, query: &Value) -> Result<Vec<Value>, CubeError> {
        let resp = self.load(query)?;
        Ok(resp
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// List cubes, measures, and dimensions (`/api/cube/meta`).
    pub fn meta(&self) -> Result<Value, CubeError> {
        self.get("meta")
    }
}

/// Structured inputs for building a Cube query from CLI flags.
#[derive(Debug, Default, Clone)]
pub struct QueryArgs {
    pub measures: Vec<String>,
    pub dimensions: Vec<String>,
    pub time_dimension: Option<String>,
    pub granularity: Option<String>,
    /// Cube `dateRange` (e.g. "last 30 days", "2024-01-01,2024-02-01").
    pub date_range: Option<String>,
    /// Raw JSON array of Cube filter objects (escape hatch for complex filters).
    pub filters_json: Option<String>,
    /// (member, direction) pairs.
    pub order: Vec<(String, String)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl QueryArgs {
    /// Build the inner Cube query object. Returns an error for invalid combos
    /// (e.g. a date range without a time dimension) or malformed `--filters`.
    pub fn to_query(&self) -> Result<Value, String> {
        let mut q = Map::new();

        if !self.measures.is_empty() {
            q.insert("measures".into(), json!(self.measures));
        }
        if !self.dimensions.is_empty() {
            q.insert("dimensions".into(), json!(self.dimensions));
        }

        match (&self.time_dimension, &self.date_range, &self.granularity) {
            (Some(dim), range, gran) => {
                let mut td = Map::new();
                td.insert("dimension".into(), json!(dim));
                if let Some(g) = gran {
                    td.insert("granularity".into(), json!(g));
                }
                if let Some(r) = range {
                    td.insert("dateRange".into(), json!(parse_date_range(r)));
                }
                q.insert("timeDimensions".into(), json!([Value::Object(td)]));
            }
            (None, Some(_), _) => {
                return Err("--since/--date-range requires --time-dimension <member>".to_string());
            }
            (None, None, Some(_)) => {
                return Err("--granularity requires --time-dimension <member>".to_string());
            }
            (None, None, None) => {}
        }

        if let Some(raw) = &self.filters_json {
            let parsed: Value = serde_json::from_str(raw)
                .map_err(|e| format!("--filters is not valid JSON: {}", e))?;
            if !parsed.is_array() {
                return Err("--filters must be a JSON array of filter objects".to_string());
            }
            q.insert("filters".into(), parsed);
        }

        if !self.order.is_empty() {
            let arr: Vec<Value> = self.order.iter().map(|(m, d)| json!([m, d])).collect();
            q.insert("order".into(), json!(arr));
        }
        if let Some(l) = self.limit {
            q.insert("limit".into(), json!(l));
        }
        if let Some(o) = self.offset {
            q.insert("offset".into(), json!(o));
        }

        Ok(Value::Object(q))
    }
}

/// Cube `dateRange` accepts relative strings ("last 30 days") as-is, or an
/// explicit `start,end` pair which we turn into a two-element array.
fn parse_date_range(raw: &str) -> Value {
    let trimmed = raw.trim();
    if let Some((start, end)) = trimmed.split_once(',') {
        let start = start.trim();
        let end = end.trim();
        if !start.is_empty() && !end.is_empty() {
            return json!([start, end]);
        }
    }
    json!(trimmed)
}

/// Render an array of cube data rows (objects of `member -> value`) as TSV.
/// Column order is the union of keys across rows, first-seen order preserved.
pub fn rows_to_tsv(rows: &[Value]) -> String {
    let mut columns: Vec<String> = Vec::new();
    for row in rows {
        if let Some(obj) = row.as_object() {
            for key in obj.keys() {
                if !columns.iter().any(|c| c == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let mut out = String::new();
    out.push_str(&columns.join("\t"));
    out.push('\n');
    for row in rows {
        let obj = row.as_object();
        let line: Vec<String> = columns
            .iter()
            .map(|col| {
                obj.and_then(|o| o.get(col))
                    .map(value_to_cell)
                    .unwrap_or_default()
            })
            .collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

/// Render a single JSON value as a flat TSV/table cell (strings unquoted).
fn value_to_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_basic_measure_query() {
        let args = QueryArgs {
            measures: vec!["public_v1_sessions.total_sessions".into()],
            ..Default::default()
        };
        let q = args.to_query().unwrap();
        assert_eq!(
            q,
            json!({ "measures": ["public_v1_sessions.total_sessions"] })
        );
    }

    #[test]
    fn builds_time_dimension_with_range_and_granularity() {
        let args = QueryArgs {
            measures: vec!["public_v1_pull_requests.ai_assisted_pull_requests".into()],
            time_dimension: Some("public_v1_pull_requests.opened_time".into()),
            granularity: Some("month".into()),
            date_range: Some("last 6 months".into()),
            ..Default::default()
        };
        let q = args.to_query().unwrap();
        assert_eq!(
            q["timeDimensions"],
            json!([{
                "dimension": "public_v1_pull_requests.opened_time",
                "granularity": "month",
                "dateRange": "last 6 months"
            }])
        );
    }

    #[test]
    fn explicit_date_range_pair_becomes_array() {
        let args = QueryArgs {
            time_dimension: Some("public_v1_sessions.session_start_time".into()),
            date_range: Some("2024-01-01, 2024-02-01".into()),
            ..Default::default()
        };
        let q = args.to_query().unwrap();
        assert_eq!(
            q["timeDimensions"][0]["dateRange"],
            json!(["2024-01-01", "2024-02-01"])
        );
    }

    #[test]
    fn date_range_without_time_dimension_errors() {
        let args = QueryArgs {
            date_range: Some("last 7 days".into()),
            ..Default::default()
        };
        assert!(args.to_query().is_err());
    }

    #[test]
    fn order_and_limit_and_filters() {
        let args = QueryArgs {
            measures: vec!["public_v1_sessions.total_sessions".into()],
            order: vec![("public_v1_sessions.total_sessions".into(), "desc".into())],
            limit: Some(25),
            filters_json: Some(
                r#"[{"member":"public_v1_sessions.agent","operator":"equals","values":["claude-code"]}]"#
                    .into(),
            ),
            ..Default::default()
        };
        let q = args.to_query().unwrap();
        assert_eq!(
            q["order"],
            json!([["public_v1_sessions.total_sessions", "desc"]])
        );
        assert_eq!(q["limit"], json!(25));
        assert_eq!(q["filters"][0]["operator"], json!("equals"));
    }

    #[test]
    fn invalid_filters_json_errors() {
        let args = QueryArgs {
            filters_json: Some("not json".into()),
            ..Default::default()
        };
        assert!(args.to_query().is_err());

        let args = QueryArgs {
            filters_json: Some(r#"{"not":"an array"}"#.into()),
            ..Default::default()
        };
        assert!(args.to_query().is_err());
    }

    #[test]
    fn tsv_uses_union_of_columns() {
        let rows = vec![json!({"a": "1", "b": "2"}), json!({"a": "3", "c": "4"})];
        let tsv = rows_to_tsv(&rows);
        let mut lines = tsv.lines();
        assert_eq!(lines.next().unwrap(), "a\tb\tc");
        assert_eq!(lines.next().unwrap(), "1\t2\t");
        assert_eq!(lines.next().unwrap(), "3\t\t4");
    }
}
