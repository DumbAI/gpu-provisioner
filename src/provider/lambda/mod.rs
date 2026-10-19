//! Lambda Cloud adapter (API v1). A Lambda VM is an on-demand GPU instance
//! in one of Lambda's regions (`us-east-1`, …), booted from the Lambda Stack
//! image (Ubuntu + CUDA + drivers) and reached over SSH as `ubuntu`.
//!
//! Primitive mapping:
//!
//! | primitive | Lambda |
//! |-----------|--------|
//! | plan      | live `GET /api/v1/instance-types` (price + regions with capacity now) → one offer per type × region, cheapest first, US regions by default (`catalog`) |
//! | prepare   | find-or-register our SSH key; per-VM firewall ruleset when extra ports are asked for; re-check capacity |
//! | create    | `POST /api/v1/instance-operations/launch` named/tagged `gpuprov-<vm>` / `gpuprov-vm-name=<vm>`, cloud-init user data, the ruleset |
//! | observe   | `GET /api/v1/instances` (paginated), filtered by the identity tag |
//! | delete    | `POST /api/v1/instance-operations/terminate` |
//! | cleanup   | delete the VM's firewall ruleset in the region |
//!
//! Launches take cloud-init user data, so the boot script (the k3s installer)
//! installs while the VM boots. Firewall rulesets attach per instance, so
//! extra inbound ports — and the direct k3s API — are supported. Lambda has
//! no spot capacity, so spot asks are rejected up front. Billing is per
//! minute.

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
use crate::userdata;

use api::{Api, FirewallRule, IdRef, ImageRef, Instance, Launch, Tag};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "lambda";

const DEFAULT_BASE_URL: &str = "https://cloud.lambda.ai";
const API_KEY_ENV: &str = "LAMBDA_API_KEY";

/// Identity tag carried by every instance of a VM.
pub const TAG_VM_NAME: &str = "gpuprov-vm-name";
pub const TAG_MANAGED: &str = "gpuprov-managed";

/// Lambda instances can take several minutes to boot (large GPU hosts more).
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] =
    &[(Feature::Spot, "Lambda offers on-demand instances only")];

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    ssh_key_name: String,
    #[serde(default)]
    firewall_ruleset_id: Option<String>,
}

pub struct LambdaProvider {
    api: Api,
}

