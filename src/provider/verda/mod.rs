//! Verda adapter (REST API v1). A Verda VM is a GPU or CPU instance in one
//! of Verda's locations (`FIN-01`, `ICE-01`, …), reached over SSH as `root`.
//!
//! Primitive mapping:
//!
//! | primitive | Verda |
//! |-----------|-------|
//! | plan      | live `GET /v1/instance-types` × `GET /v1/instance-availability` (+ `GET /v1/locations` for country filters) → one offer per type × location that can launch it now, cheapest first (`catalog`) |
//! | prepare   | find-or-register our SSH key; store the boot script as a startup script (named by content hash); re-check availability; pick the OS image |
//! | create    | `POST /v1/instances` tagged `gpuprov-vm-name=<vm>` / `gpuprov-managed=true`, OS volume tagged the same |
//! | observe   | `GET /v1/instances?tag=gpuprov-vm-name=<vm>` |
//! | delete    | `PUT /v1/instances` `delete` with the instances' volumes, permanently |
//! | cleanup   | permanently delete detached volumes still tagged for the VM; delete its startup scripts |
//!
//! Authentication is OAuth client credentials (`auth`). The boot script (the
//! k3s installer) is delivered as a Verda startup script — a shell script
//! run once as root at first boot — so clusters install while the VM boots.
//! There is no firewall API, so extra inbound ports are declared unsupported
//! (k3s then uses SSH-tunnel access).

pub mod api;
pub mod auth;
pub mod catalog;
pub mod error;

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::VerdaConfig;
use crate::provider::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceRequest, SshEndpoint,
};

use api::{Api, DeployInstance, Instance, OsVolume, Tag};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "verda";

const DEFAULT_BASE_URL: &str = "https://api.verda.com";
const CLIENT_ID_ENV: &str = "VERDA_CLIENT_ID";
const CLIENT_SECRET_ENV: &str = "VERDA_CLIENT_SECRET";

/// Identity tag carried by every instance and OS volume of a VM.
pub const TAG_VM_NAME: &str = "gpuprov-vm-name";
pub const TAG_MANAGED: &str = "gpuprov-managed";
/// Verda images log in as root.
const LOGIN_USER: &str = "root";

/// Large GPU hosts can take many minutes to provision.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] = &[(
    Feature::OpenPorts,
    "no firewall API: ports other than SSH cannot be opened or guaranteed reachable",
)];

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    ssh_key_id: String,
    #[serde(default)]
    startup_script_id: Option<String>,
    image: String,
}

pub struct VerdaProvider {
    api: Api,
}

