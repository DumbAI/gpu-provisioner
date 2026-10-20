//! Nebius AI Cloud adapter (REST gateway over the Nebius API). A Nebius VM is
//! a Compute instance on a platform (`gpu-h100-sxm`, `gpu-b300-sxm`, …) in a
//! preset size (`1gpu-16vcpu-200gb`), inside a *project*. Projects are
//! regional — every tenant gets one per region — so a region (`eu-north1`,
//! `us-central1`, …) selects a project, and the adapter keeps the
//! region → project map (configured, or discovered from the tenant).
//!
//! Primitive mapping:
//!
//! | primitive | Nebius |
//! |-----------|--------|
//! | plan      | per project `GET /compute/v1/platforms` → platform × preset × region offers; on-demand prices from `POST /billing/v1/calculator/estimate-batch`, spot from `GET /billing/v1alpha1/spot-price`; capacity advisor `GET /capacity/v1/resource-advice` drops offers with no capacity (`catalog`) |
//! | prepare   | the project's subnet (`GET /vpc/v1/subnets`) and the boot image family |
//! | create    | `POST /compute/v1/instances` named `gpuprov-<vm>`, labeled `gpuprov-vm-name=<vm>`, VM-managed boot disk, dynamic public IP, cloud-init; then a bounded wait on the create operation (`GET /compute/v1/instances/operations/{id}`), whose status carries capacity and quota failures |
//! | observe   | `GET /compute/v1/instances?parentId=<project>` (paginated), filtered by label |
//! | delete    | `DELETE /compute/v1/instances/{id}` (the managed boot disk goes with it) |
//! | cleanup   | nothing: the VM owns no other resources |
//!
//! Instances boot with cloud-init, so the boot script (the k3s installer)
//! runs at first boot, and the login is the spec's user created by it.
//! Every VM sits in its network's default security group, which Nebius
//! defines as allowing all traffic and which cannot be changed, so extra
//! inbound ports (and the direct k3s API) need no per-VM firewall.
//!
//! Preemptible (spot) VMs follow the current spot price and are *stopped*,
//! not deleted, on preemption: `observe` reports them `Stopped`, and
//! `diagnose` names the preemption. A new instance passes through `STOPPED`
//! while its create operation runs, so a stopped instance that is still
//! reconciling counts as pending.

pub mod api;
pub mod catalog;
pub mod error;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::NebiusConfig;
use crate::provider::{
    pick_instance, Capabilities, CloudProvider, Diagnosis, ErrorKind, Finding, InstanceSpec,
    InstanceStatus, Location, Observed, Offer, ProviderError, ResourceRequest, SshEndpoint,
};
use crate::userdata;

use api::{
    Api, AttachedDisk, CreateInstance, DiskSpec, ImageFamily, Instance, ManagedDisk, Metadata,
    NetworkInterface, NewInstanceSpec, Preemptible, Resources, TokenSource,
};
use catalog::{OfferDetail, RegionCatalog};
use error::Op;

pub const PROVIDER_ID: &str = "nebius";

const TOKEN_ENV: &str = "NEBIUS_IAM_TOKEN";
const TENANT_ENV: &str = "NEBIUS_TENANT_ID";

/// Label carrying the VM identity on instances and their boot disks.
pub const LABEL_VM: &str = "gpuprov-vm-name";

/// GPU VMs (8-GPU hosts especially) can take several minutes to start.
const BOOT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Bound on waiting for the create operation, whose result carries capacity
/// and quota failures; past it, the core's boot polling takes over.
const OPERATION_WAIT: Duration = Duration::from_secs(60);
const OPERATION_POLL: Duration = Duration::from_secs(2);

/// Boot disk type for every VM.
const DISK_TYPE: &str = "NETWORK_SSD";

/// What `prepare` hands to `create` (journaled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    subnet_id: String,
    /// Exactly one of family / id.
    image_family: Option<String>,
    image_id: Option<String>,
}

/// The tenant's region → project map, once known.
#[derive(Debug, Clone, Default)]
struct Projects {
    tenant_id: Option<String>,
    by_region: BTreeMap<String, String>,
}

pub struct NebiusProvider {
    api: Api,
    /// Configured tenant (config / env), if any.
    tenant_id: Option<String>,
    /// Configured projects; authoritative when non-empty.
    pinned: BTreeMap<String, String>,
    discovered: Mutex<Option<Projects>>,
    /// Bound and poll interval of the create-operation wait.
    operation_wait: (Duration, Duration),
}

