//! The cloud provider contract — the seam between the shared state machines
//! (`vm/`, `cluster/`) and cloud adapters: a trait plus the plain data types
//! (requests, offers, observations, errors) that cross it.
//!
//! Adapters expose small, idempotent primitives (`plan / prepare / create /
//! observe / delete / cleanup`) and classify their errors; all waiting,
//! retrying, failover and compensation lives in the shared core. Only code
//! below this module may talk to a cloud API. See
//! `doc/STATE_MACHINE_DESIGN.md` §4.1 for the per-primitive contract.

pub mod aws;
pub mod docker;
#[cfg(test)]
pub mod fake;
pub mod gcp;
pub mod primeintellect;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

/// Default cloud used when the spec doesn't name one.
pub const DEFAULT_PROVIDER: &str = aws::PROVIDER_ID;

/// Every provider id this build can construct (for help/error text).
pub const PROVIDER_IDS: &[&str] = &[
    aws::PROVIDER_ID,
    gcp::PROVIDER_ID,
    primeintellect::PROVIDER_ID,
    docker::PROVIDER_ID,
];

/// Construct the adapter for a provider id — the single registry point that a
/// new cloud is added to.
pub async fn build(
    name: &str,
    config: &AppConfig,
) -> Result<Box<dyn CloudProvider>, ProviderError> {
    match name {
        aws::PROVIDER_ID => Ok(Box::new(aws::AwsProvider::new(&config.aws).await)),
        gcp::PROVIDER_ID => Ok(Box::new(gcp::GcpProvider::new(&config.gcp)?)),
        primeintellect::PROVIDER_ID => Ok(Box::new(primeintellect::PrimeIntellectProvider::new(
            &config.primeintellect,
        )?)),
        docker::PROVIDER_ID => Ok(Box::new(docker::DockerProvider::new(&config.docker))),
        other => Err(ProviderError::invalid(format!(
            "unknown cloud provider '{other}' (available: {})",
            PROVIDER_IDS.join(", ")
        ))),
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A GPU ask parsed from the user's shorthand (`"L40S:1"`), cloud-agnostic.
/// How it maps to instance types is each provider's business — one GPU/count
/// can resolve to several types differing in CPU/memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRequest {
    /// Canonical uppercase GPU name, e.g. `"L40S"`.
    pub name: String,
    pub count: u32,
}

impl FromStr for GpuRequest {
    type Err = String;

    /// Parse `NAME` or `NAME:COUNT` (count defaults to 1).
    fn from_str(s: &str) -> Result<Self, String> {
        let malformed = || format!("expected NAME or NAME:COUNT (e.g. 'L40S:1'), got '{s}'");
        let mut parts = s.split(':');
        let name = parts
            .next()
            .filter(|n| !n.is_empty())
            .ok_or_else(malformed)?
            .to_uppercase();
        let count: u32 = match parts.next() {
            None => 1,
            Some(c) => c.parse().map_err(|_| malformed())?,
        };
        if parts.next().is_some() {
            return Err(malformed());
        }
        Ok(GpuRequest { name, count })
    }
}

impl fmt::Display for GpuRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.name, self.count)
    }
}

/// What to launch: an exact instance type, or a GPU shorthand the provider
/// resolves into instance types. The XOR is structural, so no downstream
/// validation is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceAsk {
    InstanceType(String),
    Gpus(GpuRequest),
}

/// The cloud-agnostic resource ask handed to a provider for planning.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    pub ask: ResourceAsk,
    /// Ordered region preference; region naming is cloud-specific. Empty is
    /// the provider's call: AWS/GCP reject it, Prime Intellect means "any".
    pub regions: Vec<String>,
    /// Optional zone pin; zone↔region conventions are cloud-specific.
    pub zone: Option<String>,
    /// Whether spot/preemptible capacity is wanted (filters marketplace offers).
    pub spot: bool,
}

/// Everything about the instance that does NOT vary per offer. Persisted in
/// the launch journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// Sanitized VM name: the cloud identity (tag/label/name) and DB key.
    pub vm_name: String,
    /// Machine-image override; `None` → the adapter picks one per offer.
    pub image_id: Option<String>,
    pub disk_gb: i32,
    pub spot: bool,
    /// Inbound ports to open besides SSH (22).
    pub open_ports: Vec<u16>,
    /// Cloud-init document creating `ssh_user` with the public key (clouds
    /// that boot with cloud-init).
    pub user_data: String,
    /// The same public key, for clouds that take keys via API or metadata
    /// (OpenSSH one-line format).
    pub ssh_public_key: String,
    /// Login user we ask for; clouds that dictate one report it in
    /// [`SshEndpoint::user`] instead.
    pub ssh_user: String,
}