impl VerdaProvider {
    /// Resolve the client credentials: config → `$VERDA_CLIENT_ID` /
    /// `$VERDA_CLIENT_SECRET`.
    pub fn new(config: &VerdaConfig) -> Result<Self, ProviderError> {
        let resolve = |configured: &Option<String>, env: &str| {
            configured
                .clone()
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var(env).ok().filter(|v| !v.is_empty()))
        };
        let (Some(client_id), Some(client_secret)) = (
            resolve(&config.client_id, CLIENT_ID_ENV),
            resolve(&config.client_secret, CLIENT_SECRET_ENV),
        ) else {
            return Err(ProviderError::credentials(format!(
                "no Verda API credentials: set verda.client_id and verda.client_secret in the \
                 config file, or ${CLIENT_ID_ENV} and ${CLIENT_SECRET_ENV} (create them under \
                 Credentials in the Verda console)"
            )));
        };
        Self::with_api(
            config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
            &client_id,
            &client_secret,
        )
    }

    /// Construct against an explicit endpoint (tests, staging).
    pub fn with_api(
        base_url: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, client_id, client_secret)
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(VerdaProvider { api })
    }

    /// Find our key among the account's keys (by type + material), or
    /// register it. Additive only: account keys are never removed.
    async fn ensure_ssh_key(&self, public_key: &str) -> Result<String, ProviderError> {
        let wanted = key_material(public_key);
        let keys = self
            .api
            .ssh_keys()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = keys.iter().find(|k| key_material(&k.key) == wanted) {
            return Ok(found.id.clone());
        }
        let name = format!("gpuprov-{}", fnv1a_hex(&wanted.join(" ")));
        self.api
            .add_ssh_key(&name, public_key.trim())
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))
    }

    /// Store the boot script as a startup script, reusing one with the same
    /// content-addressed name (so retries never pile up copies).
    async fn ensure_script(&self, vm_name: &str, content: &str) -> Result<String, ProviderError> {
        let name = script_name(vm_name, content);
        let scripts = self
            .api
            .scripts()
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if let Some(found) = scripts.iter().find(|s| s.name == name) {
            return Ok(found.id.clone());
        }
        self.api
            .add_script(&name, content)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))
    }

    /// The VM's instances (any status) as the API reports them.
    async fn tagged_instances(&self, vm_name: &str) -> Result<Vec<Instance>, ProviderError> {
        let instances = self
            .api
            .instances_tagged(TAG_VM_NAME, vm_name)
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        Ok(instances
            .into_iter()
            .filter(|i| has_tag(&i.tags, TAG_VM_NAME, vm_name))
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

/// `gpuprov-<vm>-<content hash>`: one name per script version.
fn script_name(vm_name: &str, content: &str) -> String {
    format!("gpuprov-{vm_name}-{}", fnv1a_hex(content))
}

/// Whether `name` is one of `vm_name`'s startup scripts — exactly
/// `gpuprov-<vm>-<8 hex>`, so `dev` never claims `dev-2`'s scripts.
fn is_vm_script(name: &str, vm_name: &str) -> bool {
    name.strip_prefix("gpuprov-")
        .and_then(|rest| rest.strip_prefix(vm_name))
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hash| hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()))
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

fn has_tag(tags: &[Tag], key: &str, value: &str) -> bool {
    tags.iter()
        .any(|t| t.key.eq_ignore_ascii_case(key) && t.value == value)
}

/// Normalize an instance; `None` once it is gone (`discontinued`/`notfound`).
fn to_observed(instance: &Instance) -> Option<Observed> {
    let status = match instance.status.as_str() {
        "discontinued" | "notfound" => return None,
        "running" => InstanceStatus::Running,
        "offline" => InstanceStatus::Stopped,
        "deleting" => InstanceStatus::Terminating,
        "error" | "installation_failed" => InstanceStatus::Failed(instance.status.clone()),
        "no_capacity" => InstanceStatus::Failed("no capacity".into()),
        // new, ordered, validating, provisioning, unknown, and anything new.
        _ => InstanceStatus::Pending,
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| instance.ip.clone())
        .flatten()
        .map(|host| SshEndpoint {
            host,
            port: 22,
            user: Some(LOGIN_USER.into()),
        });
    Some(Observed {
        instance_id: instance.id.clone(),
        status,
        zone: None,
        private_ip: None,
        endpoint,
    })
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone())
        .map_err(|e| ProviderError::invalid(format!("offer {offer} is not a Verda offer: {e}")))
}

