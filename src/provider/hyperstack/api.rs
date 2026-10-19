//! Typed Hyperstack Infrahub API client (`https://infrahub-api.nexgencloud.com/v1`,
//! key sent in the `api_key` header). No retries: the shared core retries
//! `Transient` errors.
//!
//! Endpoints used: `GET /core/flavors`, `GET /core/stocks`, `GET /pricebook`,
//! `GET /core/images`, `GET|POST /core/environments`,
//! `GET|POST /core/keypairs`, `GET|POST /core/virtual-machines`,
//! `DELETE /core/virtual-machines/{id}`.

use std::collections::HashMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use super::error::{error_message, ApiError};

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Page size for paginated lists, and a cap so a misbehaving server cannot
/// keep us paging forever.
const PAGE_SIZE: u32 = 100;
const MAX_PAGES: u32 = 100;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Tolerate `null` for collections and strings the API documents as present.
fn de_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Label {
    #[serde(default, deserialize_with = "de_default")]
    pub label: String,
}

/// A flavor (`GET /core/flavors`): one GPU/CPU configuration in one region,
/// e.g. `n3-RTX-A6000x1` in `CANADA-1`.
#[derive(Debug, Clone, Deserialize)]
pub struct Flavor {
    pub name: String,
    /// GPU model as priced in the pricebook, e.g. `RTX-A6000`, `A100-80G-PCIe`;
    /// empty for CPU-only flavors.
    #[serde(default, deserialize_with = "de_default")]
    pub gpu: String,
    #[serde(default, deserialize_with = "de_default")]
    pub gpu_count: u32,
    #[serde(default, deserialize_with = "de_default")]
    pub cpu: u32,
    #[serde(default, deserialize_with = "de_default")]
    pub ram: f64,
    #[serde(default, deserialize_with = "de_default")]
    pub disk: u32,
    #[serde(default, deserialize_with = "de_default")]
    pub region_name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// Whether the region can launch this flavor right now (`None` when the
    /// API leaves it out).
    #[serde(default)]
    pub stock_available: Option<bool>,
    #[serde(default, deserialize_with = "de_default")]
    pub labels: Vec<Label>,
    /// Flavor → image compatibility; documented as an object but sometimes
    /// sent as a string, so kept raw and read by [`Flavor::hard_images`].
    #[serde(default)]
    pub image_restrictions: Option<serde_json::Value>,
}