impl LambdaProvider {
    /// Resolve the API key: config → `$LAMBDA_API_KEY`.
    pub fn new(config: &ApiKeyConfig) -> Result<Self, ProviderError> {
        let api_key = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty()))
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no Lambda API key: set lambda.api_key in the config file or \
                     ${API_KEY_ENV} (create one at https://cloud.lambda.ai/api-keys)"
                ))
            })?;
        Self::with_api(
            config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
            &api_key,
        )
    }

    /// Construct against an explicit endpoint (tests, staging).
    pub fn with_api(base_url: &str, api_key: &str) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, api_key).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(LambdaProvider { api })
    }

    /// Find our key among the account's keys (by type + material), or
    /// register it; returns its name (launches reference keys by name).
    /// Additive only: account keys are never removed.
    async fn ensure_ssh_key(&self, public_key: &str) -> Result<String, ProviderError> {
        let wanted = key_material(public_key);
        let keys = self
            .api
            .ssh_keys()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = keys.iter().find(|k| key_material(&k.public_key) == wanted) {
            return Ok(found.name.clone());
        }
        let name = format!("gpuprov-{}", fnv1a_hex(&wanted.join(" ")));
        let added = self
            .api
            .add_ssh_key(&name, public_key.trim())
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        Ok(added.name)
    }

    /// Find-or-create (and reconcile the rules of) the VM's ruleset in
    /// `region`, opening SSH plus `ports`.
    async fn ensure_ruleset(
        &self,
        vm_name: &str,
        region: &str,
        ports: &[u16],
    ) -> Result<String, ProviderError> {
        let name = ruleset_name(vm_name);
        let rules = ruleset_rules(ports);
        let existing = self
            .api
            .firewall_rulesets()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = existing
            .iter()
            .find(|r| r.name == name && r.region.name == region)
        {
            if found.rules != rules {
                self.api
                    .update_firewall_rules(&found.id, &rules)
                    .await
                    .map_err(|e| e.into_provider_error(Op::Prepare))?;
            }
            return Ok(found.id.clone());
        }
        let created = self
            .api
            .create_firewall_ruleset(&name, region, &rules)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        Ok(created.id)
    }

    /// The VM's instances (any status), matched by identity tag.
    async fn vm_instances(&self, vm_name: &str) -> Result<Vec<Instance>, ProviderError> {
        let instances = self
            .api
            .instances()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        Ok(instances
            .into_iter()
            .filter(|i| is_vm_instance(i, vm_name))
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

/// Name and hostname of the VM's instance.
fn instance_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// The VM's firewall ruleset name (one per region).
fn ruleset_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// SSH plus each requested TCP port, from anywhere.
fn ruleset_rules(ports: &[u16]) -> Vec<FirewallRule> {
    let mut ports: Vec<u16> = std::iter::once(22).chain(ports.iter().copied()).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
        .into_iter()
        .map(|port| FirewallRule {
            protocol: "tcp".into(),
            port_range: Some([port, port]),
            source_network: "0.0.0.0/0".into(),
            description: format!("gpuprov: tcp/{port}"),
        })
        .collect()
}

fn identity_tags(vm_name: &str) -> Vec<Tag> {
    vec![
        Tag {
            key: TAG_VM_NAME.into(),
            value: vm_name.into(),
        },
        Tag {
            key: TAG_MANAGED.into(),
            value: "true".into(),
        },
    ]
}

/// Whether an instance belongs to `vm_name`: its identity tag, or (if the
/// tag is missing) its instance name.
fn is_vm_instance(instance: &Instance, vm_name: &str) -> bool {
    let tagged = instance
        .tags
        .iter()
        .any(|t| t.key == TAG_VM_NAME && t.value == vm_name);
    tagged || instance.name.as_deref() == Some(instance_name(vm_name).as_str())
}

/// An image override: a UUID-shaped value is an image id, anything else an
/// image family (e.g. `lambda-stack-24-04`).
fn image_ref(image: &str) -> ImageRef {
    let is_uuid = image.len() == 36
        && image.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    if is_uuid {
        ImageRef::Id {
            id: image.to_string(),
        }
    } else {
        ImageRef::Family {
            family: image.to_string(),
        }
    }
}

/// Normalize an instance; `None` once it is terminated.
fn to_observed(instance: &Instance) -> Option<Observed> {
    let status = match instance.status.as_str() {
        "terminated" => return None,
        "active" => InstanceStatus::Running,
        "terminating" => InstanceStatus::Terminating,
        "preempted" => InstanceStatus::Failed("preempted".into()),
        // booting, unhealthy (may recover; the boot deadline bounds it), and
        // anything new.
        _ => InstanceStatus::Pending,
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| instance.ip.clone())
        .flatten()
        .filter(|ip| !ip.is_empty())
        .map(|host| SshEndpoint {
            host,
            port: 22,
            // Our cloud-init creates the requested login user.
            user: None,
        });
    Some(Observed {
        instance_id: instance.id.clone(),
        status,
        zone: None,
        private_ip: instance.private_ip.clone(),
        endpoint,
    })
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone())
        .map_err(|e| ProviderError::invalid(format!("offer {offer} is not a Lambda offer: {e}")))
}

#[async_trait]
impl CloudProvider for LambdaProvider {
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
        let entries = self
            .api
            .instance_types()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        catalog::offers_from(&entries, request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        // Re-check capacity first so a stale plan fails before anything is
        // created or registered.
        let entries = self
            .api
            .instance_types()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if !catalog::has_capacity(&entries, &detail.instance_type, &detail.region) {
            return Err(ProviderError::unavailable(format!(
                "{} no longer has capacity in {}",
                detail.instance_type, detail.region
            )));
        }
        let ssh_key_name = self.ensure_ssh_key(&spec.ssh_public_key).await?;
        let firewall_ruleset_id = if spec.open_ports.is_empty() {
            None
        } else {
            Some(
                self.ensure_ruleset(&spec.vm_name, &detail.region, &spec.open_ports)
                    .await?,
            )
        };
        let prepared = Prepared {
            ssh_key_name,
            firewall_ruleset_id,
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
        let body = Launch {
            region_name: detail.region,
            instance_type_name: detail.instance_type,
            ssh_key_names: vec![prepared.ssh_key_name],
            name: instance_name(&spec.vm_name),
            hostname: instance_name(&spec.vm_name),
            image: spec.image_id.as_deref().map(image_ref),
            user_data: userdata::render(spec),
            tags: identity_tags(&spec.vm_name),
            firewall_rulesets: prepared
                .firewall_ruleset_id
                .into_iter()
                .map(|id| IdRef { id })
                .collect(),
        };
        let id = self
            .api
            .launch(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("[{}] launched instance {id}", offer.location());
        Ok(id)
    }

    /// Instances are found by identity tag across all regions, so `at` is
    /// ignored (teardown finds every instance of the VM).
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let mut observed: Vec<Observed> = self
            .vm_instances(vm_name)
            .await?
            .iter()
            .filter_map(to_observed)
            .collect();
        observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(observed)
    }

    async fn delete(
        &self,
        _at: &Location,
        vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        // Only terminate what is still live: terminating a terminated
        // instance is rejected rather than ignored.
        let live: Vec<String> = self
            .vm_instances(vm_name)
            .await?
            .into_iter()
            .filter(|i| instance_ids.contains(&i.id))
            .filter(|i| !matches!(i.status.as_str(), "terminated" | "terminating"))
            .map(|i| i.id)
            .collect();
        if live.is_empty() {
            return Ok(());
        }
        self.api
            .terminate(&live)
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))
    }

    /// Delete the VM's firewall ruleset in this region (none exists unless
    /// extra ports were opened). A ruleset still attached to a terminating
    /// instance reports `Transient`, so the core retries until it is free.
    async fn cleanup(&self, at: &Location, vm_name: &str) -> Result<(), ProviderError> {
        let name = ruleset_name(vm_name);
        let rulesets = self
            .api
            .firewall_rulesets()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        for ruleset in rulesets
            .iter()
            .filter(|r| r.name == name && r.region.name == at.region)
        {
            tracing::info!("deleting firewall ruleset {} of '{vm_name}'", ruleset.id);
            self.api
                .delete_firewall_ruleset(&ruleset.id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
