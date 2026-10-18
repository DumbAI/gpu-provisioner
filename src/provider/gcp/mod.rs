//! GCP adapter (Compute Engine v1 REST): firewall/image preparation and
//! instance create/query/delete behind the provider contract.
//!
//! Primitive mapping:
//! - `plan`: static GPU → machine-type ladder (`catalog`), expanded per
//!   **zone** (GCP capacity and instances are zonal) using live
//!   `regions.get` (zones of a region) and `aggregated/machineTypes` /
//!   `aggregated/acceleratorTypes` (zones actually offering the type) — the
//!   analog of AWS's `DescribeInstanceTypeOfferings` filter.
//! - `prepare`: per-VM firewall rule `gpuprov-<vm>` (tcp:22 + requested
//!   ports → the instance's network tag) and image resolution (Deep Learning
//!   VM family for GPU types, Ubuntu 22.04 otherwise).
//! - `create`: `instances.insert` + bounded `operations.wait` — zonal
//!   stockouts only surface in the finished operation.
//! - `observe` / `delete` / `cleanup`: label-filtered lists, instance delete,
//!   firewall delete.
//!
//! Design choices: plain REST via reqwest (no generated API client), and auth
//! by OAuth access token (config / env / `gcloud auth print-access-token`)
//! rather than service-account key signing, to keep the binary free of crypto
//! dependencies (see `api`).

mod api;
mod catalog;
mod error;
mod instance;

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};

use crate::config::GcpConfig;
use crate::provider::{
    Capabilities, CloudProvider, InstanceSpec, Location, Observed, Offer, ProviderError,
    ResourceAsk, ResourceRequest,
};

use api::{Api, Instance, Operation, TokenSource};
use catalog::TypeChoice;
use error::{classify_http, classify_operation, ApiError, Phase};

pub const PROVIDER_ID: &str = "gcp";

/// Default image for GPU machine types: Deep Learning VM "Base" with CUDA 12.9
/// and NVIDIA driver 580 on Ubuntu 22.04 (supported to Aug 2028; see
/// https://cloud.google.com/deep-learning-vm/docs/images).
pub const DEFAULT_GPU_IMAGE: &str =
    "projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2204-nvidia-580";
/// Default image for CPU machine types.
pub const DEFAULT_CPU_IMAGE: &str = "projects/ubuntu-os-cloud/global/images/family/ubuntu-2204-lts";

pub struct GcpProvider {
    api: Api,
}

impl GcpProvider {
    /// Resolve project and token source. Project: config → `$GOOGLE_CLOUD_PROJECT`
    /// / `$CLOUDSDK_CORE_PROJECT` → `gcloud config get-value project`.
    /// Token: config → `$GOOGLE_OAUTH_ACCESS_TOKEN` → `gcloud` (lazily, on
    /// first call, so a broken gcloud surfaces as a credentials error there).
    pub fn new(config: &GcpConfig) -> Result<Self, ProviderError> {
        let project = config
            .project
            .clone()
            .or_else(|| non_empty_env("GOOGLE_CLOUD_PROJECT"))
            .or_else(|| non_empty_env("CLOUDSDK_CORE_PROJECT"))
            .or_else(gcloud_project)
            .ok_or_else(|| {
                ProviderError::credentials(
                    "no GCP project: set [gcp] project in the config file, \
                     $GOOGLE_CLOUD_PROJECT, or `gcloud config set project <id>`",
                )
            })?;
        let tokens = match config
            .access_token
            .clone()
            .or_else(|| non_empty_env("GOOGLE_OAUTH_ACCESS_TOKEN"))
        {
            Some(token) => TokenSource::Static(token),
            None => TokenSource::gcloud(),
        };
        let base_url = config.base_url.as_deref().unwrap_or(api::DEFAULT_BASE_URL);
        Ok(GcpProvider {
            api: Api::new(base_url, &project, tokens),
        })
    }

    /// Test constructor: fixed endpoint, project and token.
    #[cfg(test)]
    fn with_endpoint(base_url: &str, project: &str, token: &str) -> Self {
        GcpProvider {
            api: Api::new(base_url, project, TokenSource::Static(token.to_string())),
        }
    }

