//! The cloud provisioner API ("Phase 5") — the seam between the failover
//! engine and cloud adapters. The mini analog of SkyPilot's
//! `sky/provision/__init__.py` router + `sky/provision/common.py` dataclasses,
//! as a trait instead of name-based dispatch. Only adapters below this module
//! may touch cloud SDKs.

pub mod aws;

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;

use crate::config::AppConfig;

/// Default cloud used when the spec doesn't name one.
pub const DEFAULT_PROVIDER: &str = "aws";

/// Construct the adapter for a provider id — the single registry point that a
/// new cloud is added to (SkyPilot's `sky/provision/__init__.py` router).
pub async fn build(
    name: &str,
    config: &AppConfig,
) -> Result<Box<dyn CloudProvider>, ProvisionError> {
    match name {
        "aws" => Ok(Box::new(aws::AwsProvider::new(&config.aws).await)),
        other => Err(ProvisionError::InvalidRequest {
            msg: format!("unknown cloud provider '{other}'"),
        }),
    }
}

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
/// validation is needed. (CPU/memory constraints may join this enum later.)
#[derive(Debug, Clone)]
pub enum ResourceAsk {
    InstanceType(String),
    Gpus(GpuRequest),
}

/// The cloud-agnostic resource ask handed to a provider for planning.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    pub ask: ResourceAsk,
    /// Ordered region preference; region naming is cloud-specific.
    pub regions: Vec<String>,
    /// Optional zone pin; zone↔region conventions are cloud-specific.
    pub zone: Option<String>,
}

/// One launch candidate produced by a provider's `plan_candidates` — the mini
/// analog of a SkyPilot "launchable" `Resources` (cloud, region, zone,
/// instance_type). Ordering encodes preference; the failover engine walks the
/// list in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub instance_type: String,
    pub region: String,
    /// `None` = any zone the provider finds usable in the region.
    pub zone: Option<String>,
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.instance_type, self.region)?;
        if let Some(zone) = &self.zone {
            write!(f, "/{zone}")?;
        }
        Ok(())
    }
}

/// Cross a provider's resolved instance-type ladder (cheapest first) with the
/// candidate regions — type-major, so the cheapest type is tried in every
/// region before the next size up. A zone pin selects its owning region via
/// the `region`-is-a-prefix-of-`zone` convention (holds for AWS and GCP).
/// Providers that don't share this convention can build candidates directly.
pub fn expand_candidates(
    instance_types: &[String],
    regions: &[String],
    zone: Option<&str>,
) -> Result<Vec<Candidate>, ProvisionError> {
    if regions.is_empty() {
        return Err(ProvisionError::InvalidRequest {
            msg: "no candidate regions".to_string(),
        });
    }
    let (regions, zone): (Vec<String>, Option<String>) = match zone {
        None => (regions.to_vec(), None),
        Some(zone) => match regions.iter().find(|r| zone.starts_with(r.as_str())) {
            Some(region) => (vec![region.clone()], Some(zone.to_string())),
            None => {
                return Err(ProvisionError::InvalidRequest {
                    msg: format!(
                        "zone '{zone}' does not belong to any candidate region ({})",
                        regions.join(", ")
                    ),
                })
            }
        },
    };
    let mut candidates = Vec::with_capacity(instance_types.len() * regions.len());
    for instance_type in instance_types {
        for region in &regions {
            candidates.push(Candidate {
                instance_type: instance_type.clone(),
                region: region.clone(),
                zone: zone.clone(),
            });
        }
    }
    Ok(candidates)
}

/// Everything a cloud adapter needs to provision one VM in one region —
/// the typed replacement for SkyPilot's rendered cluster-YAML `node_config`.
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    /// Sanitized VM name: cloud tag identity, SG name suffix, DB key.
    pub vm_name: String,
    pub region: String,
    /// Zone pin; `None` = any zone in the region.
    pub zone: Option<String>,
    pub instance_type: String,
    /// Machine-image override; `None` → the adapter resolves one per region
    /// from live instance-type metadata (GPU vs CPU, x86_64 vs arm64).
    pub image_id: Option<String>,
    pub disk_gb: i32,
    pub spot: bool,
    /// Security-group ingress ports besides SSH (22).
    pub extra_ports: Vec<u16>,
    /// Cloud-init document injecting the SSH public key.
    pub user_data: String,
    pub ssh_user: String,
}

/// Outcome of a successful `provision`.
#[derive(Debug, Clone)]
pub struct ProvisionRecord {
    pub region: String,
    pub zone: String,
    pub instance_id: String,
    /// True when an already-live instance was reused (idempotent re-provision).
    pub reused: bool,
}

/// Live instance inventory (the `get_cluster_info` analog).
#[derive(Debug, Clone)]
pub struct VmInfo {
    pub instance_id: String,
    pub public_ip: Option<String>,
    pub private_ip: Option<String>,
}

