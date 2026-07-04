//! The cloud provisioner API ("Phase 5") — the seam between the failover
//! engine and cloud adapters. The mini analog of SkyPilot's
//! `sky/provision/__init__.py` router + `sky/provision/common.py` dataclasses,
//! as a trait instead of name-based dispatch. Only adapters below this module
//! may touch cloud SDKs.

pub mod aws;

use async_trait::async_trait;

/// Everything a cloud adapter needs to provision one VM in one region —
/// the typed replacement for SkyPilot's rendered cluster-YAML `node_config`.
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    /// Sanitized VM name: cloud tag identity, SG name suffix, DB key.
    pub vm_name: String,
    pub region: String,
    /// Zone pins; empty = any zone in the region.
    pub zones: Vec<String>,
    pub instance_type: String,
    /// AMI override; `None` → resolve per region (SSM public parameters).
    pub ami_id: Option<String>,
    /// Selects the GPU (NVIDIA-driver) AMI when no override is given.
    pub needs_gpu_ami: bool,
    pub disk_gb: i32,
    pub spot: bool,
    /// Security-group ingress ports besides SSH (22).
    pub extra_ports: Vec<u16>,
    /// Cloud-init document injecting the SSH public key.
    pub user_data: String,
    pub ssh_user: String,
}

/// Region-level prerequisites resolved by `bootstrap`.
#[derive(Debug, Clone)]
pub struct Bootstrapped {
    /// Usable public subnets, one entry per candidate zone.
    pub subnets: Vec<SubnetInfo>,
    pub security_group_id: String,
    pub resolved_ami: String,
}

#[derive(Debug, Clone)]
pub struct SubnetInfo {
    pub subnet_id: String,
    pub zone: String,
}

/// Outcome of a successful `run_instance`.
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
    pub state: String,
}

/// Error taxonomy driving failover decisions (SkyPilot equivalents noted):
///
/// | variant          | failover action                     | SkyPilot analog                    |
/// |------------------|-------------------------------------|------------------------------------|
/// | `Capacity`       | teardown, block region, continue    | capacity `RuntimeError`            |
/// | `Config`         | block region, continue, NO teardown | `SKYPILOT_ERROR_NO_NODES_LAUNCHED` |
/// | `Credentials`    | abort all failover                  | `InvalidCloudCredentials`          |
/// | `TeardownFailed` | abort (possible resource leak)      | `StopFailoverError`                |
/// | `Other`          | teardown, block region, continue    | generic `RuntimeError`             |
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
            ProvisionError::Credentials { .. } | ProvisionError::TeardownFailed { .. }
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

/// Uniform per-cloud provisioning API (SkyPilot: `bootstrap_instances`,
/// `run_instances`, `wait_instances`, `get_cluster_info`, `terminate_instances`).
#[async_trait]
pub trait CloudProvider: Send + Sync {
    /// Idempotent region-level prep: AMI resolution, network discovery,
    /// security group ensure. Launches nothing.
    async fn bootstrap(&self, req: &ProvisionRequest) -> Result<Bootstrapped, ProvisionError>;

    /// Find-or-create the VM's instance; retries across the bootstrapped
    /// subnets/zones on capacity errors before giving up.
    async fn run_instance(
        &self,
        req: &ProvisionRequest,
        bootstrapped: &Bootstrapped,
    ) -> Result<ProvisionRecord, ProvisionError>;

    /// Block until the instance reaches the running state.
    async fn wait_running(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError>;

    /// Inventory of the running instance, if any.
    async fn describe(&self, region: &str, vm_name: &str)
        -> Result<Option<VmInfo>, ProvisionError>;

    /// Terminate the VM's instance(s) and delete managed per-VM resources.
    /// Must be safe to call when nothing exists (teardown-on-failure path).
    async fn terminate(&self, region: &str, vm_name: &str) -> Result<(), ProvisionError>;
}