// ---------------------------------------------------------------------------
// Plan / observe results
// ---------------------------------------------------------------------------

/// One launchable offer produced by `plan`: a concrete instance type at a
/// region (and optionally zone). Ordering encodes preference; the launch
/// machine walks the list in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    pub instance_type: String,
    pub region: String,
    /// `None` = any zone the provider finds usable in the region.
    pub zone: Option<String>,
    /// Hourly on-demand price when the provider knows it (display only).
    #[serde(default)]
    pub hourly_price: Option<f64>,
    /// Provider-private launch details (e.g. a marketplace offer's cloud id
    /// and socket). Persisted with the offer, so a resumed launch never has
    /// to re-plan; the core never interprets it.
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl Offer {
    pub fn new(instance_type: impl Into<String>, region: impl Into<String>) -> Self {
        Offer {
            instance_type: instance_type.into(),
            region: region.into(),
            zone: None,
            hourly_price: None,
            detail: serde_json::Value::Null,
        }
    }

    pub fn with_zone(mut self, zone: Option<String>) -> Self {
        self.zone = zone;
        self
    }

    pub fn location(&self) -> Location {
        Location {
            region: self.region.clone(),
            zone: self.zone.clone(),
        }
    }
}

impl fmt::Display for Offer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.instance_type, self.region)?;
        if let Some(zone) = &self.zone {
            write!(f, "/{zone}")?;
        }
        if let Some(price) = self.hourly_price {
            write!(f, " (${price:.2}/h)")?;
        }
        Ok(())
    }
}

/// Where a VM's resources live — the scope for `observe/delete/cleanup`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub region: String,
    pub zone: Option<String>,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.region)?;
        if let Some(zone) = &self.zone {
            write!(f, "/{zone}")?;
        }
        Ok(())
    }
}

/// How to reach a VM over SSH. Marketplace clouds dictate user and port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
    /// Login user the cloud dictates; `None` = the spec's `ssh_user`
    /// (created by our cloud-init / metadata).
    pub user: Option<String>,
}

/// Provider-normalized instance status (each adapter maps its native enum).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceStatus {
    Pending,
    Running,
    Stopping,
    Stopped,
    Terminating,
    /// The cloud reports the instance as broken (e.g. PI `ERROR`).
    Failed(String),
}

/// One live (not yet terminated) instance carrying the VM identity — the
/// `query_instances` + `get_cluster_info` analog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub instance_id: String,
    pub status: InstanceStatus,
    /// Concrete zone the instance landed in, when known.
    pub zone: Option<String>,
    pub private_ip: Option<String>,
    /// Set once the instance is reachable (public IP / SSH info assigned).
    pub endpoint: Option<SshEndpoint>,
}

impl Observed {
    /// Booted from the core's point of view: running AND reachable.
    pub fn is_ready(&self) -> bool {
        self.status == InstanceStatus::Running && self.endpoint.is_some()
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Optional launch features a cloud may not implement. Requesting one from a provider
/// that lists it as unsupported fails before planning instead of being
/// silently dropped mid-launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Machine-image override (`ami` / `image`).
    ImageOverride,
    /// Spot / preemptible capacity.
    Spot,
    /// Inbound ports beyond SSH.
    OpenPorts,
    /// Hosting a k3s cluster (needs a systemd-managed machine).
    Kubernetes,
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Feature::ImageOverride => "custom image",
            Feature::Spot => "spot instances",
            Feature::OpenPorts => "extra open ports",
            Feature::Kubernetes => "Kubernetes (k3s)",
        })
    }
}

/// What a provider can do and how its instances behave over time. The
/// timing hints parameterize the shared core's polling and deadlines.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub unsupported: &'static [(Feature, &'static str)],
    /// Whether instances run `InstanceSpec::user_data` (cloud-init) at boot.
    /// Enables boot-time workload delivery (the k3s installer); without it,
    /// workloads are pushed over SSH after the VM is up.
    pub cloud_init: bool,
    /// Max time from `create` until the instance is running + reachable.
    pub boot_timeout: Duration,
    /// Max time from `delete` until the instances are gone.
    pub delete_timeout: Duration,
    /// Interval between `observe` polls.
    pub poll_interval: Duration,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            unsupported: &[],
            cloud_init: false,
            boot_timeout: Duration::from_secs(600),
            delete_timeout: Duration::from_secs(600),
            poll_interval: Duration::from_secs(5),
        }
    }
}

impl Capabilities {
    pub fn supports(&self, feature: Feature) -> bool {
        !self.unsupported.iter().any(|(f, _)| *f == feature)
    }

