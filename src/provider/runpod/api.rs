//! Typed RunPod REST API v2 client. It does not retry (the shared core
//! retries `Transient` errors) and follows cursor pagination on listings.
//!
//! Endpoints used: `GET /v2/catalog/gpus[/{id}]` (with live availability),
//! `GET /v2/catalog/datacenters`, `GET /v2/account/ssh-keys` (read-only),
//! `GET|POST /v2/pods`, `DELETE /v2/pods/{id}`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::error::{error_message, ApiError};

/// Per-request timeout. Pod creation can take well over 10s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound on pages fetched per listing — a guard against a server that
/// keeps handing out cursors.
const MAX_PAGES: usize = 100;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Cloud tier of a pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Cloud {
    /// RunPod-owned data-center hardware.
    Secure,
    /// Community-hosted hardware.
    Community,
}

impl Cloud {
    pub const ALL: [Cloud; 2] = [Cloud::Secure, Cloud::Community];

    pub fn as_str(self) -> &'static str {
        match self {
            Cloud::Secure => "SECURE",
            Cloud::Community => "COMMUNITY",
        }
    }
}

/// Catalog stock level, ordered from none to plenty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Availability {
    None,
    Low,
    Medium,
    High,
}

/// Price per GPU per hour, by tier.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TierPrice {
    #[serde(default)]
    pub secure: f64,
    #[serde(default)]
    pub community: f64,
}

/// Largest GPU count per pod, by tier.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TierMaxCount {
    #[serde(default)]
    pub secure: u32,
    #[serde(default)]
    pub community: u32,
}

/// One data center's stock of a GPU type.
#[derive(Debug, Clone, Deserialize)]
pub struct DataCenterStock {
    pub id: String,
    pub availability: Availability,
}

/// A GPU type (`GpuType`), with availability when requested.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuType {
    /// Identifier used to create pods, e.g. `NVIDIA H100 80GB HBM3`.
    pub id: String,
    /// Display name, e.g. `H100 SXM`.
    #[serde(default)]
    pub name: String,
    /// VRAM in GB.
    #[serde(default)]
    pub memory: u32,
    #[serde(default)]
    pub price: TierPrice,
    #[serde(default)]
    pub max_count: TierMaxCount,
    /// Per-data-center stock (only with `include=AVAILABILITY`; omitted when
    /// the configuration is unavailable everywhere).
    #[serde(default)]
    pub data_centers: Vec<DataCenterStock>,
}

impl GpuType {
    pub fn price(&self, cloud: Cloud) -> f64 {
        match cloud {
            Cloud::Secure => self.price.secure,
            Cloud::Community => self.price.community,
        }
    }

