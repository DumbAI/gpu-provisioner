//! Typed Massed Compute VM API client (`https://vm.massedcompute.com/api/v1`,
//! bearer token). It does not retry — the shared core retries `Transient`
//! errors.
//!
//! Endpoints used: `GET /gpu-inventory`, `GET /images`, `GET /instance`,
//! `POST /instance/launch`, `POST /instance/terminate`, `GET|POST /ssh-keys`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::error::{error_message, ApiError};

/// Per-request timeout. A launch request can take several seconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Hardware of a product.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Specs {
    #[serde(default)]
    pub vcpu_count: Option<u32>,
    #[serde(default)]
    pub memory_gib: Option<u32>,
    #[serde(default)]
    pub storage_gb: Option<u32>,
}

/// A deployable product (`gpu_1x_a6000`), as listed by the inventory.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceType {
    /// Product name passed to `launch` as `productName`, e.g. `gpu_1x_l40`.
    pub name: String,
    /// Human description, e.g. `1x RTX A6000`.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub price_cents_per_hour: Option<u64>,
    #[serde(default)]
    pub specs: Specs,
}

/// A region named in the inventory.
#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// One inventory entry: a product and where it currently has capacity.
#[derive(Debug, Clone, Deserialize)]
pub struct InventoryEntry {
    pub instance_type: InstanceType,
    #[serde(default)]
    pub regions_with_capacity_available: Vec<Region>,
    #[serde(default)]
    pub capacity_available: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct Inventory {
    #[serde(default)]
    gpu_inventory: BTreeMap<String, InventoryEntry>,
}

/// A deployable OS image / software stack.
#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub vm_image_id: i64,
    #[serde(default)]
    pub vm_image_name: String,
    #[serde(default)]
    pub vm_image_description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Images {
    #[serde(default)]
    images: Vec<Image>,
}

/// The product of a running instance.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Product {
    #[serde(default)]
    pub name: String,
}

/// A running instance, trimmed to what the adapter reads (the API also
/// returns a cleartext VM password, which is deliberately not modelled).
#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ip: Option<String>,
    /// Login user, e.g. `Ubuntu`.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub os_booted: Option<i64>,
    #[serde(default)]
    pub active: Option<i64>,
    #[serde(default)]
    pub product: Product,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunningInstances {
    #[serde(default)]
    running_instances: Vec<Instance>,
}

/// An SSH key registered on the account.
#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub public_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SshKeys {
    #[serde(default)]
    ssh_keys: Vec<SshKey>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewSshKey<'a> {
    name: &'a str,
    public_key: &'a str,
}

/// `POST /instance/launch` body.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    pub image_id: i64,
    pub product_name: String,
    /// Placement is chosen by Massed Compute: the API takes `any`.
    pub region_name: String,
    pub instance_name: String,
    /// Names of registered SSH keys to authorize.
    pub ssh_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Launched {
    response: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstanceUuids<'a> {
    instance_uuids: &'a [String],
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Api {
    pub fn new(base_url: &str, token: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every product with its current capacity.
    pub async fn inventory(&self) -> Result<Vec<InventoryEntry>, ApiError> {
        let resp: Inventory = self.send(self.http.get(self.url("/gpu-inventory"))).await?;
        Ok(resp.gpu_inventory.into_values().collect())
    }

    pub async fn images(&self) -> Result<Vec<Image>, ApiError> {
        let resp: Images = self.send(self.http.get(self.url("/images"))).await?;
        Ok(resp.images)
    }

    /// Every running instance on the account.
    pub async fn instances(&self) -> Result<Vec<Instance>, ApiError> {
        let resp: RunningInstances = self.send(self.http.get(self.url("/instance"))).await?;
        Ok(resp.running_instances)
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        let resp: SshKeys = self.send(self.http.get(self.url("/ssh-keys"))).await?;
        Ok(resp.ssh_keys)
    }

    pub async fn add_ssh_key(&self, name: &str, public_key: &str) -> Result<(), ApiError> {
        let _: serde_json::Value = self
            .send(
                self.http
                    .post(self.url("/ssh-keys"))
                    .json(&NewSshKey { name, public_key }),
            )
            .await?;
        Ok(())
    }

    /// Launch an instance; returns its uuid.
    pub async fn launch(&self, body: &LaunchRequest) -> Result<String, ApiError> {
        let launched: Launched = self
            .send(self.http.post(self.url("/instance/launch")).json(body))
            .await?;
        Ok(launched.response)
    }

    pub async fn terminate(&self, uuids: &[String]) -> Result<(), ApiError> {
        let _: serde_json::Value = self
            .send(
                self.http
                    .post(self.url("/instance/terminate"))
                    .json(&InstanceUuids {
                        instance_uuids: uuids,
                    }),
            )
            .await?;
        Ok(())
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let resp = req
            .bearer_auth(&self.token)
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
        let body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }
}