    /// Reject a launch that needs features the provider cannot honor, listing
    /// every offending feature with its reason.
    pub fn check(&self, provider: &str, required: &[Feature]) -> Result<(), ProviderError> {
        let missing: Vec<String> = self
            .unsupported
            .iter()
            .filter(|(feature, _)| required.contains(feature))
            .map(|(feature, reason)| format!("{feature} ({reason})"))
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(ProviderError::invalid(format!(
                "{provider} does not support: {}",
                missing.join("; ")
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// How the core reacts to a provider error:
///
/// | kind             | core action                                   |
/// |------------------|-----------------------------------------------|
/// | `Transient`      | retry the same step with backoff (budgeted)   |
/// | `Capacity`       | compensate, block the offer, next offer       |
/// | `Quota`          | compensate, block the whole region, next      |
/// | `Unavailable`    | block the offer, next — nothing was created   |
/// | `Failed`         | compensate, block the offer, next             |
/// | `InvalidRequest` | abort — the ask itself is unplannable         |
/// | `Credentials`    | abort                                         |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    Transient,
    Capacity,
    Quota,
    Unavailable,
    Failed,
    InvalidRequest,
    Credentials,
}

impl ErrorKind {
    /// Errors that end the whole launch rather than the current offer.
    pub fn is_fatal(self) -> bool {
        matches!(self, ErrorKind::InvalidRequest | ErrorKind::Credentials)
    }
}

/// What a failed offer blocks for the rest of the search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockScope {
    /// Just this offer (type + region + zone + provider detail).
    Offer,
    /// This instance type anywhere in the region.
    TypeInRegion,
    /// Every type in the region (quota).
    Region,
}

impl ErrorKind {
    pub fn default_block_scope(self) -> BlockScope {
        match self {
            ErrorKind::Quota => BlockScope::Region,
            _ => BlockScope::Offer,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {msg}")]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub msg: String,
    /// Override of the kind's default block scope.
    pub block: Option<BlockScope>,
}

impl ProviderError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        ProviderError {
            kind,
            msg: msg.into(),
            block: None,
        }
    }

    pub fn transient(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transient, msg)
    }
    pub fn capacity(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Capacity, msg)
    }
    pub fn quota(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Quota, msg)
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, msg)
    }
    pub fn failed(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failed, msg)
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, msg)
    }
    pub fn credentials(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Credentials, msg)
    }

    pub fn with_block(mut self, scope: BlockScope) -> Self {
        self.block = Some(scope);
        self
    }

    pub fn block_scope(&self) -> BlockScope {
        self.block
            .unwrap_or_else(|| self.kind.default_block_scope())
    }
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Uniform per-cloud primitives. Each must be idempotent and must not loop or
/// sleep for long (beyond one API call's own pagination) — the shared core
/// owns waiting, retries, failover and compensation.
#[async_trait]
pub trait CloudProvider: Send + Sync {
    /// Canonical lowercase provider id (e.g. `"aws"`) — recorded on VM rows
    /// so teardown dispatches to the adapter that owns a VM.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Resolve a resource request into ordered offers.
    /// Creates nothing.
    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError>;

    /// Idempotent per-(VM, location) prerequisites — security group, firewall
    /// rule, key registration, image resolution. Returns provider-private
    /// state handed back to `create` (persisted in the journal).
    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError>;

