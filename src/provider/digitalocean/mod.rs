//! DigitalOcean adapter (API v2). A DigitalOcean VM is a Droplet — a GPU
//! Droplet for GPU asks — in one of DigitalOcean's regions (`tor1`, `nyc2`,
//! `atl1`, …), booted from DigitalOcean's AI/ML-ready GPU image and reached
//! over SSH as `root`.
//!
//! Primitive mapping:
//!
//! | primitive | DigitalOcean |
//! |-----------|--------------|
//! | plan      | live `GET /v2/sizes` (hourly price, `gpu_info`, regions) → one offer per size × region, cheapest first, North American regions by default (`catalog`) |
//! | prepare   | re-check the size is still offered in the region; find-or-register our SSH key; per-VM tag; per-VM cloud firewall (SSH + requested ports) bound to that tag |
//! | create    | `POST /v2/droplets` named `gpuprov-<vm>`, tagged `gpuprov` + `gpuprov-vm-<vm>`, cloud-init user data |
//! | observe   | `GET /v2/droplets?tag_name=gpuprov-vm-<vm>` (paginated) |
//! | delete    | `DELETE /v2/droplets/{id}` |
//! | cleanup   | delete the VM's cloud firewall and its tag |
//!
//! Droplets take cloud-init user data, so the boot script (the k3s
//! installer) installs while the Droplet boots. DigitalOcean installs the
//! `ssh_keys` of a create for `root` on every image, so `root` is the login
//! the adapter reports regardless of what cloud-init does. Cloud firewalls
//! deny inbound traffic not listed, so one is always created per VM; extra
//! inbound ports — and the direct k3s API — are supported. There is no spot
//! capacity. A powered-off Droplet keeps billing, which `diagnose` reports.

pub mod api;
pub mod catalog;
pub mod error;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ApiKeyConfig;
use crate::provider::{
    pick_instance, Capabilities, CloudProvider, Diagnosis, Feature, Finding, InstanceSpec,
    InstanceStatus, Location, Observed, Offer, ProviderError, ResourceRequest, SshEndpoint,
};
use crate::userdata;

use api::{Addresses, Api, Droplet, FirewallSpec, InboundRule, NewDroplet, OutboundRule};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "digitalocean";

const DEFAULT_BASE_URL: &str = "https://api.digitalocean.com";
/// Token env vars, in order (`doctl` and the Terraform provider use these).
const TOKEN_ENVS: &[&str] = &["DIGITALOCEAN_ACCESS_TOKEN", "DIGITALOCEAN_TOKEN"];

/// Tag on every Droplet gpuprov creates.
pub const TAG_MANAGED: &str = "gpuprov";
/// Prefix of the per-VM identity tag (`gpuprov-vm-<vm>`).
pub const TAG_VM_PREFIX: &str = "gpuprov-vm-";

/// The login DigitalOcean installs the create's `ssh_keys` for.
const SSH_USER: &str = "root";

/// GPU Droplets take several minutes to provision (8-GPU hosts more).
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] =
    &[(Feature::Spot, "DigitalOcean offers on-demand Droplets only")];

/// Every address, IPv4 and IPv6.
const ANYWHERE: &[&str] = &["0.0.0.0/0", "::/0"];

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    ssh_key_id: u64,
    firewall_id: String,
}

pub struct DigitalOceanProvider {
    api: Api,
}