    pub fn max_count(&self, cloud: Cloud) -> u32 {
        match cloud {
            Cloud::Secure => self.max_count.secure,
            Cloud::Community => self.max_count.community,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GpuTypes {
    #[serde(default)]
    gpus: Vec<GpuType>,
}

/// A data center (`DataCenter`) and its continental region.
#[derive(Debug, Clone, Deserialize)]
pub struct DataCenter {
    pub id: String,
    /// `NORTH_AMERICA`, `EUROPE`, …
    #[serde(default)]
    pub region: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataCenters {
    #[serde(default)]
    data_centers: Vec<DataCenter>,
}

#[derive(Debug, Clone, Deserialize)]
struct SshKeys {
    #[serde(default)]
    keys: Vec<String>,
}

/// One way to reach a pod over SSH.
#[derive(Debug, Clone, Deserialize)]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PodSsh {
    /// Straight to the pod's sshd over its published `22/tcp` mapping.
    #[serde(default)]
    pub direct: Option<SshEndpoint>,
}

/// A live port mapping of a running pod.
#[derive(Debug, Clone, Deserialize)]
pub struct RuntimePort {
    #[serde(default)]
    pub private: Option<u16>,
    #[serde(default)]
    pub public: Option<u16>,
    #[serde(default)]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PodRuntime {
    #[serde(default)]
    pub ports: Vec<RuntimePort>,
}

/// A pod, trimmed to what the adapter reads.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub data_center_id: Option<String>,
    #[serde(default)]
    pub ssh: Option<PodSsh>,
    #[serde(default)]
    pub runtime: Option<PodRuntime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    has_next_page: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct PodPage {
    #[serde(default)]
    pods: Vec<Pod>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

/// `POST /v2/pods` body.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePod {
    pub name: String,
    pub image: String,
    /// Container disk in GB.
    pub disk: i32,
    pub ports: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub start_ssh: bool,
    pub cloud: Cloud,
    pub data_center_ids: Vec<String>,
    pub gpu: GpuRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuRequest {
    pub id: String,
    pub count: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct Created {
    id: String,
}

/// Availability query for one tier.
#[derive(Debug, Clone)]
pub struct StockQuery<'a> {
    pub count: u32,
    pub cloud: Cloud,
    /// ISO 3166-1 alpha-2 codes; empty = everywhere.
    pub country_codes: &'a [String],
}

impl StockQuery<'_> {
    fn params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("include", "AVAILABILITY".to_string()),
            ("product", "POD".to_string()),
            ("count", self.count.to_string()),
            ("cloud", self.cloud.as_str().to_string()),
        ];
        if !self.country_codes.is_empty() {
            params.push(("countryCodes", self.country_codes.join(",")));
        }
        params
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Api {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every GPU type with its per-data-center stock for `query`.
    pub async fn gpu_types(&self, query: &StockQuery<'_>) -> Result<Vec<GpuType>, ApiError> {
        let resp: GpuTypes = self
            .send(
                self.http
                    .get(self.url("/v2/catalog/gpus"))
                    .query(&query.params()),
            )
            .await?;
        Ok(resp.gpus)
    }

    /// One GPU type's current stock (re-validation before create).
    pub async fn gpu_type(&self, id: &str, query: &StockQuery<'_>) -> Result<GpuType, ApiError> {
        self.send(
            self.http
                .get(self.url(&format!("/v2/catalog/gpus/{}", path_segment(id))))
                .query(&query.params()),
        )
        .await
    }

    pub async fn data_centers(&self) -> Result<Vec<DataCenter>, ApiError> {
        let resp: DataCenters = self
            .send(self.http.get(self.url("/v2/catalog/datacenters")))
            .await?;
        Ok(resp.data_centers)
    }

    /// The account's registered SSH public keys (read-only).
    pub async fn account_ssh_keys(&self) -> Result<Vec<String>, ApiError> {
        let resp: SshKeys = self
            .send(self.http.get(self.url("/v2/account/ssh-keys")))
            .await?;
        Ok(resp.keys)
    }

    /// Every pod on the account, following the cursor.
    pub async fn list_pods(&self) -> Result<Vec<Pod>, ApiError> {
        let mut pods = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut req = self.http.get(self.url("/v2/pods"));
            if let Some(cursor) = &cursor {
                req = req.query(&[("cursor", cursor)]);
            }
            let page: PodPage = self.send(req).await?;
            pods.extend(page.pods);
            match page.pagination {
                Some(Pagination {
                    next_cursor: Some(next),
                    has_next_page: true,
                }) => cursor = Some(next),
                _ => break,
            }
        }
        Ok(pods)
    }

    /// Create a pod; returns its id.
    pub async fn create_pod(&self, body: &CreatePod) -> Result<String, ApiError> {
        let created: Created = self
            .send(self.http.post(self.url("/v2/pods")).json(body))
            .await?;
        Ok(created.id)
    }

    /// Terminate a pod. An unknown pod (404) is success: deletion is
    /// idempotent.
    pub async fn delete_pod(&self, id: &str) -> Result<(), ApiError> {
        let result: Result<serde_json::Value, ApiError> = self
            .send(
                self.http
                    .delete(self.url(&format!("/v2/pods/{}", path_segment(id)))),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let resp = req
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ApiError::from_reqwest)?;
        let status = resp.status();
        let body = resp.text().await.map_err(ApiError::from_reqwest)?;
        if !status.is_success() {
            return Err(ApiError::Status {
                status: status.as_u16(),
                message: error_message(&body),
            });
        }
        // DELETE answers 204 with an empty body.
        let body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }
}

/// Percent-encode one URL path segment (GPU ids contain spaces).
pub fn path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(
            path_segment("NVIDIA H100 80GB HBM3"),
            "NVIDIA%20H100%2080GB%20HBM3"
        );
        assert_eq!(path_segment("a/b"), "a%2Fb");
        assert_eq!(path_segment("pod-1_x.y~z"), "pod-1_x.y~z");
    }

    #[test]
    fn availability_orders_from_none_to_high() {
        assert!(Availability::High > Availability::Medium);
        assert!(Availability::Low > Availability::None);
    }

    #[test]
    fn stock_query_params_are_comma_separated() {
        let countries = vec!["US".to_string(), "CA".to_string()];
        let params = StockQuery {
            count: 2,
            cloud: Cloud::Community,
            country_codes: &countries,
        }
        .params();
        assert!(params.contains(&("include", "AVAILABILITY".into())));
        assert!(params.contains(&("product", "POD".into())));
        assert!(params.contains(&("count", "2".into())));
        assert!(params.contains(&("cloud", "COMMUNITY".into())));
        assert!(params.contains(&("countryCodes", "US,CA".into())));
    }
}