    /// One creation attempt, labeled with the VM identity. Returns the new
    /// instance id. Never waits for running.
    async fn create(
        &self,
        offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError>;

    /// All non-terminated instances carrying the VM identity at `at`.
    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError>;

    /// Request deletion (idempotent; unknown ids are success). Never waits.
    async fn delete(
        &self,
        at: &Location,
        vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError>;

    /// Remove per-VM side resources once instances are gone (idempotent).
    async fn cleanup(&self, at: &Location, vm_name: &str) -> Result<(), ProviderError>;
}

/// Cross a provider's resolved instance-type ladder (cheapest first) with the
/// candidate regions — type-major, so the cheapest type is tried in every
/// region before the next size up. A zone pin selects its owning region via
/// the `region`-is-a-prefix-of-`zone` convention (holds for AWS and GCP).
pub fn expand_offers(
    instance_types: &[String],
    regions: &[String],
    zone: Option<&str>,
) -> Result<Vec<Offer>, ProviderError> {
    if regions.is_empty() {
        return Err(ProviderError::invalid(
            "no candidate regions: pass --region (repeatable) or set default regions in the \
             config file",
        ));
    }
    let (regions, zone): (Vec<String>, Option<String>) = match zone {
        None => (regions.to_vec(), None),
        Some(zone) => match regions.iter().find(|r| zone.starts_with(r.as_str())) {
            Some(region) => (vec![region.clone()], Some(zone.to_string())),
            None => {
                return Err(ProviderError::invalid(format!(
                    "zone '{zone}' does not belong to any candidate region ({})",
                    regions.join(", ")
                )))
            }
        },
    };
    let mut offers = Vec::with_capacity(instance_types.len() * regions.len());
    for instance_type in instance_types {
        for region in &regions {
            offers.push(Offer::new(instance_type, region).with_zone(zone.clone()));
        }
    }
    Ok(offers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_request_parses_shorthand() {
        let gpu: GpuRequest = "l40s".parse().unwrap();
        assert_eq!(gpu.name, "L40S");
        assert_eq!(gpu.count, 1);

        let gpu: GpuRequest = "A10G:4".parse().unwrap();
        assert_eq!(gpu.name, "A10G");
        assert_eq!(gpu.count, 4);
        assert_eq!(gpu.to_string(), "A10G:4");

        assert!("".parse::<GpuRequest>().is_err());
        assert!("L40S:x".parse::<GpuRequest>().is_err());
        assert!("L40S:1:2".parse::<GpuRequest>().is_err());
    }

    #[test]
    fn offer_display_includes_zone_and_price() {
        let mut offer = Offer::new("g6e.8xlarge", "us-east-2");
        assert_eq!(offer.to_string(), "g6e.8xlarge@us-east-2");
        offer.zone = Some("us-east-2a".into());
        offer.hourly_price = Some(1.5);
        assert_eq!(
            offer.to_string(),
            "g6e.8xlarge@us-east-2/us-east-2a ($1.50/h)"
        );
    }

    #[test]
    fn offer_round_trips_through_json_with_detail() {
        let mut offer = Offer::new("hyperstack/n3-H100x1", "canada");
        offer.detail = serde_json::json!({"cloudId": "n3-H100x1", "socket": "PCIe"});
        let back: Offer = serde_json::from_str(&serde_json::to_string(&offer).unwrap()).unwrap();
        assert_eq!(back, offer);
    }

    fn types(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expand_is_type_major_over_regions() {
        let rendered: Vec<String> = expand_offers(
            &types(&["g6e.12xlarge", "g6e.24xlarge"]),
            &types(&["us-east-2", "us-west-2"]),
            None,
        )
        .unwrap()
        .iter()
        .map(|c| c.to_string())
        .collect();
        assert_eq!(
            rendered,
            vec![
                "g6e.12xlarge@us-east-2",
                "g6e.12xlarge@us-west-2",
                "g6e.24xlarge@us-east-2",
                "g6e.24xlarge@us-west-2",
            ]
        );
    }

    #[test]
    fn expand_zone_pin_restricts_to_owning_region() {
        let offers = expand_offers(
            &types(&["t3.micro"]),
            &types(&["us-west-2", "us-east-1"]),
            Some("us-east-1b"),
        )
        .unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].region, "us-east-1");
        assert_eq!(offers[0].zone.as_deref(), Some("us-east-1b"));
    }

    #[test]
    fn expand_rejects_empty_regions_and_orphan_zone() {
        let err = expand_offers(&types(&["t3.micro"]), &[], None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(err.msg.contains("--region"));
        let err = expand_offers(
            &types(&["t3.micro"]),
            &types(&["us-west-2"]),
            Some("eu-west-1a"),
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn capabilities_check_lists_every_missing_feature() {
        let caps = Capabilities {
            unsupported: &[
                (Feature::OpenPorts, "no firewall API"),
                (Feature::Spot, "on-demand only"),
            ],
            ..Default::default()
        };
        assert!(caps.check("x", &[Feature::ImageOverride]).is_ok());
        let err = caps
            .check("x", &[Feature::OpenPorts, Feature::Spot])
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(err.msg.contains("no firewall API") && err.msg.contains("on-demand only"));
        assert!(!caps.supports(Feature::OpenPorts));
        assert!(caps.supports(Feature::ImageOverride));
    }

    #[test]
    fn block_scope_defaults_and_overrides() {
        assert_eq!(ProviderError::quota("x").block_scope(), BlockScope::Region);
        assert_eq!(
            ProviderError::capacity("x").block_scope(),
            BlockScope::Offer
        );
        assert_eq!(
            ProviderError::unavailable("x")
                .with_block(BlockScope::TypeInRegion)
                .block_scope(),
            BlockScope::TypeInRegion
        );
        assert!(ErrorKind::Credentials.is_fatal());
        assert!(!ErrorKind::Capacity.is_fatal());
    }
}
