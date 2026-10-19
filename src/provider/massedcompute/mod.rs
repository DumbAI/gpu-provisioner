//! Massed Compute adapter (VM API v1 at `https://vm.massedcompute.com/api/v1`,
//! bearer token from Settings → API). A Massed Compute "VM" is a full virtual
//! machine with a preconfigured image, launched by product name.
//!
//! Primitive mapping:
//!
//! | primitive | Massed Compute |
//! |-----------|----------------|
//! | plan      | live `GET /gpu-inventory` → one offer per product with capacity, cheapest first (`catalog`) |
//! | prepare   | find-or-register our SSH key (by key material; launches reference keys by name); re-check capacity; resolve the image (`GET /images`) |
//! | create    | `POST /instance/launch` with `instanceName` `gpuprov-<vm>`, the key name, `regionName: any` |
//! | observe   | `GET /instance` by name; login user from the instance's `username` (`Ubuntu`) |
//! | delete    | `POST /instance/terminate` (unknown instance = already gone) |
//! | cleanup   | nothing per-VM (the SSH key is shared and kept) |
//!
//! Placement: the launch API takes no region (`regionName` must be `any`),
//! so offers carry region `any` and Massed Compute picks the data center.
//!
//! Capabilities: instances are full VMs (k3s runs), but there is no firewall
//! API (no `--port`; clusters use the SSH tunnel), no spot market, and the
//! k3s installer is pushed over SSH rather than through the startup command,
//! whose run-as user, size limit and per-boot behavior are not documented.

pub mod api;
pub mod catalog;
pub mod error;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ApiKeyConfig;
use crate::provider::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceRequest, SshEndpoint,
};

use api::{Api, Instance, LaunchRequest};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "massedcompute";

const DEFAULT_BASE_URL: &str = "https://vm.massedcompute.com/api/v1";
const API_KEY_ENV: &str = "MASSED_COMPUTE_API_KEY";
/// Login user of the Ubuntu images when an instance does not report one.
const DEFAULT_USER: &str = "Ubuntu";

/// Instances show "Initializing" while the image boots.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] = &[
    (
        Feature::OpenPorts,
        "no firewall API: ports other than SSH cannot be opened or guaranteed reachable",
    ),
    (Feature::Spot, "only on-demand instances are offered"),
];

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    /// Name of our registered SSH key (launches reference keys by name).
    ssh_key_name: String,
    image_id: i64,
}

pub struct MassedComputeProvider {
    api: Api,
}

impl MassedComputeProvider {
    /// Resolve the API token: config `api_key` → `$MASSED_COMPUTE_API_KEY`.
    pub fn new(config: &ApiKeyConfig) -> Result<Self, ProviderError> {
        let env_key = std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty());
        let token = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or(env_key)
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no Massed Compute API token: set massedcompute.api_key in the config file \
                     or ${API_KEY_ENV} (create a Full access token at \
                     https://vm.massedcompute.com/settings/api)"
                ))
            })?;
        Self::with_api(
            config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
            &token,
        )
    }

    /// Construct against an explicit endpoint (tests, staging).
    pub fn with_api(base_url: &str, token: &str) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, token).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(MassedComputeProvider { api })
    }
}

/// Instance name carrying the VM identity.
fn instance_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// `[type, material]` of an authorized_keys line (comments ignored).
fn key_material(line: &str) -> Vec<&str> {
    line.split_whitespace().take(2).collect()
}

/// Stable name for our key on the account (FNV-1a of the key material), so
/// every gpuprov install with the same key finds the same entry.
fn key_name(public_key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key_material(public_key).join(" ").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("gpuprov-{:08x}", hash as u32)
}