/// Error taxonomy driving failover decisions (SkyPilot equivalents noted):
///
/// | variant          | failover action                     | SkyPilot analog                    |
/// |------------------|-------------------------------------|------------------------------------|
/// | `Capacity`       | teardown, block candidate, continue | capacity `RuntimeError`            |
/// | `Config`         | block candidate, continue, NO teardown | `SKYPILOT_ERROR_NO_NODES_LAUNCHED` |
/// | `InvalidRequest` | abort — the ask itself is unplannable | `ResourcesUnavailableError` at plan time |
/// | `Credentials`    | abort all failover                  | `InvalidCloudCredentials`          |
/// | `TeardownFailed` | abort (possible resource leak)      | `StopFailoverError`                |
/// | `Other`          | teardown, block candidate, continue | generic `RuntimeError`             |
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProvisionError {
    #[error("capacity/limit in {region}{}: {msg}", zone.as_deref().map(|z| format!(" ({z})")).unwrap_or_default())]
    Capacity {
        region: String,
        zone: Option<String>,
        msg: String,
    },
    #[error("config problem in {region} (nothing launched): {msg}")]
    Config { region: String, msg: String },
    /// The resource request cannot be planned for this cloud at all (unknown
    /// GPU shorthand, malformed zone, ...). No candidate/region to blame and
    /// no retry will fix it.
    #[error("invalid request: {msg}")]
    InvalidRequest { msg: String },
    #[error("cloud credentials error: {msg}")]
    Credentials { msg: String },
    #[error(
        "teardown failed in {region} — resources may be leaked, check the cloud console: {msg}"
    )]
    TeardownFailed { region: String, msg: String },
    #[error("provision error in {region}: {msg}")]
    Other { region: String, msg: String },
}

impl ProvisionError {
    /// Errors that end the whole failover loop rather than the current candidate.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ProvisionError::Credentials { .. }
                | ProvisionError::TeardownFailed { .. }
                | ProvisionError::InvalidRequest { .. }
        )
    }

    /// Whether partial resources may exist and must be torn down.
    pub fn needs_teardown(&self) -> bool {
        matches!(
            self,
            ProvisionError::Capacity { .. } | ProvisionError::Other { .. }
        )
    }
}

/// Uniform per-cloud provisioning API. The engine above stays cloud-agnostic:
/// it plans candidates, provisions one, waits, and (on failure) tears down —
/// all through this trait. A new cloud implements these six methods and
/// registers in `build`; nothing above the trait imports a cloud SDK.
#[async_trait]
pub trait CloudProvider: Send + Sync {
    /// Canonical lowercase provider id (e.g. `"aws"`, `"gcp"`) — recorded on
    /// VM rows so teardown and future multi-cloud dispatch know which adapter
    /// owns a VM.
    fn name(&self) -> &'static str;

    /// Resolve a resource request into ordered launch candidates
    /// `{instance_type, region, zone}` — the mini analog of SkyPilot's
    /// optimizer. Cloud-specific: a GPU ask maps to this cloud's instance
    /// types (one GPU:count may match several, differing in CPU/memory).
    /// Most adapters resolve the type ladder then call `expand_candidates`.
    async fn plan_candidates(
        &self,
        request: &ResourceRequest,
    ) -> Result<Vec<Candidate>, ProvisionError>;

    /// Provision the VM's instance and return its record. Does the
    /// cloud-specific prep (network, image, security group) and find-or-create
    /// in one step; idempotent — reuses a live instance already tagged for
    /// this VM (SkyPilot: `bootstrap_instances` + `run_instances`).
    async fn provision(&self, req: &ProvisionRequest) -> Result<ProvisionRecord, ProvisionError>;

    /// Block until the instance reaches the running state.
    async fn wait_running(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError>;

    /// Inventory of the running instance, if any.
    async fn describe(&self, region: &str, vm_name: &str)
        -> Result<Option<VmInfo>, ProvisionError>;

    /// Terminate the VM's instance(s) and delete managed per-VM resources.
    /// Must be safe to call when nothing exists (teardown-on-failure path).
    async fn terminate(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError>;
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
    fn candidate_display_includes_zone_when_pinned() {
        let mut candidate = Candidate {
            instance_type: "g6e.8xlarge".into(),
            region: "us-east-2".into(),
            zone: None,
        };
        assert_eq!(candidate.to_string(), "g6e.8xlarge@us-east-2");
        candidate.zone = Some("us-east-2a".into());
        assert_eq!(candidate.to_string(), "g6e.8xlarge@us-east-2/us-east-2a");
    }

    fn types(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expand_is_type_major_over_regions() {
        let rendered: Vec<String> = expand_candidates(
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
        let candidates = expand_candidates(
            &types(&["t3.micro"]),
            &types(&["us-west-2", "us-east-1"]),
            Some("us-east-1b"),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].region, "us-east-1");
        assert_eq!(candidates[0].zone.as_deref(), Some("us-east-1b"));
    }

    #[test]
    fn expand_rejects_empty_regions_and_orphan_zone() {
        assert!(matches!(
            expand_candidates(&types(&["t3.micro"]), &[], None),
            Err(ProvisionError::InvalidRequest { .. })
        ));
        assert!(matches!(
            expand_candidates(
                &types(&["t3.micro"]),
                &types(&["us-west-2"]),
                Some("eu-west-1a")
            ),
            Err(ProvisionError::InvalidRequest { .. })
        ));
    }
}
