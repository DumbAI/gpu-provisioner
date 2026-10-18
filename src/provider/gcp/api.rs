//! Minimal Compute Engine v1 REST client (reqwest): bearer auth, JSON
//! envelopes, pagination, and long-running-operation waits. Typed only as far
//! as the adapter reads responses.
//!
//! Auth deliberately avoids service-account JWT signing (which would pull in
//! RSA/JWT crates for a single-binary CLI): the token is a static one
//! (config or `$GOOGLE_OAUTH_ACCESS_TOKEN`) or minted by
//! `gcloud auth print-access-token`, cached for 45 minutes (tokens live 60).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use super::error::ApiError;

pub const DEFAULT_BASE_URL: &str = "https://compute.googleapis.com/compute/v1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
const TOKEN_TTL: Duration = Duration::from_secs(45 * 60);
/// `operations.wait` blocks server-side for up to ~2 minutes per call; a
/// handful of calls bounds an insert at ~8 minutes.
const OPERATION_WAIT_CALLS: usize = 4;

pub enum TokenSource {
    Static(String),
    Gcloud {
        cache: Mutex<Option<(String, Instant)>>,
    },
}

impl TokenSource {
    pub fn gcloud() -> Self {
        TokenSource::Gcloud {
            cache: Mutex::new(None),
        }
    }

    /// A bearer token; failures surface as a synthetic 401 (→ credentials).
    async fn token(&self) -> Result<String, ApiError> {
        match self {
            TokenSource::Static(token) => Ok(token.clone()),
            TokenSource::Gcloud { cache } => {
                if let Some((token, at)) = cache.lock().expect("token cache").clone() {
                    if at.elapsed() < TOKEN_TTL {
                        return Ok(token);
                    }
                }
                let token = gcloud_access_token().await.map_err(|message| ApiError {
                    status: 401,
                    reason: Some("tokenUnavailable".into()),
                    message,
                })?;
                *cache.lock().expect("token cache") = Some((token.clone(), Instant::now()));
                Ok(token)
            }
        }
    }
}

async fn gcloud_access_token() -> Result<String, String> {
    let out = tokio::process::Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .await
        .map_err(|e| format!("cannot run `gcloud auth print-access-token`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`gcloud auth print-access-token` failed (run `gcloud auth login`): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        return Err("`gcloud auth print-access-token` printed nothing".into());
    }
    Ok(token)
}

/// A long-running operation (`zoneOperations` / `globalOperations`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Operation {
    pub name: String,
    pub status: String,
    pub error: Option<OperationErrors>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OperationErrors {
    pub errors: Vec<OperationError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

impl Operation {
    pub fn is_done(&self) -> bool {
        self.status == "DONE"
    }

    pub fn first_error(&self) -> Option<&OperationError> {
        self.error.as_ref().and_then(|e| e.errors.first())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Instance {
    pub name: String,
    pub status: String,
    /// Zone URL.
    pub zone: String,
    pub network_interfaces: Vec<NetworkInterface>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetworkInterface {
    #[serde(rename = "networkIP")]
    pub network_ip: Option<String>,
    pub access_configs: Vec<AccessConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccessConfig {
    #[serde(rename = "natIP")]
    pub nat_ip: Option<String>,
}

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    pub project: String,
    tokens: TokenSource,
}

impl Api {
    pub fn new(base_url: &str, project: &str, tokens: TokenSource) -> Self {
        Api {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
            base_url: base_url.trim_end_matches('/').to_string(),
            project: project.to_string(),
            tokens,
        }
    }

    /// `projects/<p>/<path>` under the base URL.
    pub fn project_url(&self, path: &str) -> String {
        format!("{}/projects/{}/{path}", self.base_url, self.project)
    }

    /// Any resource path (`projects/x/global/images/family/y`) or full URL.
    pub fn resource_url(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("https://") || path_or_url.starts_with("http://") {
            path_or_url.to_string()
        } else {
            format!("{}/{}", self.base_url, path_or_url.trim_start_matches('/'))
        }
    }

    pub async fn send(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let token = self.tokens.token().await?;
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(token)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| ApiError::transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::from_response(status.as_u16(), &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| ApiError::transport(format!("invalid JSON from {url}: {e}")))
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let value = self.send(Method::GET, url, query, None).await?;
        decode(url, value)
    }

    pub async fn post<T: DeserializeOwned>(&self, url: &str, body: &Value) -> Result<T, ApiError> {
        let value = self.send(Method::POST, url, &[], Some(body)).await?;
        decode(url, value)
    }

    /// All pages of a list call, collecting `items` (an array).
    pub async fn list_all<T: DeserializeOwned>(
        &self,
        url: &str,
        filter: Option<&str>,
    ) -> Result<Vec<T>, ApiError> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(filter) = filter {
                query.push(("filter", filter.to_string()));
            }
            if let Some(token) = &page_token {
                query.push(("pageToken", token.clone()));
            }
            let page: Value = self.get(url, &query).await?;
            if let Some(array) = page.get("items").and_then(Value::as_array) {
                for item in array {
                    items.push(decode(url, item.clone())?);
                }
            }
            page_token = page
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                return Ok(items);
            }
        }
    }

    /// All pages of an `aggregated/*` call: `(scope, scoped object)` pairs,
    /// e.g. `("zones/us-central1-a", {"instances": [...]})`.
    pub async fn aggregated(
        &self,
        url: &str,
        filter: Option<&str>,
    ) -> Result<Vec<(String, Value)>, ApiError> {
        let mut scopes = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(filter) = filter {
                query.push(("filter", filter.to_string()));
            }
            if let Some(token) = &page_token {
                query.push(("pageToken", token.clone()));
            }
            let page: Value = self.get(url, &query).await?;
            if let Some(map) = page.get("items").and_then(Value::as_object) {
                scopes.extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            page_token = page
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                return Ok(scopes);
            }
        }
    }

    /// Wait for an operation via `operations.wait` (server-side blocking).
    /// Returns the last state seen; the caller decides what "not done" means.
    pub async fn wait_operation(&self, op: Operation, scope: &str) -> Result<Operation, ApiError> {
        let mut op = op;
        for _ in 0..OPERATION_WAIT_CALLS {
            if op.is_done() {
                break;
            }
            let url = self.project_url(&format!("{scope}/operations/{}/wait", op.name));
            let value = self.send(Method::POST, &url, &[], None).await?;
            op = decode(&url, value)?;
        }
        Ok(op)
    }
}

fn decode<T: DeserializeOwned>(url: &str, value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| ApiError::transport(format!("unexpected response from {url}: {e}")))
}