impl NebiusProvider {
    /// Resolve credentials: `token` → `$NEBIUS_IAM_TOKEN` → the `nebius`
    /// CLI (`profile`); tenant: `tenant_id` → `$NEBIUS_TENANT_ID` →
    /// discovered on first use.
    pub fn new(config: &NebiusConfig) -> Result<Self, ProviderError> {
        let static_token = config
            .token
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty()));
        let token = match static_token {
            Some(token) => TokenSource::Static(token),
            None => TokenSource::cli(config.profile.clone()),
        };
        let tenant_id = config
            .tenant_id
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| std::env::var(TENANT_ENV).ok().filter(|t| !t.is_empty()));
        Self::build(
            config.base_url.as_deref().unwrap_or(api::DEFAULT_BASE_URL),
            token,
            tenant_id,
            config.projects.clone(),
        )
    }

    /// Construct against an explicit endpoint with a static token (tests,
    /// staging); never reads the environment or the CLI.
    pub fn with_api(
        base_url: &str,
        token: &str,
        tenant_id: Option<&str>,
        projects: BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        Self::build(
            base_url,
            TokenSource::Static(token.to_string()),
            tenant_id.map(str::to_string),
            projects,
        )
    }

    fn build(
        base_url: &str,
        token: TokenSource,
        tenant_id: Option<String>,
        pinned: BTreeMap<String, String>,
    ) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, token).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(NebiusProvider {
            api,
            tenant_id,
            pinned,
            discovered: Mutex::new(None),
            operation_wait: (OPERATION_WAIT, OPERATION_POLL),
        })
    }

    /// Shorten the create-operation wait (tests).
    #[cfg(test)]
    fn with_operation_wait(mut self, wait: Duration, poll: Duration) -> Self {
        self.operation_wait = (wait, poll);
        self
    }

    /// The region → project map (and the tenant, when known): the pinned
    /// projects, else the tenant's active projects, discovered once.
    async fn projects(&self, op: Op) -> Result<Projects, ProviderError> {
        if !self.pinned.is_empty() {
            return Ok(Projects {
                tenant_id: self.tenant_id.clone(),
                by_region: self.pinned.clone(),
            });
        }
        if let Some(known) = self.discovered.lock().expect("projects cache").clone() {
            return Ok(known);
        }
        let tenant_id = match &self.tenant_id {
            Some(tenant) => tenant.clone(),
            None => self.only_tenant(op).await?,
        };
        let projects = self
            .api
            .projects(&tenant_id)
            .await
            .map_err(|e| e.into_provider_error(op))?;
        let found = Projects {
            by_region: projects_by_region(&projects),
            tenant_id: Some(tenant_id.clone()),
        };
        if found.by_region.is_empty() {
            return Err(ProviderError::invalid(format!(
                "tenant {tenant_id} has no active Nebius project (create one in the console, \
                 or pin one with [nebius] projects)"
            )));
        }
        *self.discovered.lock().expect("projects cache") = Some(found.clone());
        Ok(found)
    }

    /// The caller's tenant when it has exactly one.
    async fn only_tenant(&self, op: Op) -> Result<String, ProviderError> {
        let tenants = self
            .api
            .tenants()
            .await
            .map_err(|e| e.into_provider_error(op))?;
        match tenants.as_slice() {
            [one] => Ok(one.metadata.id.clone()),
            [] => Err(ProviderError::invalid(format!(
                "no Nebius tenant is visible to these credentials; set [nebius] tenant_id \
                 (or ${TENANT_ENV}) or pin projects with [nebius] projects"
            ))),
            many => Err(ProviderError::invalid(format!(
                "these credentials see {} Nebius tenants ({}); choose one with [nebius] \
                 tenant_id or ${TENANT_ENV}",
                many.len(),
                many.iter()
                    .map(|t| t.metadata.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The project of `region`.
    async fn project_for(&self, region: &str, op: Op) -> Result<String, ProviderError> {
        let projects = self.projects(op).await?;
        projects.by_region.get(region).cloned().ok_or_else(|| {
            ProviderError::invalid(format!(
                "no Nebius project in region '{region}' (projects exist in: {})",
                projects
                    .by_region
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    /// Each selected region's platforms. A region whose listing fails is
    /// skipped (and logged) unless every region fails or the failure is
    /// about credentials.
    async fn catalogs(
        &self,
        regions: &BTreeMap<String, String>,
    ) -> Result<Vec<RegionCatalog>, ProviderError> {
        let mut catalogs = Vec::new();
        let mut last_error = None;
        for (region, project_id) in regions {
            match self.api.platforms(project_id).await {
                Ok(platforms) => catalogs.push(RegionCatalog {
                    region: region.clone(),
                    project_id: project_id.clone(),
                    platforms,
                }),
                Err(e) => {
                    let err = e.into_provider_error(Op::Plan);
                    if err.kind == ErrorKind::Credentials {
                        return Err(err);
                    }
                    tracing::warn!("skipping Nebius region {region}: {}", err.msg);
                    last_error = Some(err);
                }
            }
        }
        match (catalogs.is_empty(), last_error) {
            (true, Some(err)) => Err(err),
            _ => Ok(catalogs),
        }
    }

    /// The VM's instances in a project, matched by label (or by name, for
    /// an instance whose labels were lost).
    async fn vm_instances(
        &self,
        at: &Location,
        vm_name: &str,
    ) -> Result<Vec<Instance>, ProviderError> {
        let project_id = self.project_for(&at.region, Op::Manage).await?;
        let instances = self
            .api
            .instances(&project_id)
            .await
            .map_err(|e| e.into_provider_error(Op::Manage))?;
        let name = instance_name(vm_name);
        Ok(instances
            .into_iter()
            .filter(|i| {
                i.metadata.labels.get(LABEL_VM).map(String::as_str) == Some(vm_name)
                    || i.metadata.name == name
            })
            .collect())
    }

    /// Follow the create operation until it finishes or `OPERATION_WAIT`
    /// passes. A failed operation is the launch's error; one still running
    /// is left to the core's boot polling.
    async fn await_create(&self, mut operation: api::Operation) -> Result<String, ProviderError> {
        let (wait, poll) = self.operation_wait;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if operation.is_done() {
                if let Some(err) = operation.error() {
                    return Err(err.into_provider_error(Op::LaunchResult));
                }
                break;
            }
            if tokio::time::Instant::now() >= deadline || operation.id.is_empty() {
                break;
            }
            tokio::time::sleep(poll).await;
            match self.api.instance_operation(&operation.id).await {
                Ok(next) => operation = next,
                Err(e) => {
                    tracing::debug!("polling create operation {}: {e:?}", operation.id);
                    break;
                }
            }
        }
        if operation.resource_id.is_empty() {
            return Err(ProviderError::failed(format!(
                "create operation {} did not name the new instance",
                operation.id
            )));
        }
        Ok(operation.resource_id)
    }
}

/// Active projects by region, deterministic when a region has several (the
/// lowest id wins, so `plan` and `observe` agree).
fn projects_by_region(projects: &[api::Container]) -> BTreeMap<String, String> {
    let mut by_region: BTreeMap<String, String> = BTreeMap::new();
    for project in projects.iter().filter(|p| p.is_active()) {
        let region = project.region();
        if region.is_empty() || project.metadata.id.is_empty() {
            continue;
        }
        by_region
            .entry(region.to_string())
            .and_modify(|id| {
                if project.metadata.id < *id {
                    *id = project.metadata.id.clone();
                }
            })
            .or_insert_with(|| project.metadata.id.clone());
    }
    by_region
}

/// The regions a request may use: its explicit regions, or a zone pin
/// (Nebius has no zones, so it names a region), or every region with a
/// project.
fn select_regions(
    projects: &BTreeMap<String, String>,
    request: &ResourceRequest,
) -> Result<BTreeMap<String, String>, ProviderError> {
    let wanted: Vec<String> = match &request.zone {
        Some(zone) => vec![zone.clone()],
        None => request.regions.clone(),
    };
    if wanted.is_empty() {
        return Ok(projects.clone());
    }
    let selected: BTreeMap<String, String> = projects
        .iter()
        .filter(|(region, _)| wanted.iter().any(|w| w.eq_ignore_ascii_case(region)))
        .map(|(r, p)| (r.clone(), p.clone()))
        .collect();
    if selected.is_empty() {
        return Err(ProviderError::invalid(format!(
            "no Nebius project in {} (projects exist in: {})",
            wanted.join(", "),
            projects.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(selected)
}

/// Name of the VM's instance.
fn instance_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// Normalize an instance; `None` for a state that means nothing is left.
fn to_observed(instance: &Instance) -> Option<Observed> {
    let status = match instance.status.state.as_str() {
        "RUNNING" => InstanceStatus::Running,
        "STOPPING" => InstanceStatus::Stopping,
        // A new instance goes STOPPED → STARTING while its create operation
        // runs; only a settled stop is a stop.
        "STOPPED" if instance.status.reconciling => InstanceStatus::Pending,
        "STOPPED" => InstanceStatus::Stopped,
        "DELETING" => InstanceStatus::Terminating,
        "ERROR" => InstanceStatus::Failed("Nebius reports the VM in ERROR".into()),
        // CREATING, STARTING, UPDATING, and anything new to the API.
        _ => InstanceStatus::Pending,
    };
    let endpoint = (status == InstanceStatus::Running)
        .then(|| instance.public_ip())
        .flatten()
        .map(|host| SshEndpoint {
            host,
            port: 22,
            // The spec's user, created by cloud-init.
            user: None,
        });
    Some(Observed {
        instance_id: instance.metadata.id.clone(),
        status,
        zone: None,
        private_ip: instance.private_ip(),
        endpoint,
    })
}

/// Nebius-native health signals for one instance. Pure.
fn findings_for(instance: &Instance) -> Vec<Finding> {
    let mut findings = Vec::new();
    let settled_stop = instance.status.state == "STOPPED" && !instance.status.reconciling;
    if instance.is_preemptible() {
        if settled_stop {
            findings.push(Finding::critical(
                "preempted — Nebius stops preemptible VMs instead of deleting them; the \
                 boot disk keeps billing and quota stays held until `gpuprov down`",
            ));
        } else {
            findings.push(Finding::info(
                "preemptible VM (Nebius may stop it when capacity or the spot price requires)",
            ));
        }
    } else if settled_stop {
        findings.push(Finding::critical(
            "stopped — compute is not billed, but the boot disk is and quota stays held",
        ));
    }
    if !instance.status.maintenance_event_id.is_empty() {
        findings.push(Finding::warn(format!(
            "Nebius maintenance event {} affects this VM",
            instance.status.maintenance_event_id
        )));
    }
    findings
}

/// The VM's diagnosis from its raw instances. Pure.
fn diagnosis_from(instances: &[Instance], recorded: Option<&str>) -> Diagnosis {
    let observed: Vec<Observed> = instances.iter().filter_map(to_observed).collect();
    let instance = pick_instance(observed, recorded);
    let findings = instance
        .as_ref()
        .and_then(|o| instances.iter().find(|i| i.metadata.id == o.instance_id))
        .map(findings_for)
        .unwrap_or_default();
    Diagnosis { instance, findings }
}

/// An image override: a `computeimage-…` id, else an image family.
fn image_override(image: &str) -> (Option<String>, Option<String>) {
    if image.starts_with("computeimage-") {
        (None, Some(image.to_string()))
    } else {
        (Some(image.to_string()), None)
    }
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone())
        .map_err(|e| ProviderError::invalid(format!("offer {offer} is not a Nebius offer: {e}")))
}

/// The instance create request for an offer.
fn create_request(
    detail: &OfferDetail,
    prepared: &Prepared,
    spec: &InstanceSpec,
) -> CreateInstance {
    let labels: BTreeMap<String, String> = [(LABEL_VM.to_string(), spec.vm_name.clone())].into();
    let name = instance_name(&spec.vm_name);
    CreateInstance {
        metadata: Metadata {
            parent_id: detail.project_id.clone(),
            name: name.clone(),
            labels: labels.clone(),
            ..Metadata::default()
        },
        spec: NewInstanceSpec {
            resources: Resources {
                platform: detail.platform.clone(),
                preset: detail.preset.clone(),
            },
            boot_disk: AttachedDisk {
                attach_mode: "READ_WRITE".into(),
                managed_disk: ManagedDisk {
                    name: format!("{name}-boot"),
                    labels,
                    spec: DiskSpec {
                        kind: DISK_TYPE.into(),
                        size_gibibytes: spec.disk_gb.to_string(),
                        source_image_family: prepared
                            .image_family
                            .clone()
                            .map(|image_family| ImageFamily { image_family }),
                        source_image_id: prepared.image_id.clone(),
                    },
                },
            },
            network_interfaces: vec![NetworkInterface {
                name: "eth0".into(),
                subnet_id: prepared.subnet_id.clone(),
                ip_address: serde_json::json!({}),
                public_ip_address: serde_json::json!({}),
            }],
            cloud_init_user_data: userdata::render(spec),
            preemptible: spec.spot.then(|| Preemptible {
                on_preemption: "STOP".into(),
            }),
            // Preemptible VMs support only FAIL (the VM stays stopped).
            recovery_policy: spec.spot.then(|| "FAIL".to_string()),
            follows_spot_price: spec.spot.then(|| serde_json::json!({})),
        },
    }
}

#[async_trait]
impl CloudProvider for NebiusProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: &[],
            boot_script: true,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: DELETE_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let projects = self.projects(Op::Plan).await?;
        let regions = select_regions(&projects.by_region, request)?;
        let catalogs = self.catalogs(&regions).await?;
        let candidates = catalog::candidates(&catalogs, request)?;

        let mut on_demand = BTreeMap::new();
        let mut spot = Default::default();
        if request.spot {
            match self.api.spot_prices().await {
                Ok(prices) => spot = catalog::spot_price_index(&prices),
                Err(e) => tracing::warn!("Nebius spot prices unavailable: {e:?}"),
            }
        } else {
            let configs: Vec<(String, String, String, String)> = candidates
                .iter()
                .map(|c| {
                    (
                        c.key(),
                        c.detail.project_id.clone(),
                        c.detail.platform.clone(),
                        c.detail.preset.clone(),
                    )
                })
                .collect();
            match self.api.hourly_prices(&configs).await {
                Ok(prices) => on_demand = prices,
                Err(e) => tracing::warn!("Nebius price estimates unavailable: {e:?}"),
            }
        }
        let advice = match &projects.tenant_id {
            Some(tenant) => match self.api.resource_advice(tenant).await {
                Ok(advice) => Some(advice),
                Err(e) => {
                    tracing::info!("Nebius capacity advisor unavailable, not used: {e:?}");
                    None
                }
            },
            None => None,
        };
        let offers = catalog::offers_from(
            candidates,
            request.spot,
            &on_demand,
            &spot,
            advice.as_deref(),
        );
        if offers.is_empty() {
            tracing::warn!("the Nebius capacity advisor reports no capacity for any match");
        }
        Ok(offers)
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        let mut subnets = self
            .api
            .subnets(&detail.project_id)
            .await
            .map_err(|e| e.into_provider_error(Op::Prepare))?;
        subnets.retain(|s| matches!(s.status.state.as_str(), "READY" | ""));
        // Prefer the project's default subnet; stable otherwise.
        subnets.sort_by(|a, b| {
            let default = |s: &api::Subnet| !s.metadata.name.starts_with("default");
            default(a)
                .cmp(&default(b))
                .then_with(|| a.metadata.id.cmp(&b.metadata.id))
        });
        let Some(subnet) = subnets.first() else {
            return Err(ProviderError::unavailable(format!(
                "project {} in {} has no ready subnet (the console creates a default \
                 network per project)",
                detail.project_id, detail.region
            )));
        };
        let (image_family, image_id) = match &spec.image_id {
            Some(image) => image_override(image),
            None => (Some(detail.image_family.clone()), None),
        };
        let prepared = Prepared {
            subnet_id: subnet.metadata.id.clone(),
            image_family,
            image_id,
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
        let operation = self
            .api
            .create_instance(&create_request(&detail, &prepared, spec))
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        let id = self.await_create(operation).await?;
        tracing::info!("[{}] created instance {id}", offer.location());
        Ok(id)
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let mut observed: Vec<Observed> = self
            .vm_instances(at, vm_name)
            .await?
            .iter()
            .filter_map(to_observed)
            .collect();
        observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Ok(observed)
    }

    /// Adds preemption, stopped-VM and maintenance findings.
    async fn diagnose(
        &self,
        at: &Location,
        vm_name: &str,
        instance_id: Option<&str>,
    ) -> Result<Diagnosis, ProviderError> {
        let instances = self.vm_instances(at, vm_name).await?;
        Ok(diagnosis_from(&instances, instance_id))
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        // By id (ids are globally unique); an unknown one is already gone.
        for id in instance_ids {
            self.api
                .delete_instance(id)
                .await
                .map_err(|e| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }

    /// Nothing to remove: the boot disk is VM-managed (deleted with the
    /// instance) and the subnet and security group are the project's own.
    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
