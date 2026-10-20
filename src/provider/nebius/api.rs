//! Typed client for the Nebius AI Cloud REST gateway
//! (`https://api.nebius.cloud`, HTTP/1.1 + JSON generated from the same
//! protobuf services as the gRPC API; OpenAPI at
//! <https://api.nebius.cloud/openapi.json>). Typed only as far as the adapter
//! reads responses; it does not retry — the shared core retries `Transient`
//! errors.
//!
//! JSON follows the protobuf JSON mapping: camelCase fields, enums as their
//! names, `int64` as decimal strings (accepted as numbers too on reads).
//! Lists page with `pageSize` / `pageToken` and end on an empty
//! `nextPageToken`.
//!
//! Auth is `Authorization: Bearer <IAM access token>`. The token is a static
//! one (config or `$NEBIUS_IAM_TOKEN`) or minted by
//! `nebius iam get-access-token`, which covers user and service-account CLI
//! profiles alike — so no RSA/JWT signing is needed here. CLI tokens live 12
//! hours and are cached for one.
//!
//! | endpoint | API reference |
//! |---|---|
//! | `GET /iam/v1/tenants` | [TenantService/List](https://api.nebius.cloud/openapi#/nebius.iam.v1.TenantService) |
//! | `GET /iam/v1/projects?parentId=` | [ProjectService/List](https://api.nebius.cloud/openapi#/nebius.iam.v1.ProjectService) |
//! | `GET /compute/v1/platforms?parentId=` | [PlatformService/List](https://docs.nebius.com/compute/virtual-machines/list-platforms) |
//! | `POST /billing/v1/calculator/estimate-batch` | [CalculatorService/EstimateBatch](https://api.nebius.cloud/openapi#/nebius.billing.v1.CalculatorService) |
//! | `GET /billing/v1alpha1/spot-price` | [SpotPriceService/List](https://docs.nebius.com/signup-billing/spot-prices) |
//! | `GET /capacity/v1/resource-advice?parentId=` | [ResourceAdviceService/List](https://docs.nebius.com/compute/virtual-machines/capacity-advisor) |
//! | `GET /vpc/v1/subnets?parentId=` | [SubnetService/List](https://api.nebius.cloud/openapi#/nebius.vpc.v1.SubnetService) |
//! | `POST /compute/v1/instances` | [InstanceService/Create](https://docs.nebius.com/compute/virtual-machines/create) |
//! | `GET /compute/v1/instances?parentId=` | [InstanceService/List](https://api.nebius.cloud/openapi#/nebius.compute.v1.InstanceService) |
//! | `GET /compute/v1/instances/operations/{id}` | [OperationService/Get](https://api.nebius.cloud/openapi#/nebius.compute.v1.InstanceService) |
//! | `DELETE /compute/v1/instances/{id}` | [InstanceService/Delete](https://docs.nebius.com/compute/virtual-machines/delete) |

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::error::{ApiError, Status};

pub const DEFAULT_BASE_URL: &str = "https://api.nebius.cloud";
/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Page size for list calls.
const PAGE_SIZE: &str = "100";
/// Safety stop for pagination.
const MAX_PAGES: usize = 100;
/// CLI tokens are valid for 12 hours; re-mint well before that.
const CLI_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

pub enum TokenSource {
    Static(String),
    /// `nebius [--profile P] iam get-access-token`, cached.
    Cli {
        profile: Option<String>,
        cache: Mutex<Option<(String, Instant)>>,
    },
}

impl TokenSource {
    pub fn cli(profile: Option<String>) -> Self {
        TokenSource::Cli {
            profile,
            cache: Mutex::new(None),
        }
    }

    async fn token(&self) -> Result<String, ApiError> {
        match self {
            TokenSource::Static(token) => Ok(token.clone()),
            TokenSource::Cli { profile, cache } => {
                if let Some((token, at)) = cache.lock().expect("token cache").clone() {
                    if at.elapsed() < CLI_TOKEN_TTL {
                        return Ok(token);
                    }
                }
                let token = cli_access_token(profile.as_deref())
                    .await
                    .map_err(ApiError::Token)?;
                *cache.lock().expect("token cache") = Some((token.clone(), Instant::now()));
                Ok(token)
            }
        }
    }
}

