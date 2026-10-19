//! Hyperstack adapter (Infrahub API v1). A Hyperstack VM is a GPU or CPU
//! virtual machine in one region (`CANADA-1`, `US-1`, `NORWAY-1`), placed in
//! an *environment* (a per-region project), reached over SSH as `ubuntu` on
//! its public IP.
//!
//! Primitive mapping:
//!
//! | primitive | Hyperstack |
//! |-----------|------------|
//! | plan      | `GET /core/flavors` ⨝ `GET /core/stocks` ⨝ `GET /pricebook` → one offer per in-stock flavor × region, cheapest first; North American regions unless asked otherwise (`catalog`) |
//! | prepare   | re-check the flavor's stock; find-or-create environment `gpuprov-<region>`; find-or-import our key pair into it; pick the image |
//! | create    | `POST /core/virtual-machines` named `gpuprov-<vm>`, labeled, public IP (SSH kept on 22), inbound rules for 22 + requested ports, cloud-init user data incl. the boot script |
//! | observe   | `GET /core/virtual-machines?environment=…&search=gpuprov-<vm>`, exact name match |
//! | delete    | `DELETE /core/virtual-machines/{id}` (already gone = success) |
//! | cleanup   | nothing per VM: environments and key pairs are shared across VMs |
//!
//! Spot capacity is a separate flavor family (`…-spot`), selected by
//! `--spot`. Firewall rules are part of the create call, so extra ports
//! (and the k3s API) are reachable directly.

pub mod api;
pub mod catalog;
pub mod error;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ApiKeyConfig;
use crate::provider::{
    Capabilities, CloudProvider, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceRequest, SshEndpoint,
};
use crate::userdata;

use api::{Api, CreateVm, SecurityRule, Vm};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "hyperstack";

const DEFAULT_BASE_URL: &str = "https://infrahub-api.nexgencloud.com/v1";
const API_KEY_ENV: &str = "HYPERSTACK_API_KEY";

/// Labels carried by every VM gpuprov creates.
const LABEL_MANAGED: &str = "gpuprov-managed";
fn identity_label(vm_name: &str) -> String {
    format!("gpuprov-vm-name={vm_name}")
}

/// Large GPU flavors can take many minutes to build.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    environment: String,
    key_name: String,
    image: String,
}

pub struct HyperstackProvider {
    api: Api,
}

impl HyperstackProvider {
    /// Resolve the API key: config → `$HYPERSTACK_API_KEY`.
    pub fn new(config: &ApiKeyConfig) -> Result<Self, ProviderError> {
        let key = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty()))
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no Hyperstack API key: set hyperstack.api_key in the config file or \
                     ${API_KEY_ENV} (create one under API Keys in the Hyperstack console)"
                ))
            })?;
        Self::with_api(config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL), &key)
    }

    /// Construct against an explicit endpoint (tests, staging).
    pub fn with_api(base_url: &str, api_key: &str) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, api_key).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(HyperstackProvider { api })
    }

    /// Find environment `name` in `region`, or create it. Additive only.
    async fn ensure_environment(&self, name: &str, region: &str) -> Result<(), ProviderError> {
        let exists = |envs: &[api::Environment]| envs.iter().any(|e| e.name == name);
        let envs = self
            .api
            .environments()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if exists(&envs) {
            return Ok(());
        }
        match self.api.create_environment(name, region).await {
            Ok(()) => Ok(()),
            // Lost a race with a concurrent create: fine if it now exists.
            Err(e) if e.status() == Some(409) || e.status() == Some(400) => {
                let envs = self
                    .api
                    .environments()
                    .await
                    .map_err(|e| e.into_provider_error(Op::Prepare))?;
                if exists(&envs) {
                    Ok(())
                } else {
                    Err(e.into_provider_error(Op::Prepare))
                }
            }
            Err(e) => Err(e.into_provider_error(Op::Prepare)),
        }
    }

    /// Find our key (by type + material) in `environment`, or import it.
    /// Returns the key pair name. Additive only.
    async fn ensure_keypair(
        &self,
        environment: &str,
        public_key: &str,
    ) -> Result<String, ProviderError> {
        let wanted = key_material(public_key);
        let find = |pairs: &[api::Keypair]| {
            pairs
                .iter()
                .find(|k| {
                    k.environment.name == environment && key_material(&k.public_key) == wanted
                })
                .map(|k| k.name.clone())
        };
        let pairs = self
            .api
            .keypairs()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(name) = find(&pairs) {
            return Ok(name);
        }
        let name = keypair_name(public_key);
        match self
            .api
            .import_keypair(&name, environment, public_key)
            .await
        {
            Ok(()) => Ok(name),
            Err(e) if e.status() == Some(409) || e.status() == Some(400) => {
                let pairs = self
                    .api
                    .keypairs()
                    .await
                    .map_err(|e| e.into_provider_error(Op::Prepare))?;
                find(&pairs).ok_or_else(|| e.into_provider_error(Op::Prepare))
            }
            Err(e) => Err(e.into_provider_error(Op::Prepare)),
        }
    }

    /// This VM's instances in `region`'s environment (exact name match).
    async fn vms(&self, region: &str, vm_name: &str) -> Result<Vec<Vm>, ProviderError> {
        let name = vm_resource_name(vm_name);
        let vms = self
            .api
            .vms(&environment_name(region), &name)
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        Ok(vms.into_iter().filter(|vm| vm.name == name).collect())
    }
}