impl Flavor {
    /// Images this flavor is *restricted* to (hard restrictions only).
    pub fn hard_images(&self) -> Vec<String> {
        let Some(restrictions) = self.image_restrictions.as_ref().filter(|v| v.is_object()) else {
            return Vec::new();
        };
        let restricted = restrictions
            .get("has_image_restrictions")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hard = restrictions
            .get("restriction_type")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t.eq_ignore_ascii_case("hard"));
        if !(restricted && hard) {
            return Vec::new();
        }
        restrictions
            .get("compatible_images")
            .and_then(|v| v.as_array())
            .map(|images| {
                images
                    .iter()
                    .filter_map(|i| i.get("image_name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlavorGroup {
    #[serde(default, deserialize_with = "de_default")]
    pub flavors: Vec<Flavor>,
}

#[derive(Debug, Clone, Deserialize)]
struct FlavorsResponse {
    #[serde(default, deserialize_with = "de_default")]
    data: Vec<FlavorGroup>,
}

/// Free units of one GPU model in one region (`GET /core/stocks`):
/// `configurations` maps `"1x"`, `"2x"`, `"4x"`, `"8x"`, … to counts.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelStock {
    #[serde(default, deserialize_with = "de_default")]
    pub model: String,
    #[serde(default, deserialize_with = "de_default")]
    pub configurations: HashMap<String, i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegionStock {
    #[serde(default, deserialize_with = "de_default")]
    pub region: String,
    #[serde(default, deserialize_with = "de_default")]
    pub models: Vec<ModelStock>,
}

#[derive(Debug, Clone, Deserialize)]
struct StocksResponse {
    #[serde(default, deserialize_with = "de_default")]
    stocks: Vec<RegionStock>,
}

/// One pricebook row: the hourly rate (USD) the account is billed for a
/// resource. GPU rows are per GPU per hour, named like the flavor's `gpu`.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceEntry {
    pub name: String,
    #[serde(default, deserialize_with = "de_default")]
    pub value: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Image {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageGroup {
    #[serde(default, deserialize_with = "de_default")]
    images: Vec<Image>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImagesResponse {
    #[serde(default, deserialize_with = "de_default")]
    images: Vec<ImageGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Environment {
    pub name: String,
    #[serde(default, deserialize_with = "de_default")]
    pub region: String,
}

#[derive(Debug, Clone, Deserialize)]
struct EnvironmentsResponse {
    #[serde(default, deserialize_with = "de_default")]
    environments: Vec<Environment>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NameRef {
    #[serde(default, deserialize_with = "de_default")]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Keypair {
    pub name: String,
    #[serde(default, deserialize_with = "de_default")]
    pub public_key: String,
    #[serde(default, deserialize_with = "de_default")]
    pub environment: NameRef,
}

#[derive(Debug, Clone, Deserialize)]
struct KeypairsResponse {
    #[serde(default, deserialize_with = "de_default")]
    keypairs: Vec<Keypair>,
}

/// A virtual machine (`GET /core/virtual-machines`), trimmed to what the
/// adapter reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Vm {
    pub id: i64,
    #[serde(default, deserialize_with = "de_default")]
    pub name: String,
    /// `CREATING`, `BUILD`, `ACTIVE`, `SHUTOFF`, `HIBERNATED`, `DELETING`, …
    #[serde(default, deserialize_with = "de_default")]
    pub status: String,
    #[serde(default)]
    pub floating_ip: Option<String>,
    #[serde(default)]
    pub fixed_ip: Option<String>,
    #[serde(default, deserialize_with = "de_default")]
    pub environment: NameRef,
    #[serde(default, deserialize_with = "de_default")]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VmsResponse {
    #[serde(default, deserialize_with = "de_default")]
    instances: Vec<Vm>,
}

/// One inbound rule in the create body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecurityRule {
    pub direction: String,
    pub ethertype: String,
    pub protocol: String,
    pub remote_ip_prefix: String,
    pub port_range_min: u16,
    pub port_range_max: u16,
}

impl SecurityRule {
    /// Allow inbound TCP on `port` from anywhere (IPv4).
    pub fn tcp_ingress(port: u16) -> Self {
        SecurityRule {
            direction: "ingress".into(),
            ethertype: "IPv4".into(),
            protocol: "tcp".into(),
            remote_ip_prefix: "0.0.0.0/0".into(),
            port_range_min: port,
            port_range_max: port,
        }
    }
}

/// `POST /core/virtual-machines` body (one VM).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateVm {
    pub name: String,
    pub environment_name: String,
    pub image_name: String,
    pub flavor_name: String,
    pub key_name: String,
    pub count: u32,
    pub assign_floating_ip: bool,
    /// Keep SSH on port 22 of the public IP.
    pub enable_port_randomization: bool,
    pub user_data: String,
    pub labels: Vec<String>,
    pub security_rules: Vec<SecurityRule>,
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

    /// Flavors, optionally limited to one region.
    pub async fn flavors(&self, region: Option<&str>) -> Result<Vec<Flavor>, ApiError> {
        let mut req = self.get("/core/flavors");
        if let Some(region) = region {
            req = req.query(&[("region", region)]);
        }
        let resp: FlavorsResponse = self.send(req).await?;
        Ok(resp.data.into_iter().flat_map(|g| g.flavors).collect())
    }

    pub async fn stocks(&self) -> Result<Vec<RegionStock>, ApiError> {
        let resp: StocksResponse = self.send(self.get("/core/stocks")).await?;
        Ok(resp.stocks)
    }

    pub async fn pricebook(&self) -> Result<Vec<PriceEntry>, ApiError> {
        self.send(self.get("/pricebook")).await
    }

    /// Public images offered in `region`.
    pub async fn images(&self, region: &str) -> Result<Vec<String>, ApiError> {
        let resp: ImagesResponse = self
            .send(
                self.get("/core/images")
                    .query(&[("region", region), ("include_public", "true")]),
            )
            .await?;
        Ok(resp
            .images
            .into_iter()
            .flat_map(|g| g.images)
            .map(|i| i.name)
            .collect())
    }

    pub async fn environments(&self) -> Result<Vec<Environment>, ApiError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let resp: EnvironmentsResponse = self
                .send(self.get("/core/environments").query(&page_query(page)))
                .await?;
            let n = resp.environments.len();
            all.extend(resp.environments);
            if n < PAGE_SIZE as usize {
                break;
            }
        }
        Ok(all)
    }

    pub async fn create_environment(&self, name: &str, region: &str) -> Result<(), ApiError> {
        self.send::<serde_json::Value>(
            self.http
                .post(self.url("/core/environments"))
                .json(&serde_json::json!({"name": name, "region": region})),
        )
        .await
        .map(|_| ())
    }

    pub async fn keypairs(&self) -> Result<Vec<Keypair>, ApiError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let resp: KeypairsResponse = self
                .send(self.get("/core/keypairs").query(&page_query(page)))
                .await?;
            let n = resp.keypairs.len();
            all.extend(resp.keypairs);
            if n < PAGE_SIZE as usize {
                break;
            }
        }
        Ok(all)
    }

    pub async fn import_keypair(
        &self,
        name: &str,
        environment: &str,
        public_key: &str,
    ) -> Result<(), ApiError> {
        self.send::<serde_json::Value>(self.http.post(self.url("/core/keypairs")).json(
            &serde_json::json!({
                "name": name,
                "environment_name": environment,
                "public_key": public_key,
            }),
        ))
        .await
        .map(|_| ())
    }

    /// Create one VM; returns its id.
    pub async fn create_vm(&self, body: &CreateVm) -> Result<i64, ApiError> {
        let resp: VmsResponse = self
            .send(
                self.http
                    .post(self.url("/core/virtual-machines"))
                    .json(body),
            )
            .await?;
        resp.instances
            .first()
            .map(|vm| vm.id)
            .ok_or_else(|| ApiError::Decode("create returned no virtual machine".into()))
    }

    /// VMs in `environment` whose name contains `search`.
    pub async fn vms(&self, environment: &str, search: &str) -> Result<Vec<Vm>, ApiError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut query = page_query(page);
            query.push(("environment", environment.to_string()));
            query.push(("exact_environment_match", "true".to_string()));
            query.push(("search", search.to_string()));
            let resp: VmsResponse = self
                .send(self.get("/core/virtual-machines").query(&query))
                .await?;
            let n = resp.instances.len();
            all.extend(resp.instances);
            if n < PAGE_SIZE as usize {
                break;
            }
        }
        Ok(all)
    }

    /// Delete a VM; an already-gone VM is success.
    pub async fn delete_vm(&self, id: &str) -> Result<(), ApiError> {
        match self
            .send::<serde_json::Value>(
                self.http
                    .delete(self.url(&format!("/core/virtual-machines/{}", path_segment(id)))),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path))
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let resp = req
            .header("api_key", &self.api_key)
            .header("accept", "application/json")
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