#[async_trait]
impl CloudProvider for VerdaProvider {
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
        let types = self
            .api
            .instance_types()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        let availability = self
            .api
            .availability(request.spot)
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        let locations = self
            .api
            .locations()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        catalog::offers_from(&types, &availability, &locations, request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        let ssh_key_id = self.ensure_ssh_key(&spec.ssh_public_key).await?;
        let startup_script_id = match &spec.boot_script {
            Some(boot) => Some(self.ensure_script(&spec.vm_name, &boot.to_shell()).await?),
            None => None,
        };
        // Re-check stock so a stale plan fails before anything is launched.
        let available = self
            .api
            .is_available(&detail.instance_type, &detail.location_code, detail.is_spot)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        if !available {
            return Err(ProviderError::unavailable(format!(
                "{} is no longer available in {}",
                detail.instance_type, detail.location_code
            )));
        }
        let image = match &spec.image_id {
            Some(image) => image.clone(),
            None => catalog::choose_image(&detail.supported_os, detail.gpu).ok_or_else(|| {
                ProviderError::unavailable(format!(
                    "instance type {} lists no supported OS image",
                    detail.instance_type
                ))
            })?,
        };
        let prepared = Prepared {
            ssh_key_id,
            startup_script_id,
            image,
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
        let vm = &spec.vm_name;
        let body = DeployInstance {
            instance_type: detail.instance_type,
            image: prepared.image,
            ssh_key_ids: vec![prepared.ssh_key_id],
            startup_script_id: prepared.startup_script_id,
            location_code: detail.location_code,
            hostname: format!("gpuprov-{vm}"),
            description: format!("gpuprov {vm}"),
            is_spot: detail.is_spot,
            os_volume: OsVolume {
                name: format!("gpuprov-{vm}-os"),
                size: spec.disk_gb,
                on_spot_discontinue: "delete_permanently".into(),
                tags: identity_tags(vm),
            },
            tags: identity_tags(vm),
        };
        let id = self
            .api
            .deploy(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("[{}] deployed instance {id}", offer.location());
        Ok(id)
    }

    /// Instances are found by identity tag across all locations, so `at` is
    /// ignored (teardown finds every instance of the VM).
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let mut observed: Vec<Observed> = self
            .tagged_instances(vm_name)
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
        // Deleting an instance keeps any volume not named in the request, so
        // collect each target's OS and data volumes to delete with it.
        let targets: Vec<Instance> = self
            .tagged_instances(vm_name)
            .await?
            .into_iter()
            .filter(|i| instance_ids.contains(&i.id))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = targets.iter().map(|i| i.id.clone()).collect();
        let mut volumes: Vec<String> = targets
            .iter()
            .flat_map(|i| i.os_volume_id.iter().chain(i.volume_ids.iter()).cloned())
            .collect();
        volumes.sort();
        volumes.dedup();
        let results = match self.api.delete_instances(&ids, &volumes).await {
            Ok(results) => results,
            Err(e) if e.status() == Some(404) => return Ok(()),
            Err(e) => return Err(e.into_provider_error(Op::Manage)),
        };
        let failures: Vec<String> = results
            .iter()
            .filter(|r| r.status == "error")
            .filter(|r| {
                let not_found = r.status_code == Some(404.0)
                    || r.error
                        .as_deref()
                        .is_some_and(|e| e.to_lowercase().contains("not found"));
                !not_found
            })
            .map(|r| {
                format!(
                    "{}: {}",
                    r.instance_id,
                    r.error.as_deref().unwrap_or("unknown error")
                )
            })
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ProviderError::failed(format!(
                "Verda refused to delete instance(s): {}",
                failures.join("; ")
            )))
        }
    }

    /// Remove leftovers once the instances are gone: detached volumes still
    /// tagged for the VM, and its startup scripts. SSH keys are shared
    /// across VMs and stay registered.
    async fn cleanup(&self, _at: &Location, vm_name: &str) -> Result<(), ProviderError> {
        let volumes = self
            .api
            .volumes()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        for volume in volumes
            .iter()
            .filter(|v| has_tag(&v.tags, TAG_VM_NAME, vm_name))
        {
            let gone = matches!(volume.status.as_str(), "deleted" | "deleting");
            let attached = volume.instance_id.is_some()
                || matches!(volume.status.as_str(), "attached" | "attaching");
            if gone || attached {
                continue;
            }
            tracing::info!("deleting leftover volume {} of '{vm_name}'", volume.id);
            self.api
                .delete_volume(&volume.id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        let scripts = self
            .api
            .scripts()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        for script in scripts.iter().filter(|s| is_vm_script(&s.name, vm_name)) {
            self.api
                .delete_script(&script.id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