/// The environment gpuprov uses in a region (shared by all its VMs there).
pub fn environment_name(region: &str) -> String {
    format!("gpuprov-{}", region.to_lowercase())
}

/// The VM's resource name (≤ 50 chars: VM names are ≤ 40).
pub fn vm_resource_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// Key type + base64 material, ignoring the comment.
fn key_material(public_key: &str) -> Vec<String> {
    public_key
        .split_whitespace()
        .take(2)
        .map(str::to_string)
        .collect()
}

/// Stable, content-addressed key pair name: `gpuprov-<8 hex FNV-1a>`.
fn keypair_name(public_key: &str) -> String {
    format!("gpuprov-{}", fnv1a_hex(&key_material(public_key).join(" ")))
}

fn fnv1a_hex(text: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// Normalize a VM (`None` once it is gone).
fn to_observed(vm: &Vm) -> Option<Observed> {
    let status = match vm.status.to_uppercase().as_str() {
        "DELETED" => return None,
        "ACTIVE" => InstanceStatus::Running,
        "STOPPING" => InstanceStatus::Stopping,
        "SHUTOFF" | "HIBERNATING" | "HIBERNATED" => InstanceStatus::Stopped,
        "DELETING" => InstanceStatus::Terminating,
        "ERROR" => InstanceStatus::Failed("ERROR".into()),
        // CREATING, BUILD, STARTING, RESTORING, REBOOTING, and anything new.
        _ => InstanceStatus::Pending,
    };
    let public_ip = vm
        .floating_ip
        .as_deref()
        .map(str::trim)
        .filter(|ip| !ip.is_empty());
    let endpoint = (status == InstanceStatus::Running)
        .then_some(public_ip)
        .flatten()
        .map(|ip| SshEndpoint {
            host: ip.to_string(),
            port: 22,
            // The login user comes from our cloud-init (`ubuntu` by default,
            // as on Hyperstack's Ubuntu images).
            user: None,
        });
    Some(Observed {
        instance_id: vm.id.to_string(),
        status,
        zone: None,
        private_ip: vm.fixed_ip.clone().filter(|ip| !ip.is_empty()),
        endpoint,
    })
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone()).map_err(|e| {
        ProviderError::invalid(format!("offer {offer} is not a Hyperstack offer: {e}"))
    })
}

#[async_trait]
impl CloudProvider for HyperstackProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        // Full VMs with cloud-init, per-VM firewall rules, spot flavors and
        // public + custom images: everything is supported.
        Capabilities {
            unsupported: &[],
            boot_script: true,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: DELETE_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let (flavors, stocks, prices) = tokio::try_join!(
            self.api.flavors(None),
            self.api.stocks(),
            self.api.pricebook(),
        )
        .map_err(|e| e.into_provider_error(Op::Plan))?;
        catalog::offers_from(&flavors, &stocks, &catalog::price_index(&prices), request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        // 1. Stock moves fast: re-check before touching anything.
        let (flavors, stocks) =
            tokio::try_join!(self.api.flavors(Some(&detail.region)), self.api.stocks(),)
                .map_err(|e| e.into_provider_error(Op::Prepare))?;
        let still_in_stock = flavors
            .iter()
            .filter(|f| f.name == detail.flavor_name && f.region_name == detail.region)
            .any(|f| catalog::in_stock(f, &stocks));
        if !still_in_stock {
            return Err(ProviderError::unavailable(format!(
                "{} is no longer in stock in {}",
                detail.flavor_name, detail.region
            )));
        }
        // 2. Environment and key pair (shared, find-or-create).
        let environment = environment_name(&detail.region);
        self.ensure_environment(&environment, &detail.region)
            .await?;
        let key_name = self
            .ensure_keypair(&environment, spec.ssh_public_key.trim())
            .await?;
        // 3. Image.
        let image = match &spec.image_id {
            Some(image) => image.clone(),
            None => {
                let images = self
                    .api
                    .images(&detail.region)
                    .await
                    .map_err(|e| e.into_provider_error(Op::Prepare))?;
                catalog::choose_image(&images, &detail).ok_or_else(|| {
                    ProviderError::unavailable(format!(
                        "no suitable Ubuntu image for {} in {}",
                        detail.flavor_name, detail.region
                    ))
                })?
            }
        };
        serde_json::to_value(Prepared {
            environment,
            key_name,
            image,
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
        let mut ports: Vec<u16> = vec![22];
        ports.extend(&spec.open_ports);
        ports.sort_unstable();
        ports.dedup();
        let body = CreateVm {
            name: vm_resource_name(&spec.vm_name),
            environment_name: prepared.environment,
            image_name: prepared.image,
            flavor_name: detail.flavor_name,
            key_name: prepared.key_name,
            count: 1,
            assign_floating_ip: true,
            enable_port_randomization: false,
            user_data: userdata::render(spec),
            labels: vec![identity_label(&spec.vm_name), LABEL_MANAGED.to_string()],
            security_rules: ports.into_iter().map(SecurityRule::tcp_ingress).collect(),
        };
        let id = self
            .api
            .create_vm(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("[{}] created VM {id}", offer.region);
        Ok(id.to_string())
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        Ok(self
            .vms(&at.region, vm_name)
            .await?
            .iter()
            .filter_map(to_observed)
            .collect())
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        for id in instance_ids {
            self.api
                .delete_vm(id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }

    /// Environments and key pairs are shared by every gpuprov VM in a
    /// region, and the public IP is released with the VM: nothing to do.
    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