impl DigitalOceanProvider {
    /// Resolve the token: config → `$DIGITALOCEAN_ACCESS_TOKEN` →
    /// `$DIGITALOCEAN_TOKEN`.
    pub fn new(config: &ApiKeyConfig) -> Result<Self, ProviderError> {
        let token = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                TOKEN_ENVS
                    .iter()
                    .find_map(|var| std::env::var(var).ok().filter(|k| !k.is_empty()))
            })
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no DigitalOcean API token: set digitalocean.api_key in the config file or \
                     ${} (create a read/write token at \
                     https://cloud.digitalocean.com/account/api/tokens)",
                    TOKEN_ENVS[0]
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
        Ok(DigitalOceanProvider { api })
    }

    /// Find our key among the account's keys (by type + material), or
    /// register it; returns its id. Additive only: account keys are never
    /// removed (the API also rejects registering the same key twice).
    async fn ensure_ssh_key(&self, public_key: &str) -> Result<u64, ProviderError> {
        let wanted = key_material(public_key);
        let keys = self
            .api
            .ssh_keys()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = keys.iter().find(|k| key_material(&k.public_key) == wanted) {
            return Ok(found.id);
        }
        let name = format!("gpuprov-{}", fnv1a_hex(&wanted.join(" ")));
        let added = self
            .api
            .add_ssh_key(&name, public_key.trim())
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        Ok(added.id)
    }

    /// Make sure the VM's tag exists, so a firewall can reference it before
    /// any Droplet carries it.
    async fn ensure_tag(&self, tag: &str) -> Result<(), ProviderError> {
        let exists = self
            .api
            .tag_exists(tag)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if !exists {
            self.api
                .create_tag(tag)
                .await
                .map_err(|e| e.into_provider_error(Op::Prepare))?;
        }
        Ok(())
    }

    /// Find-or-create the VM's firewall (by name), writing the wanted rules
    /// either way; returns its id.
    async fn ensure_firewall(&self, vm_name: &str, ports: &[u16]) -> Result<String, ProviderError> {
        let spec = firewall_spec(vm_name, ports);
        let existing = self
            .api
            .firewalls()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = existing.iter().find(|f| f.name == spec.name) {
            self.api
                .update_firewall(&found.id, &spec)
                .await
                .map_err(|e| e.into_provider_error(Op::Prepare))?;
            return Ok(found.id.clone());
        }
        let created = self
            .api
            .create_firewall(&spec)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        Ok(created.id)
    }

    /// The VM's Droplets (any status), matched by identity tag.
    async fn vm_droplets(&self, vm_name: &str) -> Result<Vec<Droplet>, ProviderError> {
        let tag = vm_tag(vm_name);
        let droplets = self
            .api
            .droplets_tagged(&tag)
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        Ok(droplets
            .into_iter()
            .filter(|d| d.tags.contains(&tag))
            .collect())
    }
}

/// `[type, material]` of an authorized_keys line (comments ignored).
fn key_material(line: &str) -> Vec<&str> {
    line.split_whitespace().take(2).collect()
}

/// 32-bit FNV-1a as 8 hex digits: a stable, dependency-free content hash
/// for resource names.
fn fnv1a_hex(text: &str) -> String {
    let hash = text.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    format!("{hash:08x}")
}

/// Name (and hostname) of the VM's Droplet; also its firewall's name.
fn droplet_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// The VM's identity tag.
fn vm_tag(vm_name: &str) -> String {
    format!("{TAG_VM_PREFIX}{vm_name}")
}

fn anywhere() -> Addresses {
    Addresses {
        addresses: ANYWHERE.iter().map(|a| a.to_string()).collect(),
    }
}

/// The VM's cloud firewall: inbound SSH, each requested TCP port, and ICMP
/// (so `gpuprov health` can ping) from anywhere; all outbound traffic (a
/// firewall without outbound rules would block it). Bound to the VM tag.
fn firewall_spec(vm_name: &str, ports: &[u16]) -> FirewallSpec {
    let mut ports: Vec<u16> = std::iter::once(22).chain(ports.iter().copied()).collect();
    ports.sort_unstable();
    ports.dedup();
    let inbound_rules = ports
        .into_iter()
        .map(|port| InboundRule {
            protocol: "tcp".into(),
            ports: Some(port.to_string()),
            sources: anywhere(),
        })
        .chain(std::iter::once(InboundRule {
            protocol: "icmp".into(),
            ports: None,
            sources: anywhere(),
        }))
        .collect();
    // `ports: "0"` opens every port of a protocol; ICMP takes none.
    let outbound_rules = [("tcp", Some("0")), ("udp", Some("0")), ("icmp", None)]
        .into_iter()
        .map(|(protocol, ports)| OutboundRule {
            protocol: protocol.into(),
            ports: ports.map(str::to_string),
            destinations: anywhere(),
        })
        .collect();
    FirewallSpec {
        name: droplet_name(vm_name),
        inbound_rules,
        outbound_rules,
        tags: vec![vm_tag(vm_name)],
    }
}

/// An image override: all digits is an image id, anything else a slug
/// (e.g. `ubuntu-22-04-x64`).
fn image_ref(image: &str) -> serde_json::Value {
    match image.parse::<u64>() {
        Ok(id) => serde_json::Value::from(id),
        Err(_) => serde_json::Value::from(image),
    }
}

/// Normalize a Droplet; `None` once it is archived (destroyed).
fn to_observed(droplet: &Droplet) -> Option<Observed> {
    let status = match droplet.status.as_str() {
        "archive" => return None,
        "active" => InstanceStatus::Running,
        "off" => InstanceStatus::Stopped,
        // new, and anything new to the API.
        _ => InstanceStatus::Pending,
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| droplet.ipv4("public"))
        .flatten()
        .map(|host| SshEndpoint {
            host: host.to_string(),
            port: 22,
            user: Some(SSH_USER.into()),
        });
    Some(Observed {
        instance_id: droplet.id.to_string(),
        status,
        zone: None,
        private_ip: droplet.ipv4("private").map(str::to_string),
        endpoint,
    })
}