/// Normalize an instance; `None` once it is terminated (gone).
fn to_observed(instance: &Instance) -> Option<Observed> {
    let status = instance.status.to_lowercase();
    let status = match status.as_str() {
        "terminated" | "deleted" => return None,
        "terminating" | "deleting" => InstanceStatus::Terminating,
        "stopped" | "stopping" | "offline" => InstanceStatus::Stopped,
        "error" | "failed" => InstanceStatus::Failed(format!("instance {status}")),
        // `rented`, `running`, `booting`, `initializing`, …: running once
        // the OS reports booted and an IP is assigned.
        _ => {
            let booted = instance.os_booted == Some(1) && instance.active != Some(0);
            if booted && ip_of(instance).is_some() {
                InstanceStatus::Running
            } else {
                InstanceStatus::Pending
            }
        }
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| {
            ip_of(instance).map(|ip| SshEndpoint {
                host: ip.to_string(),
                port: 22,
                user: Some(
                    instance
                        .username
                        .clone()
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| DEFAULT_USER.to_string()),
                ),
            })
        })
        .flatten();
    Some(Observed {
        instance_id: instance.uuid.clone(),
        status,
        zone: None,
        private_ip: None,
        endpoint,
    })
}

fn ip_of(instance: &Instance) -> Option<&str> {
    instance.ip.as_deref().filter(|ip| !ip.trim().is_empty())
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone()).map_err(|e| {
        ProviderError::invalid(format!("offer {offer} is not a Massed Compute offer: {e}"))
    })
}

#[async_trait]
impl CloudProvider for MassedComputeProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: UNSUPPORTED,
            boot_script: false,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: DELETE_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let inventory = self
            .api
            .inventory()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        catalog::offers_from(&inventory, request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;

        // Our key, found by material (any name) or registered under a stable
        // name. Additive only: account keys are never removed.
        let keys = self
            .api
            .ssh_keys()
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        let ours = key_material(&spec.ssh_public_key);
        let ssh_key_name = match keys.iter().find(|k| key_material(&k.public_key) == ours) {
            Some(existing) => existing.name.clone(),
            None => {
                let name = key_name(&spec.ssh_public_key);
                self.api
                    .add_ssh_key(&name, spec.ssh_public_key.trim())
                    .await
                    .map_err(|e| e.into_provider_error(Op::Launch))?;
                tracing::info!("registered SSH key '{name}' on the Massed Compute account");
                name
            }
        };

        // Re-check capacity so a stale plan fails before anything is created.
        let inventory = self
            .api
            .inventory()
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        let in_stock = inventory.iter().any(|e| {
            e.instance_type.name == detail.product
                && (e.capacity_available > 0 || !e.regions_with_capacity_available.is_empty())
        });
        if !in_stock {
            return Err(ProviderError::unavailable(format!(
                "{} has no capacity anymore",
                detail.product
            )));
        }

        let images = self
            .api
            .images()
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        let image_id = catalog::choose_image(&images, spec.image_id.as_deref())?;

        serde_json::to_value(Prepared {
            ssh_key_name,
            image_id,
        })
        .map_err(|e| ProviderError::failed(format!("serializing prepared state: {e}")))
    }

    async fn create(
        &self,
        offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let detail = offer_detail(offer)?;
        let prepared: Prepared = serde_json::from_value(prepared.clone())
            .map_err(|e| ProviderError::failed(format!("corrupt prepared state: {e}")))?;
        let body = LaunchRequest {
            image_id: prepared.image_id,
            product_name: detail.product,
            region_name: catalog::ANY_REGION.to_string(),
            instance_name: instance_name(&spec.vm_name),
            ssh_keys: vec![prepared.ssh_key_name],
        };
        let uuid = self
            .api
            .launch(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!(
            "launched Massed Compute instance {uuid} ({})",
            offer.instance_type
        );
        Ok(uuid)
    }

    /// Instances are account-wide and placement is Massed Compute's choice,
    /// so `at` is ignored and instances are matched by name.
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let name = instance_name(vm_name);
        let instances = self
            .api
            .instances()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        let mut observed: Vec<Observed> = instances
            .iter()
            .filter(|i| i.name == name)
            .filter_map(to_observed)
            .collect();
        observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(observed)
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        if instance_ids.is_empty() {
            return Ok(());
        }
        match self.api.terminate(instance_ids).await {
            Ok(()) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e.into_provider_error(Op::Manage)),
        }
    }

    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
