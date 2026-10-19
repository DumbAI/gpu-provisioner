//! RunPod adapter (REST API v2). A RunPod "VM" is a **pod**: a container on a
//! GPU host, reached over SSH through a public port mapped to its `22/tcp`.
//!
//! Primitive mapping:
//!
//! | primitive | RunPod |
//! |-----------|--------|
//! | plan      | live `GET /v2/catalog/gpus?include=AVAILABILITY` per cloud tier + `GET /v2/catalog/datacenters` → one offer per GPU type × tier × data center with stock, cheapest first (`catalog`) |
//! | prepare   | read the account's SSH keys (never modified) into the pod's `PUBLIC_KEY`; re-check stock in the offer's data center |
//! | create    | `POST /v2/pods` pinned to the data center, `22/tcp` published, `PUBLIC_KEY` env, `startSsh` |
//! | observe   | `GET /v2/pods` (cursor-paginated) by name `gpuprov-<vm>`; SSH user/port from `ssh.direct` |
//! | delete    | `DELETE /v2/pods/{id}` (404 = already gone) |
//! | cleanup   | nothing per-VM |
//!
//! SSH: the official `runpod/*` images start sshd and authorize `$PUBLIC_KEY`
//! at boot, so passing our key in the pod's environment is enough — no
//! custom start command, and no change to the account's registered keys.
//! The login user is `root` on a random public port, reported by the API.
//!
//! Pods are containers without systemd (no k3s), publish ports on random
//! public ports (no `--port`), and the REST API has no interruptible pods
//! (no `--spot`); these are declared as unsupported features.

pub mod api;
pub mod catalog;
pub mod error;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::RunpodConfig;
use crate::provider::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceRequest, SshEndpoint,
};

use api::{Api, Cloud, CreatePod, GpuRequest, Pod, StockQuery};
use catalog::OfferDetail;
use error::Op;

pub const PROVIDER_ID: &str = "runpod";

const DEFAULT_BASE_URL: &str = "https://api.runpod.io";
/// Official image with CUDA 12.9 that starts sshd from `$PUBLIC_KEY`.
pub const DEFAULT_IMAGE: &str = "runpod/base:1.4.1-cuda1290-ubuntu2204";
const API_KEY_ENV: &str = "RUNPOD_API_KEY";
/// The runpod CLI's credentials file, relative to the home directory.
const CREDENTIALS_PATH: &str = ".runpod/config.toml";

/// Image pulls on a fresh host can take many minutes.
const BOOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] = &[
    (
        Feature::OpenPorts,
        "pod ports are published on random public ports",
    ),
    (Feature::Spot, "the REST API has no interruptible pods"),
    (
        Feature::Kubernetes,
        "pods are containers without systemd, so k3s cannot run",
    ),
];

/// `~/.runpod/config.toml` as written by the runpod CLI: the key under
/// `[default]`, or at the top level.
#[derive(Debug, Default, Deserialize)]
struct RunpodCliConfig {
    api_key: Option<String>,
    #[serde(default)]
    default: Option<RunpodCliProfile>,
}

#[derive(Debug, Default, Deserialize)]
struct RunpodCliProfile {
    api_key: Option<String>,
}

fn read_cli_api_key(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let cfg: RunpodCliConfig = toml::from_str(&text).ok()?;
    cfg.default
        .and_then(|p| p.api_key)
        .or(cfg.api_key)
        .filter(|k| !k.is_empty())
}

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    /// Newline-separated authorized keys for the pod's `PUBLIC_KEY`.
    public_key: String,
}

pub struct RunpodProvider {
    api: Api,
    image: String,
}

impl RunpodProvider {
    /// Resolve the API key: config `api_key` → `$RUNPOD_API_KEY` →
    /// `~/.runpod/config.toml`.
    pub fn new(config: &RunpodConfig) -> Result<Self, ProviderError> {
        let env_key = std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty());
        let api_key = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or(env_key)
            .or_else(|| dirs::home_dir().and_then(|h| read_cli_api_key(&h.join(CREDENTIALS_PATH))))
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no RunPod API key: set runpod.api_key in the config file, ${API_KEY_ENV}, \
                     or run `runpod config` (~/{CREDENTIALS_PATH})"
                ))
            })?;
        Self::with_api(
            config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL),
            &api_key,
            config.image.as_deref().unwrap_or(DEFAULT_IMAGE),
        )
    }

    /// Construct against an explicit endpoint (tests, staging).
    pub fn with_api(base_url: &str, api_key: &str, image: &str) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, api_key).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(RunpodProvider {
            api,
            image: image.to_string(),
        })
    }
}

/// Pod name carrying the VM identity.
fn pod_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// `[type, material]` of an authorized_keys line (comments ignored).
fn key_material(line: &str) -> Vec<&str> {
    line.split_whitespace().take(2).collect()
}

/// Our key first, then the account's registered keys, without duplicates.
fn merge_keys(ours: &str, account: &[String]) -> String {
    let mut lines: Vec<&str> = vec![ours.trim()];
    for key in account.iter().map(|k| k.trim()) {
        if !key.is_empty() && !lines.iter().any(|l| key_material(l) == key_material(key)) {
            lines.push(key);
        }
    }
    lines.join("\n")
}