/// DigitalOcean-native health signals for one Droplet (`status`, `locked`;
/// spec `components/schemas/droplet`). Pure.
fn findings_for(droplet: &Droplet) -> Vec<Finding> {
    let mut findings = Vec::new();
    if droplet.status == "off" {
        findings.push(Finding::critical(
            "powered off — DigitalOcean keeps billing powered-off Droplets; \
             `gpuprov down` to stop billing",
        ));
    }
    if droplet.locked {
        findings.push(Finding::info(
            "locked by DigitalOcean while an action (resize, snapshot, …) runs",
        ));
    }
    findings
}

/// The VM's diagnosis from its raw Droplets: the default instance choice
/// plus DigitalOcean's own findings for the chosen Droplet. Pure.
fn diagnosis_from(droplets: &[Droplet], recorded: Option<&str>) -> Diagnosis {
    let observed: Vec<Observed> = droplets.iter().filter_map(to_observed).collect();
    let instance = pick_instance(observed, recorded);
    let findings = instance
        .as_ref()
        .and_then(|o| droplets.iter().find(|d| d.id.to_string() == o.instance_id))
        .map(findings_for)
        .unwrap_or_default();
    Diagnosis { instance, findings }
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone()).map_err(|e| {
        ProviderError::invalid(format!("offer {offer} is not a DigitalOcean offer: {e}"))
    })
}

#[async_trait]
impl CloudProvider for DigitalOceanProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: UNSUPPORTED,
            boot_script: true,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: DELETE_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let sizes = self
            .api
            .sizes()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        catalog::offers_from(&sizes, request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        // Re-check availability first so a stale plan fails before anything
        // is created or registered.
        let sizes = self
            .api
            .sizes()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if !catalog::has_capacity(&sizes, &detail.size, &detail.region) {
            return Err(ProviderError::unavailable(format!(
                "{} is no longer offered in {}",
                detail.size, detail.region
            )));
        }
        let ssh_key_id = self.ensure_ssh_key(&spec.ssh_public_key).await?;
        self.ensure_tag(&vm_tag(&spec.vm_name)).await?;
        let firewall_id = self
            .ensure_firewall(&spec.vm_name, &spec.open_ports)
            .await?;
        let prepared = Prepared {
            ssh_key_id,
            firewall_id,
        };
        serde_json::to_value(prepared)
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
        let body = NewDroplet {
            name: droplet_name(&spec.vm_name),
            region: detail.region,
            size: detail.size,
            image: image_ref(spec.image_id.as_deref().unwrap_or(&detail.image)),
            ssh_keys: vec![prepared.ssh_key_id],
            tags: vec![TAG_MANAGED.into(), vm_tag(&spec.vm_name)],
            user_data: userdata::render(spec),
        };
        let id = self
            .api
            .create_droplet(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("[{}] created Droplet {id}", offer.location());
        Ok(id.to_string())
    }

    /// Droplets are found by identity tag across all regions, so `at` is
    /// ignored (teardown finds every Droplet of the VM).
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let mut observed: Vec<Observed> = self
            .vm_droplets(vm_name)
            .await?
            .iter()
            .filter_map(to_observed)
            .collect();
        observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(observed)
    }

    /// Adds powered-off (still billing) and locked Droplets as findings.
    async fn diagnose(
        &self,
        _at: &Location,
        vm_name: &str,
        instance_id: Option<&str>,
    ) -> Result<Diagnosis, ProviderError> {
        let droplets = self.vm_droplets(vm_name).await?;
        Ok(diagnosis_from(&droplets, instance_id))
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        // By id, without listing first: a just-created Droplet may not be
        // listed under its tag yet. Ids are account-unique and come from
        // `create`/`observe`; an already-destroyed one answers 404 (success).
        for id in instance_ids {
            self.api
                .delete_droplet(id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }

    /// Delete the VM's cloud firewall and tag. Both are account-wide, not
    /// regional, so `at` is ignored and repeated calls are no-ops.
    async fn cleanup(&self, _at: &Location, vm_name: &str) -> Result<(), ProviderError> {
        let name = droplet_name(vm_name);
        let firewalls = self
            .api
            .firewalls()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        for firewall in firewalls.iter().filter(|f| f.name == name) {
            tracing::info!("deleting cloud firewall {} of '{vm_name}'", firewall.id);
            self.api
                .delete_firewall(&firewall.id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        self.api
            .delete_tag(&vm_tag(vm_name))
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))
    }
}

#[cfg(test)]
mod tests;