    /// Zones of a region, sorted.
    async fn region_zones(&self, region: &str) -> Result<Vec<String>, ProviderError> {
        let url = self.api.project_url(&format!("regions/{region}"));
        let value: Value = self.api.get(&url, &[]).await.map_err(|e| {
            if e.is_not_found() {
                ProviderError::invalid(format!("unknown GCP region '{region}'"))
            } else {
                classify_http(&e, Phase::Plan)
            }
        })?;
        let mut zones: Vec<String> = value
            .get("zones")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|z| catalog::basename(z).to_string())
            .collect();
        zones.sort();
        Ok(zones)
    }

    /// Zones where an aggregated resource (`machineTypes`, `acceleratorTypes`)
    /// named `name` exists.
    async fn zones_with(
        &self,
        collection: &str,
        name: &str,
    ) -> Result<BTreeSet<String>, ProviderError> {
        let url = self.api.project_url(&format!("aggregated/{collection}"));
        let filter = format!("name = \"{name}\"");
        let scopes = self
            .api
            .aggregated(&url, Some(&filter))
            .await
            .map_err(|e| classify_http(&e, Phase::Plan))?;
        Ok(scopes
            .into_iter()
            .filter(|(_, v)| {
                v.get(collection)
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty())
            })
            .filter_map(|(scope, _)| scope.strip_prefix("zones/").map(str::to_string))
            .collect())
    }

    async fn zones_offering(&self, choice: &TypeChoice) -> Result<BTreeSet<String>, ProviderError> {
        let mut zones = self
            .zones_with("machineTypes", &choice.machine_type)
            .await?;
        if let Some(acc) = &choice.accelerator {
            let with_acc = self.zones_with("acceleratorTypes", &acc.kind).await?;
            zones.retain(|z| with_acc.contains(z));
        }
        Ok(zones)
    }

    /// Ensure the per-VM firewall rule exists and allows exactly what the
    /// spec needs (patched on drift, e.g. a re-provision with new ports).
    async fn ensure_firewall(&self, tag: &str, open_ports: &[u16]) -> Result<(), ProviderError> {
        let url = self.api.project_url(&format!("global/firewalls/{tag}"));
        let wanted = instance::firewall_ports(open_ports);
        let op: Operation = match self.api.get::<Value>(&url, &[]).await {
            Ok(existing) => {
                if instance::allowed_tcp_ports(&existing) == wanted {
                    return Ok(());
                }
                let patch = json!({"allowed": [{"IPProtocol": "tcp", "ports": wanted}]});
                let value = self
                    .api
                    .send(Method::PATCH, &url, &[], Some(&patch))
                    .await
                    .map_err(|e| classify_http(&e, Phase::Create))?;
                serde_json::from_value(value).unwrap_or_default()
            }
            Err(e) if e.is_not_found() => {
                let create_url = self.api.project_url("global/firewalls");
                match self
                    .api
                    .post::<Operation>(&create_url, &instance::firewall_body(tag, open_ports))
                    .await
                {
                    Ok(op) => op,
                    // Lost a create race: the rule exists now.
                    Err(e) if e.status == 409 => return Ok(()),
                    Err(e) => return Err(classify_http(&e, Phase::Create)),
                }
            }
            Err(e) => return Err(classify_http(&e, Phase::Create)),
        };
        if op.name.is_empty() {
            return Ok(());
        }
        let op = self
            .api
            .wait_operation(op, "global")
            .await
            .map_err(|e| classify_http(&e, Phase::Create))?;
        if let Some(err) = op.first_error() {
            return Err(ProviderError::unavailable(format!(
                "firewall rule {tag}: {}: {}",
                err.code, err.message
            )));
        }
        if !op.is_done() {
            return Err(ProviderError::transient(format!(
                "firewall rule {tag} still being created"
            )));
        }
        Ok(())
    }

    /// Resolve an image (family or concrete) to its self link.
    async fn resolve_image(&self, image: &str) -> Result<String, ProviderError> {
        let url = self.api.resource_url(image);
        let value: Value = self.api.get(&url, &[]).await.map_err(|e| {
            if e.is_not_found() {
                ProviderError::unavailable(format!("image '{image}' not found"))
            } else {
                classify_http(&e, Phase::Create)
            }
        })?;
        value
            .get("selfLink")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ProviderError::unavailable(format!("image '{image}' has no selfLink")))
    }

    async fn list_instances(
        &self,
        at: &Location,
        vm_name: &str,
    ) -> Result<Vec<Instance>, ApiError> {
        let filter = instance::label_filter(vm_name);
        match &at.zone {
            Some(zone) => {
                let url = self.api.project_url(&format!("zones/{zone}/instances"));
                self.api.list_all(&url, Some(&filter)).await
            }
            None => {
                let url = self.api.project_url("aggregated/instances");
                let prefix = format!("zones/{}-", at.region);
                let mut found = Vec::new();
                for (scope, value) in self.api.aggregated(&url, Some(&filter)).await? {
                    if !scope.starts_with(&prefix) {
                        continue;
                    }
                    for item in value
                        .get("instances")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Ok(instance) = serde_json::from_value::<Instance>(item.clone()) {
                            found.push(instance);
                        }
                    }
                }
                Ok(found)
            }
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn gcloud_project() -> Option<String> {
    let out = std::process::Command::new("gcloud")
        .args(["config", "get-value", "project"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let project = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !project.is_empty() && project != "(unset)").then_some(project)
}

fn describe_ask(ask: &ResourceAsk) -> String {
    match ask {
        ResourceAsk::InstanceType(t) => format!("machine type '{t}'"),
        ResourceAsk::Gpus(g) => format!("GPU '{g}'"),
    }
}

#[async_trait]
impl CloudProvider for GcpProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        // Everything is supported; Ubuntu and Deep Learning VM images apply
        // the `user-data` metadata key with cloud-init.
        Capabilities {
            cloud_init: true,
            ..Capabilities::default()
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let choices = catalog::types_for(&request.ask)?;
        if request.regions.is_empty() {
            return Err(ProviderError::invalid(
                "no candidate regions: pass --region (repeatable) or set default regions in \
                 the config file",
            ));
        }
        let regions: Vec<String> = match request.zone.as_deref() {
            None => request.regions.clone(),
            Some(zone) => {
                let region = catalog::region_of_zone(zone).ok_or_else(|| {
                    ProviderError::invalid(format!("malformed GCP zone '{zone}'"))
                })?;
                if !request.regions.iter().any(|r| r == region) {
                    return Err(ProviderError::invalid(format!(
                        "zone '{zone}' does not belong to any candidate region ({})",
                        request.regions.join(", ")
                    )));
                }
                vec![region.to_string()]
            }
        };
        let mut region_zones = Vec::with_capacity(regions.len());
        for region in &regions {
            region_zones.push((region.clone(), self.region_zones(region).await?));
        }
        let mut offering: HashMap<String, BTreeSet<String>> = HashMap::new();
        for choice in &choices {
            let key = serde_json::to_string(choice).expect("TypeChoice serializes");
            if let Entry::Vacant(slot) = offering.entry(key) {
                slot.insert(self.zones_offering(choice).await?);
            }
        }
        let offers = catalog::build_offers(
            &choices,
            &region_zones,
            |c| {
                offering
                    .get(&serde_json::to_string(c).expect("TypeChoice serializes"))
                    .cloned()
                    .unwrap_or_default()
            },
            request.zone.as_deref(),
        );
        if offers.is_empty() {
            return Err(ProviderError::invalid(format!(
                "{} is not offered in any zone of {}",
                describe_ask(&request.ask),
                regions.join(", ")
            )));
        }
        Ok(offers)
    }

    async fn prepare(&self, offer: &Offer, spec: &InstanceSpec) -> Result<Value, ProviderError> {
        let choice = catalog::choice_of(offer);
        let tag = instance::network_tag(&spec.vm_name);
        self.ensure_firewall(&tag, &spec.open_ports).await?;
        let image = match &spec.image_id {
            Some(image) => image.as_str(),
            None if choice.gpu => DEFAULT_GPU_IMAGE,
            None => DEFAULT_CPU_IMAGE,
        };
        let image = self.resolve_image(image).await?;
        Ok(json!({"image": image, "network_tag": tag}))
    }

    async fn create(
        &self,
        offer: &Offer,
        prepared: &Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let zone = offer
            .zone
            .as_deref()
            .ok_or_else(|| ProviderError::unavailable("GCP offers must name a zone"))?;
        let (Some(image), Some(tag)) = (
            prepared.get("image").and_then(Value::as_str),
            prepared.get("network_tag").and_then(Value::as_str),
        ) else {
            return Err(ProviderError::failed("corrupt GCP prepare state"));
        };
        let name = instance::instance_name(&spec.vm_name);
        let body = instance::insert_body(zone, &catalog::choice_of(offer), image, tag, spec);
        let url = self.api.project_url(&format!("zones/{zone}/instances"));
        let op: Operation = match self.api.post(&url, &body).await {
            Ok(op) => op,
            // Already exists (an earlier attempt landed): idempotent success.
            Err(e) if e.status == 409 => return Ok(name),
            Err(e) => return Err(classify_http(&e, Phase::Create)),
        };
        let op = self
            .api
            .wait_operation(op, &format!("zones/{zone}"))
            .await
            .map_err(|e| classify_http(&e, Phase::Runtime))?;
        if let Some(err) = op.first_error() {
            return Err(classify_operation(&err.code, &err.message));
        }
        if !op.is_done() {
            // The core retries Creating, which observes first and adopts.
            return Err(ProviderError::transient(format!(
                "insert of {name} in {zone} still running"
            )));
        }
        tracing::info!("[{zone}] created instance {name}");
        Ok(name)
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let instances = self
            .list_instances(at, vm_name)
            .await
            .map_err(|e| classify_http(&e, Phase::Runtime))?;
        let mut observed: Vec<Observed> = instances.iter().map(instance::to_observed).collect();
        observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(observed)
    }

    async fn delete(
        &self,
        at: &Location,
        vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        // Instances are addressed by (zone, name); find zones when unpinned.
        let zones: HashMap<String, String> = match &at.zone {
            Some(zone) => instance_ids
                .iter()
                .map(|id| (id.clone(), zone.clone()))
                .collect(),
            None => self
                .observe(at, vm_name)
                .await?
                .into_iter()
                .filter_map(|o| o.zone.map(|z| (o.instance_id, z)))
                .collect(),
        };
        for id in instance_ids {
            let Some(zone) = zones.get(id) else {
                continue; // not found anywhere: already gone
            };
            let url = self
                .api
                .project_url(&format!("zones/{zone}/instances/{id}"));
            match self.api.send(Method::DELETE, &url, &[], None).await {
                Ok(_) => tracing::info!("[{zone}] deleting instance {id}"),
                // Gone, or a delete is already in flight.
                Err(e) if e.status == 404 || e.status == 409 => {}
                Err(e) if e.reason.as_deref() == Some("resourceNotReady") => {}
                Err(e) => return Err(classify_http(&e, Phase::Runtime)),
            }
        }
        Ok(())
    }

    async fn cleanup(&self, _at: &Location, vm_name: &str) -> Result<(), ProviderError> {
        let tag = instance::network_tag(vm_name);
        let url = self.api.project_url(&format!("global/firewalls/{tag}"));
        match self.api.send(Method::DELETE, &url, &[], None).await {
            Ok(_) => {
                tracing::info!("deleted firewall rule {tag}");
                Ok(())
            }
            Err(e) if e.status == 404 => Ok(()),
            Err(e) if e.reason.as_deref() == Some("resourceNotReady") => Err(
                ProviderError::transient(format!("firewall rule {tag} busy: {e}")),
            ),
            Err(e) => Err(classify_http(&e, Phase::Runtime)),
        }
    }
}

#[cfg(test)]
mod tests;