/// Normalize a pod; `None` for terminated pods (gone for our purposes).
fn to_observed(pod: &Pod) -> Option<Observed> {
    let status = match pod.status.as_str() {
        "TERMINATED" => return None,
        "RUNNING" => InstanceStatus::Running,
        "EXITED" => InstanceStatus::Failed("container exited".into()),
        "ERROR" => InstanceStatus::Failed("pod error".into()),
        // PROVISIONING, STARTING, and anything new.
        _ => InstanceStatus::Pending,
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| ssh_endpoint(pod))
        .flatten();
    Some(Observed {
        instance_id: pod.id.clone(),
        status,
        zone: pod.data_center_id.clone(),
        private_ip: None,
        endpoint,
    })
}

/// The pod's direct SSH endpoint; falls back to the live `22/tcp` mapping
/// (as `root`) while `ssh.direct` is not yet populated.
fn ssh_endpoint(pod: &Pod) -> Option<SshEndpoint> {
    if let Some(direct) = pod.ssh.as_ref().and_then(|s| s.direct.as_ref()) {
        return Some(SshEndpoint {
            host: direct.host.clone(),
            port: direct.port,
            user: Some(direct.username.clone()),
        });
    }
    pod.runtime
        .as_ref()?
        .ports
        .iter()
        .find(|p| p.private == Some(22))
        .and_then(|p| {
            Some(SshEndpoint {
                host: p.ip.clone()?,
                port: p.public?,
                user: Some("root".into()),
            })
        })
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone())
        .map_err(|e| ProviderError::invalid(format!("offer {offer} is not a RunPod offer: {e}")))
}

#[async_trait]
impl CloudProvider for RunpodProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        // Pods take no user data; the SSH key travels in the environment.
        Capabilities {
            unsupported: UNSUPPORTED,
            boot_script: false,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: DELETE_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let (countries, _) = catalog::split_regions(&request.regions);
        let count = catalog::GpuAsk::from_request(&request.ask).count();
        let mut stock = Vec::with_capacity(Cloud::ALL.len());
        for cloud in Cloud::ALL {
            let query = StockQuery {
                count,
                cloud,
                country_codes: &countries,
            };
            let gpus = self
                .api
                .gpu_types(&query)
                .await
                .map_err(|e| e.into_provider_error(Op::Plan))?;
            stock.push((cloud, gpus));
        }
        let continents: HashMap<String, String> = self
            .api
            .data_centers()
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?
            .into_iter()
            .map(|dc| (dc.id, dc.region))
            .collect();
        catalog::offers_from(&stock, &continents, request)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        // Best effort: also authorize the account's own keys so the user can
        // reach the pod with them. Never fatal — our key is what we need.
        let account_keys = match self.api.account_ssh_keys().await {
            Ok(keys) => keys,
            Err(e) => {
                tracing::debug!("not reading RunPod account SSH keys: {e:?}");
                Vec::new()
            }
        };
        // Re-check stock so a stale plan fails before anything is created.
        let gpu = self
            .api
            .gpu_type(
                &detail.gpu_id,
                &StockQuery {
                    count: detail.count,
                    cloud: detail.cloud,
                    country_codes: &[],
                },
            )
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        let in_stock = gpu
            .data_centers
            .iter()
            .any(|dc| dc.id == detail.data_center_id && dc.availability != api::Availability::None);
        if !in_stock {
            return Err(ProviderError::unavailable(format!(
                "{} x{} is no longer in stock in {}",
                detail.gpu_id, detail.count, detail.data_center_id
            )));
        }
        let prepared = Prepared {
            public_key: merge_keys(&spec.ssh_public_key, &account_keys),
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
        let body = CreatePod {
            name: pod_name(&spec.vm_name),
            image: spec.image_id.clone().unwrap_or_else(|| self.image.clone()),
            disk: spec.disk_gb,
            ports: vec!["22/tcp".into()],
            env: BTreeMap::from([("PUBLIC_KEY".to_string(), prepared.public_key)]),
            start_ssh: true,
            cloud: detail.cloud,
            data_center_ids: vec![detail.data_center_id],
            gpu: GpuRequest {
                id: detail.gpu_id,
                count: detail.count,
            },
        };
        let id = self
            .api
            .create_pod(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("[{}] created pod {id}", offer.location());
        Ok(id)
    }

    /// Pods are account-wide, so `at` is ignored and pods are matched by name.
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let name = pod_name(vm_name);
        let pods = self
            .api
            .list_pods()
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        let mut observed: Vec<Observed> = pods
            .iter()
            .filter(|p| p.name == name)
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
        for id in instance_ids {
            self.api
                .delete_pod(id)
                .await
                .map_err(|e| match e.status() {
                    // The pod is mid-transition (e.g. still starting): try again.
                    Some(409) => ProviderError::transient(format!("pod {id} busy: {e:?}")),
                    _ => e.into_provider_error(Op::Manage),
                })?;
        }
        Ok(())
    }

    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
