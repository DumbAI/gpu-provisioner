//! Typed Prime Intellect REST client. It does not retry (the shared core
//! retries `Transient` errors) and it follows pagination on every listing.
//!
//! Endpoints used: `GET /api/v1/availability/gpus`, `GET|POST /api/v1/pods/`,
//! `DELETE /api/v1/pods/{id}`, `GET|POST /api/v1/ssh_keys/`.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::error::{error_message, ApiError};

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size for paginated listings (the API maximum).
pub const PAGE_SIZE: usize = 100;
/// Upper bound on pages fetched per listing — a guard against a server that
/// keeps returning full pages.
const MAX_PAGES: usize = 100;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `GET /api/v1/availability/gpus` envelope.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailabilityPage {
    #[serde(default)]
    pub items: Vec<GpuOffer>,
    #[serde(default)]
    pub total_count: usize,
}

/// One marketplace offer (`GpuAvailability`). Only the fields we use; the API
/// returns many more.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuOffer {
    pub cloud_id: String,
    pub gpu_type: String,
    pub socket: String,
    /// Upstream provider (`ProviderTypeEnum`, e.g. `hyperstack`).
    pub provider: String,
    pub gpu_count: u32,
    /// Prime Intellect region id (`united_states`, `canada`, ...).
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub data_center: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub stock_status: Option<String>,
    #[serde(default)]
    pub security: Option<String>,
    #[serde(default)]
    pub prices: Prices,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub is_spot: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prices {
    #[serde(default)]
    pub on_demand: Option<f64>,
}

/// `GenericPaginationResponse` envelope used by `/pods/` and `/ssh_keys/`.
#[derive(Debug, Clone, Deserialize)]
struct OffsetPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    total_count: usize,
}

/// A pod (`APIPodConfig`), trimmed to what the adapter reads.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    pub id: String,
    pub name: String,
    #[serde(default = "default_pod_status")]
    pub status: String,
    #[serde(default)]
    pub installation_failure: Option<String>,
    /// String, list of strings/nulls, or null.
    #[serde(default)]
    pub ssh_connection: Option<serde_json::Value>,
    /// String, list of strings/nulls, or null.
    #[serde(default)]
    pub ip: Option<serde_json::Value>,
}

fn default_pod_status() -> String {
    "PROVISIONING".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKey {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewSshKey<'a> {
    name: &'a str,
    public_key: &'a str,
}

/// `POST /api/v1/pods/` body.
#[derive(Debug, Clone, Serialize)]
pub struct CreatePod {
    pub pod: PodRequest,
    pub provider: ProviderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamConfig>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodRequest {
    pub name: String,
    pub cloud_id: String,
    pub gpu_type: String,
    pub socket: String,
    pub gpu_count: u32,
    pub disk_size: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_center_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
    pub ssh_key_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderType {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamConfig {
    pub team_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Created {
    id: String,
}

/// Filters for an availability query (all optional).
#[derive(Debug, Clone, Default)]
pub struct AvailabilityQuery {
    pub regions: Vec<String>,
    pub gpu_count: Option<u32>,
    pub cloud_id: Option<String>,
    pub data_center_id: Option<String>,
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

    /// Every offer matching `query`, across all pages.
    pub async fn availability(&self, query: &AvailabilityQuery) -> Result<Vec<GpuOffer>, ApiError> {
        let mut offers = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut params: Vec<(&str, String)> = vec![
                ("page", page.to_string()),
                ("page_size", PAGE_SIZE.to_string()),
            ];
            params.extend(query.regions.iter().map(|r| ("regions", r.clone())));
            if let Some(count) = query.gpu_count {
                params.push(("gpu_count", count.to_string()));
            }
            if let Some(cloud_id) = &query.cloud_id {
                params.push(("cloud_id", cloud_id.clone()));
            }
            if let Some(dc) = &query.data_center_id {
                params.push(("data_center_id", dc.clone()));
            }
            let resp: AvailabilityPage = self
                .send(
                    self.http
                        .get(self.url("/api/v1/availability/gpus"))
                        .query(&params),
                )
                .await?;
            let got = resp.items.len();
            offers.extend(resp.items);
            if got == 0 || offers.len() >= resp.total_count {
                break;
            }
        }
        Ok(offers)
    }

    /// Every pod on the account (offset pagination).
    pub async fn list_pods(&self) -> Result<Vec<Pod>, ApiError> {
        self.list_offset("/api/v1/pods/").await
    }

    pub async fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        self.list_offset("/api/v1/ssh_keys/").await
    }

    /// Register a public key; returns its id.
    pub async fn add_ssh_key(&self, name: &str, public_key: &str) -> Result<String, ApiError> {
        let created: Created = self
            .send(
                self.http
                    .post(self.url("/api/v1/ssh_keys/"))
                    .json(&NewSshKey { name, public_key }),
            )
            .await?;
        Ok(created.id)
    }

    /// Create a pod; returns its id.
    pub async fn create_pod(&self, body: &CreatePod) -> Result<String, ApiError> {
        let created: Created = self
            .send(self.http.post(self.url("/api/v1/pods/")).json(body))
            .await?;
        Ok(created.id)
    }

    /// Delete a pod. An unknown pod (404) is success: deletion is idempotent.
    pub async fn delete_pod(&self, pod_id: &str) -> Result<(), ApiError> {
        let result: Result<serde_json::Value, ApiError> = self
            .send(
                self.http
                    .delete(self.url(&format!("/api/v1/pods/{pod_id}"))),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn list_offset<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, ApiError> {
        let mut all: Vec<T> = Vec::new();
        for _ in 0..MAX_PAGES {
            let params = [
                ("offset", all.len().to_string()),
                ("limit", PAGE_SIZE.to_string()),
            ];
            let page: OffsetPage<T> = self
                .send(self.http.get(self.url(path)).query(&params))
                .await?;
            let got = page.data.len();
            all.extend(page.data);
            // Stop on a short page, or once the reported total is reached
            // (servers that omit `total_count` are paged until a short page).
            let reached_total = page.total_count > 0 && all.len() >= page.total_count;
            if got < PAGE_SIZE || reached_total {
                break;
            }
        }
        Ok(all)
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
        // Some endpoints (DELETE) may answer with an empty body.
        let body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }
}