async fn cli_access_token(profile: Option<&str>) -> Result<String, String> {
    let mut command = tokio::process::Command::new("nebius");
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    let out = command
        .args(["iam", "get-access-token"])
        .output()
        .await
        .map_err(|e| format!("cannot run `nebius iam get-access-token`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`nebius iam get-access-token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        return Err("`nebius iam get-access-token` printed nothing".into());
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// An `int64` in protobuf JSON: a decimal string, or a plain number.
fn int64<'de, D: Deserializer<'de>>(de: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(i64),
        Text(String),
    }
    match Option::<Either>::deserialize(de)? {
        None => Ok(0),
        Some(Either::Number(n)) => Ok(n),
        Some(Either::Text(s)) => s.parse().map_err(serde::de::Error::custom),
    }
}

/// `nebius.common.v1.ResourceMetadata`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Metadata {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub parent_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// A tenant or project (`nebius.iam.v1.Container`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Container {
    pub metadata: Metadata,
    pub spec: ContainerSpec,
    pub status: ContainerStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerSpec {
    pub region: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContainerStatus {
    pub region: String,
    /// `ACTIVE`, `SCHEDULED_FOR_DELETION`, … (`nebius.iam.v1.State`).
    pub container_state: String,
}

impl Container {
    /// The project's region (`status.region`, else `spec.region`).
    pub fn region(&self) -> &str {
        if self.status.region.is_empty() {
            &self.spec.region
        } else {
            &self.status.region
        }
    }

    /// Usable: active, or a state the API did not report.
    pub fn is_active(&self) -> bool {
        matches!(
            self.status.container_state.as_str(),
            "" | "ACTIVE" | "STATE_UNSPECIFIED"
        )
    }
}

/// A VM platform (`nebius.compute.v1.Platform`), e.g. `gpu-h100-sxm`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Platform {
    pub metadata: Metadata,
    pub spec: PlatformSpec,
    pub status: PlatformStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlatformSpec {
    /// e.g. "NVIDIA® H100 NVLink with Intel Sapphire Rapids".
    pub human_readable_name: String,
    pub short_human_readable_name: String,
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlatformStatus {
    pub allowed_for_preemptibles: bool,
}

/// A size of a platform, e.g. `1gpu-16vcpu-200gb`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preset {
    pub name: String,
    pub resources: PresetResources,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PresetResources {
    pub gpu_count: u32,
    pub vcpu_count: u32,
    pub memory_gibibytes: u32,
}

/// `nebius.billing.v1alpha1.SpotPrice`: per GPU hour, per platform × region.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpotPrice {
    pub region: String,
    pub compute_instance: SpotPriceInstance,
    pub price: SpotPriceValue,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpotPriceInstance {
    pub platform: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpotPriceValue {
    /// Decimal string, e.g. "2.89".
    pub value: String,
    pub currency: String,
}

/// `nebius.capacity.v1.ResourceAdvice` (capacity advisor).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceAdvice {
    pub spec: AdviceSpec,
    pub status: AdviceStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdviceSpec {
    pub region: String,
    pub compute_instance: AdviceInstance,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdviceInstance {
    pub platform: String,
    pub preset: AdvicePreset,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdvicePreset {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AdviceStatus {
    pub on_demand: Availability,
    pub preemptible: Availability,
}

/// `ResourceAdviceStatus.Availability`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Availability {
    /// VMs that can be launched now (clipped by quota).
    #[serde(deserialize_with = "int64")]
    pub available: i64,
    /// `AVAILABILITY_LEVEL_HIGH` / `_MEDIUM` / `_LOW` / `_LIMIT_REACHED` / …
    pub availability_level: String,
    /// `DATA_STATE_FRESH` / `_STALE` / `_UNKNOWN`.
    pub data_state: String,
}

/// `nebius.vpc.v1.Subnet`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Subnet {
    pub metadata: Metadata,
    pub status: SubnetStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SubnetStatus {
    /// `CREATING`, `READY`, `DELETING`.
    pub state: String,
}

/// `nebius.compute.v1.Instance`, trimmed to what the adapter reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Instance {
    pub metadata: Metadata,
    pub spec: InstanceSpecView,
    pub status: InstanceStatus,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InstanceSpecView {
    pub resources: Resources,
    /// Present (possibly `{}`-like) on preemptible VMs.
    pub preemptible: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Resources {
    pub platform: String,
    pub preset: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InstanceStatus {
    /// `CREATING`, `UPDATING`, `STARTING`, `RUNNING`, `STOPPING`, `STOPPED`,
    /// `DELETING`, `ERROR`.
    pub state: String,
    /// An operation is in progress on the instance.
    pub reconciling: bool,
    pub network_interfaces: Vec<NetworkInterfaceStatus>,
    pub maintenance_event_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NetworkInterfaceStatus {
    pub ip_address: Option<AddressStatus>,
    pub public_ip_address: Option<AddressStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AddressStatus {
    /// CIDR notation, e.g. `203.0.113.5/32`.
    pub address: String,
}

/// `address` without its prefix length.
fn host(address: &str) -> Option<String> {
    let host = address.split('/').next().unwrap_or_default().trim();
    (!host.is_empty()).then(|| host.to_string())
}

impl Instance {
    pub fn public_ip(&self) -> Option<String> {
        self.status
            .network_interfaces
            .iter()
            .filter_map(|n| n.public_ip_address.as_ref())
            .find_map(|a| host(&a.address))
    }

    pub fn private_ip(&self) -> Option<String> {
        self.status
            .network_interfaces
            .iter()
            .filter_map(|n| n.ip_address.as_ref())
            .find_map(|a| host(&a.address))
    }

    pub fn is_preemptible(&self) -> bool {
        self.spec.preemptible.is_some()
    }
}

/// `nebius.common.v1.Operation`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Operation {
    pub id: String,
    pub resource_id: String,
    pub finished_at: Option<String>,
    /// `google.rpc.Status`, set once finished.
    pub status: Option<Value>,
}

impl Operation {
    pub fn is_done(&self) -> bool {
        self.finished_at.as_deref().is_some_and(|t| !t.is_empty())
    }

    /// The failure of a finished operation (`status.code` ≠ 0).
    pub fn error(&self) -> Option<ApiError> {
        let status = Status::from_value(self.status.as_ref()?);
        (status.code.unwrap_or(0) != 0).then(|| ApiError::Operation {
            id: self.id.clone(),
            error: status,
        })
    }
}

// --- Create request (`nebius.compute.v1.CreateInstanceRequest`) --------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateInstance {
    pub metadata: Metadata,
    pub spec: NewInstanceSpec,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewInstanceSpec {
    pub resources: Resources,
    pub boot_disk: AttachedDisk,
    pub network_interfaces: Vec<NetworkInterface>,
    pub cloud_init_user_data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preemptible: Option<Preemptible>,
    /// `FAIL` for preemptible VMs (the only policy they support).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<String>,
    /// `{}`: a preemptible VM pays the current spot price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follows_spot_price: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preemptible {
    /// `STOP` (the only supported value).
    pub on_preemption: String,
}

/// A VM-managed boot disk: created with the VM, deleted with it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedDisk {
    /// `READ_WRITE`.
    pub attach_mode: String,
    pub managed_disk: ManagedDisk,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedDisk {
    pub name: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    pub spec: DiskSpec,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpec {
    /// `NETWORK_SSD`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `int64` → decimal string in protobuf JSON.
    pub size_gibibytes: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_image_family: Option<ImageFamily>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_image_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageFamily {
    pub image_family: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    pub name: String,
    pub subnet_id: String,
    /// `{}`: a private address from the subnet.
    pub ip_address: Value,
    /// `{}`: a dynamic public address.
    pub public_ip_address: Value,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    token: TokenSource,
}

impl Api {
    pub fn new(base_url: &str, token: TokenSource) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Tenants the caller can see.
    pub async fn tenants(&self) -> Result<Vec<Container>, ApiError> {
        self.list("/iam/v1/tenants", &[]).await
    }

    /// Projects of a tenant (one per region by default).
    pub async fn projects(&self, tenant_id: &str) -> Result<Vec<Container>, ApiError> {
        self.list("/iam/v1/projects", &[("parentId", tenant_id)])
            .await
    }

    /// VM platforms (and their presets) available in a project.
    pub async fn platforms(&self, project_id: &str) -> Result<Vec<Platform>, ApiError> {
        self.list("/compute/v1/platforms", &[("parentId", project_id)])
            .await
    }

    /// Current spot prices of every preemptible GPU platform × region.
    pub async fn spot_prices(&self) -> Result<Vec<SpotPrice>, ApiError> {
        self.list("/billing/v1alpha1/spot-price", &[]).await
    }

    /// Capacity-advisor data for a tenant.
    pub async fn resource_advice(&self, tenant_id: &str) -> Result<Vec<ResourceAdvice>, ApiError> {
        self.list("/capacity/v1/resource-advice", &[("parentId", tenant_id)])
            .await
    }

    /// Hourly on-demand prices of `(key, project, platform, preset)`
    /// configurations, priced as instance creations in one batch. Returns
    /// `key → USD per hour` for every configuration the calculator priced;
    /// keys travel as the request's `metadata.name` and come back on each
    /// `resourceCosts[].metadata`.
    pub async fn hourly_prices(
        &self,
        configs: &[(String, String, String, String)],
    ) -> Result<BTreeMap<String, f64>, ApiError> {
        if configs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let resource_specs: Vec<Value> = configs
            .iter()
            .map(|(key, project, platform, preset)| {
                serde_json::json!({"spec": {
                    "@type": "type.googleapis.com/nebius.compute.v1.CreateInstanceRequest",
                    "metadata": {"parentId": project, "name": key},
                    "spec": {"resources": {"platform": platform, "preset": preset}},
                }})
            })
            .collect();
        let body = serde_json::json!({
            "resourceSpecs": resource_specs,
            "filterAggregationUnit": {"filterAggregationUnitValues": ["FILTER_AGGREGATION_UNIT_HOUR"]},
        });
        let resp: Value = self
            .send(
                self.http
                    .post(self.url("/billing/v1/calculator/estimate-batch"))
                    .json(&body),
            )
            .await?;
        Ok(parse_hourly_costs(&resp))
    }

    pub async fn subnets(&self, project_id: &str) -> Result<Vec<Subnet>, ApiError> {
        self.list("/vpc/v1/subnets", &[("parentId", project_id)])
            .await
    }

    /// Request an instance; returns the create operation.
    pub async fn create_instance(&self, body: &CreateInstance) -> Result<Operation, ApiError> {
        self.send(self.http.post(self.url("/compute/v1/instances")).json(body))
            .await
    }

    pub async fn instances(&self, project_id: &str) -> Result<Vec<Instance>, ApiError> {
        self.list("/compute/v1/instances", &[("parentId", project_id)])
            .await
    }

    pub async fn instance_operation(&self, id: &str) -> Result<Operation, ApiError> {
        self.send(self.http.get(self.url(&format!(
            "/compute/v1/instances/operations/{}",
            path_segment(id)
        ))))
        .await
    }

    /// Request deletion; an unknown instance (already gone) is success. The
    /// returned operation is not awaited.
    pub async fn delete_instance(&self, id: &str) -> Result<(), ApiError> {
        match self
            .send::<Value>(
                self.http
                    .delete(self.url(&format!("/compute/v1/instances/{}", path_segment(id)))),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every `items` entry across pages (`pageToken` / `nextPageToken`).
    async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        filter: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        let mut all = Vec::new();
        let mut token = String::new();
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, &str)> = vec![("pageSize", PAGE_SIZE)];
            query.extend_from_slice(filter);
            if !token.is_empty() {
                query.push(("pageToken", &token));
            }
            let mut body: Value = self
                .send(self.http.get(self.url(path)).query(&query))
                .await?;
            let items = body["items"].take();
            if !items.is_null() {
                let items: Vec<T> = serde_json::from_value(items)
                    .map_err(|e| ApiError::Decode(format!("{path} `items`: {e}")))?;
                all.extend(items);
            }
            match body["nextPageToken"].as_str() {
                Some(next) if !next.is_empty() => token = next.to_string(),
                _ => return Ok(all),
            }
        }
        Err(ApiError::Decode(format!(
            "{path} did not end after {MAX_PAGES} pages"
        )))
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let token = self.token.token().await?;
        let resp = req
            .bearer_auth(token)
            .send()
            .await
            .map_err(ApiError::from_reqwest)?;
        let status = resp.status();
        let body = resp.text().await.map_err(ApiError::from_reqwest)?;
        if !status.is_success() {
            return Err(ApiError::Http {
                status: status.as_u16(),
                error: Status::from_body(&body),
            });
        }
        let body = if body.trim().is_empty() { "{}" } else { &body };
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }
}

/// `resourceCosts[]` of an estimate → `metadata.name → hourly cost`. Costs
/// are decimal strings (`general.total.cost`); entries in a non-hourly unit
/// are skipped.
pub fn parse_hourly_costs(resp: &Value) -> BTreeMap<String, f64> {
    let mut prices = BTreeMap::new();
    for cost in resp["resourceCosts"].as_array().into_iter().flatten() {
        let unit = cost["aggregationUnit"]["unit"].as_str().unwrap_or("hour");
        if !unit.to_lowercase().starts_with("hour") {
            continue;
        }
        let name = cost["metadata"]["name"]
            .as_str()
            .or_else(|| cost["overrideMetadata"]["name"].as_str());
        let total = cost["general"]["total"]["cost"]
            .as_str()
            .or_else(|| cost["fixedInstance"]["perInstance"]["cost"].as_str());
        if let (Some(name), Some(Ok(price))) = (name, total.map(str::parse::<f64>)) {
            prices.insert(name.to_string(), price);
        }
    }
    prices
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
    use serde_json::json;

    use super::*;

    #[test]
    fn instance_addresses_drop_the_prefix_length() {
        let instance: Instance = serde_json::from_value(json!({
            "metadata": {"id": "computeinstance-e00a", "parentId": "project-e00b", "name": "gpuprov-dev",
                         "labels": {"gpuprov-vm-name": "dev"}, "resourceVersion": "3"},
            "spec": {"resources": {"platform": "gpu-h100-sxm", "preset": "1gpu-16vcpu-200gb"}},
            "status": {"state": "RUNNING", "reconciling": false, "networkInterfaces": [{
                "name": "eth0", "ipAddress": {"address": "192.168.0.5/32"},
                "publicIpAddress": {"address": "203.0.113.7/32", "static": false}
            }]}
        }))
        .unwrap();
        assert_eq!(instance.public_ip().as_deref(), Some("203.0.113.7"));
        assert_eq!(instance.private_ip().as_deref(), Some("192.168.0.5"));
        assert!(!instance.is_preemptible());
        assert_eq!(instance.metadata.labels["gpuprov-vm-name"], "dev");
    }

    #[test]
    fn create_body_follows_the_protobuf_json_mapping() {
        let body = CreateInstance {
            metadata: Metadata {
                parent_id: "project-e00b".into(),
                name: "gpuprov-dev".into(),
                labels: [("gpuprov-vm-name".to_string(), "dev".to_string())].into(),
                ..Metadata::default()
            },
            spec: NewInstanceSpec {
                resources: Resources {
                    platform: "gpu-h100-sxm".into(),
                    preset: "1gpu-16vcpu-200gb".into(),
                },
                boot_disk: AttachedDisk {
                    attach_mode: "READ_WRITE".into(),
                    managed_disk: ManagedDisk {
                        name: "gpuprov-dev-boot".into(),
                        labels: BTreeMap::new(),
                        spec: DiskSpec {
                            kind: "NETWORK_SSD".into(),
                            size_gibibytes: "100".into(),
                            source_image_family: Some(ImageFamily {
                                image_family: "ubuntu24.04-cuda13.0".into(),
                            }),
                            source_image_id: None,
                        },
                    },
                },
                network_interfaces: vec![NetworkInterface {
                    name: "eth0".into(),
                    subnet_id: "vpcsubnet-e00c".into(),
                    ip_address: json!({}),
                    public_ip_address: json!({}),
                }],
                cloud_init_user_data: "#cloud-config\n".into(),
                preemptible: Some(Preemptible {
                    on_preemption: "STOP".into(),
                }),
                recovery_policy: Some("FAIL".into()),
                follows_spot_price: Some(json!({})),
            },
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(
            value["metadata"],
            json!({"parentId": "project-e00b", "name": "gpuprov-dev", "labels": {"gpuprov-vm-name": "dev"}})
        );
        let disk = &value["spec"]["bootDisk"];
        assert_eq!(disk["attachMode"], "READ_WRITE");
        assert_eq!(disk["managedDisk"]["spec"]["type"], "NETWORK_SSD");
        assert_eq!(disk["managedDisk"]["spec"]["sizeGibibytes"], "100");
        assert_eq!(
            disk["managedDisk"]["spec"]["sourceImageFamily"]["imageFamily"],
            "ubuntu24.04-cuda13.0"
        );
        assert!(disk["managedDisk"]["spec"].get("sourceImageId").is_none());
        assert_eq!(
            value["spec"]["networkInterfaces"][0]["subnetId"],
            "vpcsubnet-e00c"
        );
        assert_eq!(
            value["spec"]["preemptible"],
            json!({"onPreemption": "STOP"})
        );
        assert_eq!(value["spec"]["recoveryPolicy"], "FAIL");
        assert_eq!(value["spec"]["followsSpotPrice"], json!({}));
        assert_eq!(value["spec"]["cloudInitUserData"], "#cloud-config\n");
    }

    #[test]
    fn operations_report_done_and_failure() {
        let running: Operation = serde_json::from_value(json!({
            "id": "computeoperation-e00x", "resourceId": "computeinstance-e00a",
            "createdAt": "2026-10-19T00:00:00Z"
        }))
        .unwrap();
        assert!(!running.is_done() && running.error().is_none());
        let ok: Operation = serde_json::from_value(json!({
            "id": "op", "resourceId": "computeinstance-e00a",
            "finishedAt": "2026-10-19T00:01:00Z", "status": {}
        }))
        .unwrap();
        assert!(ok.is_done() && ok.error().is_none());
        let failed: Operation = serde_json::from_value(json!({
            "id": "op", "finishedAt": "2026-10-19T00:01:00Z",
            "status": {"code": 8, "message": "Not enough resources"}
        }))
        .unwrap();
        assert!(matches!(failed.error(), Some(ApiError::Operation { .. })));
    }

    #[test]
    fn advice_counts_accept_strings_and_numbers() {
        let advice: ResourceAdvice = serde_json::from_value(json!({
            "spec": {"region": "eu-north1", "fabric": "fabric-2",
                     "computeInstance": {"platform": "gpu-h100-sxm",
                                         "preset": {"name": "1gpu-16vcpu-200gb"}}},
            "status": {
                "onDemand": {"available": "24", "limit": "32", "availabilityLevel": "AVAILABILITY_LEVEL_HIGH",
                             "dataState": "DATA_STATE_FRESH"},
                "preemptible": {"available": 0, "availabilityLevel": "AVAILABILITY_LEVEL_LIMIT_REACHED",
                                "dataState": "DATA_STATE_FRESH"}
            }
        }))
        .unwrap();
        assert_eq!(advice.status.on_demand.available, 24);
        assert_eq!(advice.status.preemptible.available, 0);
        assert_eq!(
            advice.spec.compute_instance.preset.name,
            "1gpu-16vcpu-200gb"
        );
    }

    #[test]
    fn hourly_costs_are_keyed_by_request_name() {
        let prices = parse_hourly_costs(&json!({
            "currency": "usd",
            "resourceCosts": [
                {"metadata": {"parentId": "project-e00b", "name": "a"},
                 "aggregationUnit": {"unit": "hour", "unitType": "UNIT_TYPE_TIME_BASED"},
                 "general": {"total": {"cost": "2.95", "costRounded": "2.95"}}},
                {"metadata": {"name": "b"},
                 "aggregationUnit": {"unit": "month", "unitType": "UNIT_TYPE_TIME_BASED"},
                 "general": {"total": {"cost": "2000"}}},
                {"metadata": {"name": "c"}, "general": {"total": {"cost": "not-a-number"}}}
            ]
        }));
        assert_eq!(prices.len(), 1);
        assert_eq!(prices["a"], 2.95);
    }
}