/// `page` / `pageSize` query pairs (the API takes both as strings or ints).
fn page_query(page: u32) -> Vec<(&'static str, String)> {
    vec![
        ("page", page.to_string()),
        ("pageSize", PAGE_SIZE.to_string()),
    ]
}

/// Percent-encode one URL path segment.
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

    fn flavor(value: serde_json::Value) -> Flavor {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn flavor_tolerates_nulls_and_missing_fields() {
        let f = flavor(serde_json::json!({
            "name": "n3-RTX-A6000x1", "gpu": "RTX-A6000", "gpu_count": 1,
            "region_name": "CANADA-1", "labels": null, "image_restrictions": "{}"
        }));
        assert_eq!(f.gpu_count, 1);
        assert_eq!(f.stock_available, None);
        assert!(f.labels.is_empty());
        assert!(f.hard_images().is_empty());
    }

    #[test]
    fn only_hard_image_restrictions_constrain_the_image() {
        let hard = flavor(serde_json::json!({
            "name": "n3-B300x8", "gpu": "B300", "gpu_count": 8,
            "image_restrictions": {
                "has_image_restrictions": true, "restriction_type": "hard",
                "compatible_images": [{"image_name": "Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker"}]
            }
        }));
        assert_eq!(
            hard.hard_images(),
            ["Ubuntu Server 24.04 LTS R595 CUDA 13.2 with Docker"]
        );
        let soft = flavor(serde_json::json!({
            "name": "x", "image_restrictions": {
                "has_image_restrictions": true, "restriction_type": "soft",
                "compatible_images": [{"image_name": "A"}]
            }
        }));
        assert!(soft.hard_images().is_empty());
    }

    #[test]
    fn create_body_serializes_documented_fields() {
        let body = CreateVm {
            name: "gpuprov-dev".into(),
            environment_name: "gpuprov-canada-1".into(),
            image_name: "img".into(),
            flavor_name: "n3-RTX-A6000x1".into(),
            key_name: "k".into(),
            count: 1,
            assign_floating_ip: true,
            enable_port_randomization: false,
            user_data: "#cloud-config\n".into(),
            labels: vec!["gpuprov-managed".into()],
            security_rules: vec![SecurityRule::tcp_ingress(22)],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["security_rules"][0]["port_range_min"], 22);
        assert_eq!(json["security_rules"][0]["direction"], "ingress");
        assert_eq!(json["enable_port_randomization"], false);
    }
}
