//! Stateful stub of the **Nebius AI Cloud REST API** (`https://api.nebius.cloud`),
//! limited to the endpoints gpuprov's Nebius adapter uses.
//!
//! Vendor reference: <https://docs.nebius.com/rest-api> (OpenAPI:
//! <https://api.nebius.cloud/openapi.json>; protos:
//! <https://github.com/nebius/api>). Shapes follow that spec — the protobuf
//! JSON mapping (camelCase, enum names, `int64` as strings), `items` +
//! `nextPageToken` lists, long-running `Operation`s, and
//! `{"code", "message", "details": [ServiceError]}` errors with gRPC
//! canonical codes — not gpuprov's parser. Every request must carry
//! `Authorization: Bearer <token>` (else `401 {"code": 16}`), except the
//! spot-price list, which Nebius serves without authentication.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /iam/v1/tenants` | [TenantService](https://api.nebius.cloud/openapi#/nebius.iam.v1.TenantService) | one tenant |
//! | `GET /iam/v1/projects?parentId=` | [ProjectService](https://docs.nebius.com/iam/overview) | `eu-north1` and `us-central1` projects (region in `status.region`) |
//! | `GET /compute/v1/platforms?parentId=` | [List platforms and presets](https://docs.nebius.com/compute/virtual-machines/list-platforms) | `gpu-h200-sxm` in both projects, `gpu-h100-sxm` and `cpu-d3` in eu-north1 |
//! | `POST /billing/v1/calculator/estimate-batch` | [CalculatorService](https://api.nebius.cloud/openapi#/nebius.billing.v1.CalculatorService) | hourly cost per resource spec, echoing its metadata |
//! | `GET /billing/v1alpha1/spot-price` | [Getting spot prices](https://docs.nebius.com/signup-billing/spot-prices) | per-GPU-hour prices; no auth |
//! | `GET /capacity/v1/resource-advice?parentId=` | [Capacity advisor](https://docs.nebius.com/compute/virtual-machines/capacity-advisor) | fresh `AVAILABILITY_LEVEL_HIGH` advice for every preset |
//! | `GET /vpc/v1/subnets?parentId=` | [SubnetService](https://api.nebius.cloud/openapi#/nebius.vpc.v1.SubnetService) | one `READY` default subnet per project |
//! | `POST /compute/v1/instances` | [Create a VM](https://docs.nebius.com/compute/virtual-machines/create) | validates the body; returns a finished `Operation` naming the instance; duplicate name → `409 {code: 6}`; capacity fault → finished operation with `status {code: 8, NotEnoughResources}` and nothing created |
//! | `GET /compute/v1/instances?parentId=` | [InstanceService](https://api.nebius.cloud/openapi#/nebius.compute.v1.InstanceService) | the project's instances |
//! | `GET /compute/v1/instances/operations/{id}` | [OperationService](https://api.nebius.cloud/openapi#/nebius.compute.v1.InstanceService) | stored operations; unknown → `404 {code: 5}` |
//! | `DELETE /compute/v1/instances/{id}` | [Delete a VM](https://docs.nebius.com/compute/virtual-machines/delete) | `DELETING`, then gone with its managed boot disk; unknown → `404 {code: 5}` |
//!
//! Every list serves **one item per page** so the adapter's paging runs on
//! every call. Instances follow the documented lifecycle
//! (<https://docs.nebius.com/compute/virtual-machines/lifecycle>): created
//! `STOPPED` while reconciling, then `STARTING`, then `RUNNING` with private
//! and public addresses in CIDR form (`/32`) — one transition per first-page
//! list. Preemptible VMs need `recoveryPolicy: FAIL` and a spot pricing
//! choice (documented), and are *stopped* on preemption.
//!
//! Documented assumptions (not in the spec): that the create operation
//! finishes before the VM boots, and that a capacity failure arrives as the
//! operation's status rather than as the create's HTTP answer.

use std::collections::HashMap;

use async_trait::async_trait;
use gpu_provisioner::config::{AppConfig, NebiusConfig};
use serde_json::{json, Value};
use wiremock::{Request, ResponseTemplate};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, last_segment, query, Ids, StubServer};

const TOKEN: &str = "nebius-stub-token";
const TENANT: &str = "tenant-e00stub";
/// `(region, project)`.
const PROJECTS: &[(&str, &str)] = &[
    ("eu-north1", "project-e00north"),
    ("us-central1", "project-e00central"),
];

#[derive(Debug)]
struct Instance {
    id: String,
    project: String,
    name: String,
    labels: Value,
    platform: String,
    preset: String,
    preemptible: bool,
    state: &'static str,
    reconciling: bool,
    /// Held in `STARTING` until released.
    held: bool,
    /// Lists left in `DELETING`.
    deleting: Option<u32>,
    ip: String,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    instances: Vec<Instance>,
    operations: HashMap<String, Value>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    boot_payloads: Vec<String>,
    /// Create bodies as received.
    create_bodies: Vec<Value>,
}

/// Simulated Nebius tenant. See the module docs for the endpoint map.
pub struct NebiusStub {
    server: StubServer<Account>,
}

/// `{"code", "message", "details"}` with a `ServiceError` detail.
fn error(http: u16, code: i32, service_code: &str, message: &str) -> ResponseTemplate {
    json(
        http,
        json!({"code": code, "message": message, "details": [{
            "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
            "service": "compute", "code": service_code, "retryType": "NOTHING"
        }]}),
    )
}

fn not_found(id: &str) -> ResponseTemplate {
    json(
        404,
        json!({"code": 5, "message": format!("resource {id} not found"), "details": [{
            "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
            "service": "compute", "code": "NotFound", "resourceNotFound": {"resourceId": id}
        }]}),
    )
}

fn bad_request(field: &str, message: &str) -> ResponseTemplate {
    json(
        400,
        json!({"code": 3, "message": format!("{field} is invalid"), "details": [{
            "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
            "service": "compute", "code": "BadRequest",
            "badRequest": {"violations": [{"field": field, "message": message}]},
            "retryType": "NOTHING"
        }]}),
    )
}

fn unauthorized(req: &Request) -> Option<ResponseTemplate> {
    let ok = header(req, "authorization").as_deref() == Some(&format!("Bearer {TOKEN}"));
    (!ok).then(|| json(401, json!({"code": 16, "message": "invalid token"})))
}

/// One page of `items` (one item per page; the token is the next index).
fn page(req: &Request, items: Vec<Value>) -> ResponseTemplate {
    let start: usize = query(req, "pageToken")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    let chunk: Vec<Value> = items.iter().skip(start).take(1).cloned().collect();
    let next = if start + 1 < items.len() {
        (start + 1).to_string()
    } else {
        String::new()
    };
    json(200, json!({"items": chunk, "nextPageToken": next}))
}

fn project_region(project: &str) -> Option<&'static str> {
    PROJECTS
        .iter()
        .find(|(_, p)| *p == project)
        .map(|(r, _)| *r)
}

/// A simulated platform with its presets (`(name, GPUs)`).
struct PlatformDef {
    name: &'static str,
    human: &'static str,
    presets: Vec<(&'static str, u32)>,
    preemptible: bool,
}

/// The platforms each region's project offers.
fn platforms_in(region: &str) -> Vec<PlatformDef> {
    let h200 = || PlatformDef {
        name: "gpu-h200-sxm",
        human: "NVIDIA® H200 NVLink with Intel Sapphire Rapids",
        presets: vec![("1gpu-16vcpu-200gb", 1), ("8gpu-128vcpu-1600gb", 8)],
        preemptible: true,
    };
    match region {
        "eu-north1" => vec![
            h200(),
            PlatformDef {
                name: "gpu-h100-sxm",
                human: "NVIDIA® H100 NVLink with Intel Sapphire Rapids",
                presets: vec![("1gpu-16vcpu-200gb", 1)],
                preemptible: true,
            },
            PlatformDef {
                name: "cpu-d3",
                human: "Non-GPU AMD EPYC Genoa",
                presets: vec![("4vcpu-16gb", 0)],
                preemptible: false,
            },
        ],
        "us-central1" => vec![h200()],
        _ => Vec::new(),
    }
}

fn platform_json(project: &str, platform: &PlatformDef) -> Value {
    let presets: Vec<Value> = platform
        .presets
        .iter()
        .map(|(preset, gpus)| {
            json!({"name": preset, "allowGpuClustering": *gpus == 8,
                   "resources": {"vcpuCount": 16 * gpus.max(&1), "memoryGibibytes": 200 * gpus.max(&1),
                                 "gpuCount": gpus, "gpuMemoryGibibytes": 141}})
        })
        .collect();
    json!({
        "metadata": {"id": format!("computeplatform-e00{}", platform.name), "parentId": project,
                     "name": platform.name},
        "spec": {"humanReadableName": platform.human, "presets": presets, "allowPresetChange": false,
                 "gpuMemoryGibibytes": 141},
        "status": {"allowedForPreemptibles": platform.preemptible}
    })
}

/// Hourly on-demand price of a configuration.
fn hourly(region: &str, platform: &str, preset: &str) -> f64 {
    let gpus: f64 = preset
        .split("gpu")
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0.0);
    let per_gpu = match (platform, region) {
        ("gpu-h200-sxm", "eu-north1") => 3.50,
        ("gpu-h200-sxm", _) => 3.60,
        ("gpu-h100-sxm", _) => 2.95,
        _ => 0.0,
    };
    if gpus > 0.0 {
        per_gpu * gpus
    } else {
        0.12
    }
}

fn instance_json(i: &Instance) -> Value {
    let interfaces = if i.state == "RUNNING" {
        json!([{"name": "eth0", "index": 0, "macAddress": "aa:bb:cc:dd:ee:ff",
                "ipAddress": {"address": "192.168.0.10/32", "allocationId": "vpcallocation-e00p"},
                "publicIpAddress": {"address": format!("{}/32", i.ip), "allocationId": "vpcallocation-e00q", "static": false},
                "fqdn": format!("{}.internal", i.name)}])
    } else {
        json!([])
    };
    let mut spec = json!({
        "resources": {"platform": i.platform, "preset": i.preset},
        "bootDisk": {"attachMode": "READ_WRITE", "managedDisk": {"name": format!("{}-boot", i.name)}},
        "networkInterfaces": [{"name": "eth0", "subnetId": format!("vpcsubnet-e00{}", i.project), "ipAddress": {}, "publicIpAddress": {}}],
        "recoveryPolicy": if i.preemptible { "FAIL" } else { "RECOVER" },
    });
    if i.preemptible {
        spec["preemptible"] = json!({"onPreemption": "STOP"});
        spec["followsSpotPrice"] = json!({});
    }
    json!({
        "metadata": {"id": i.id, "parentId": i.project, "name": i.name, "labels": i.labels,
                     "resourceVersion": "1", "createdAt": "2026-10-19T00:00:00Z"},
        "spec": spec,
        "status": {"state": i.state, "reconciling": i.reconciling, "networkInterfaces": interfaces,
                   "diskAttachments": [{"id": format!("computedisk-e00{}", i.id), "name": format!("{}-boot", i.name), "isManaged": true}]}
    })
}

/// A finished operation on `resource_id` (`status` = `{}` on success).
fn operation(id: &str, resource_id: &str, description: &str, status: Value) -> Value {
    json!({
        "id": id, "description": description, "createdAt": "2026-10-19T00:00:00Z",
        "createdBy": "serviceaccount-e00stub", "finishedAt": "2026-10-19T00:00:02Z",
        "resourceId": resource_id, "status": status,
    })
}

/// Validate a `CreateInstanceRequest` against the project; the error is the
/// `(field, message)` of a `400` response.
fn validate_create(spec: &Value) -> Result<(), (&'static str, String)> {
    let project = spec["metadata"]["parentId"].as_str().unwrap_or_default();
    let Some(region) = project_region(project) else {
        return Err(("metadata.parent_id", "unknown project".into()));
    };
    if spec["metadata"]["name"]
        .as_str()
        .unwrap_or_default()
        .is_empty()
    {
        return Err(("metadata.name", "required".into()));
    }
    let resources = &spec["spec"]["resources"];
    let platform = resources["platform"].as_str().unwrap_or_default();
    let preset = resources["preset"].as_str().unwrap_or_default();
    let offered = platforms_in(region)
        .into_iter()
        .any(|p| p.name == platform && p.presets.iter().any(|(s, _)| *s == preset));
    if !offered {
        return Err((
            "spec.resources",
            format!("preset {preset} of platform {platform} is not available in {region}"),
        ));
    }
    let disk = &spec["spec"]["bootDisk"];
    let managed = &disk["managedDisk"];
    let size_ok = managed["spec"]["sizeGibibytes"]
        .as_str()
        .is_some_and(|s| s.parse::<u64>().is_ok());
    let source_ok = managed["spec"]["sourceImageFamily"]["imageFamily"]
        .as_str()
        .is_some_and(|f| !f.is_empty())
        || managed["spec"]["sourceImageId"].as_str().is_some();
    if disk["attachMode"] != "READ_WRITE"
        || managed["name"].as_str().unwrap_or_default().is_empty()
        || managed["spec"]["type"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
        || !size_ok
        || !source_ok
    {
        return Err(("spec.boot_disk", "invalid managed boot disk".into()));
    }
    let nic = &spec["spec"]["networkInterfaces"][0];
    if nic["name"].as_str().unwrap_or_default().is_empty()
        || nic["subnetId"] != json!(format!("vpcsubnet-e00{project}"))
        || !nic["ipAddress"].is_object()
    {
        return Err((
            "spec.network_interfaces",
            "invalid network interface".into(),
        ));
    }
    if !spec["spec"]["preemptible"].is_null() {
        let pricing = !spec["spec"]["followsSpotPrice"].is_null()
            || !spec["spec"]["spotPricingPolicy"].is_null();
        if spec["spec"]["recoveryPolicy"] != "FAIL" || !pricing {
            return Err((
                "spec.recovery_policy",
                "preemptible VMs support only FAIL and need a spot pricing setting".into(),
            ));
        }
    }
    Ok(())
}

impl NebiusStub {
    /// Stop every instance as a preemption would.
    pub fn preempt(&self) {
        for i in self.server.state().instances.iter_mut() {
            i.state = "STOPPED";
            i.reconciling = false;
        }
    }

    /// Create bodies received so far.
    pub fn create_bodies(&self) -> Vec<Value> {
        self.server.state().create_bodies.clone()
    }
}

#[async_trait]
impl VendorStub for NebiusStub {
    const CLOUD: &'static str = "nebius";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;

        // GET /iam/v1/tenants — TenantService/List.
        server
            .route("GET", r"^/iam/v1/tenants$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                page(
                    req,
                    vec![json!({"metadata": {"id": TENANT, "name": "acme"},
                                "spec": {}, "status": {"containerState": "ACTIVE"}})],
                )
            })
            .await;
        // GET /iam/v1/projects?parentId= — ProjectService/List.
        server
            .route("GET", r"^/iam/v1/projects$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                if query(req, "parentId").as_deref() != Some(TENANT) {
                    return bad_request("parent_id", "unknown tenant");
                }
                let projects = PROJECTS
                    .iter()
                    .map(|(region, id)| {
                        json!({"metadata": {"id": id, "parentId": TENANT, "name": format!("default-project-{region}")},
                               "spec": {"region": region},
                               "status": {"containerState": "ACTIVE", "region": region}})
                    })
                    .collect();
                page(req, projects)
            })
            .await;
        // GET /compute/v1/platforms?parentId= — PlatformService/List.
        server
            .route("GET", r"^/compute/v1/platforms$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let project = query(req, "parentId").unwrap_or_default();
                let Some(region) = project_region(&project) else {
                    return bad_request("parent_id", "unknown project");
                };
                let items = platforms_in(region)
                    .into_iter()
                    .map(|p| platform_json(&project, &p))
                    .collect();
                page(req, items)
            })
            .await;
        // POST /billing/v1/calculator/estimate-batch — CalculatorService/EstimateBatch.
        server
            .route(
                "POST",
                r"^/billing/v1/calculator/estimate-batch$",
                |req, _| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let request = body(req);
                    let mut costs = Vec::new();
                    for spec in request["resourceSpecs"].as_array().into_iter().flatten() {
                        let any = &spec["spec"];
                        if any["@type"]
                            != "type.googleapis.com/nebius.compute.v1.CreateInstanceRequest"
                        {
                            return bad_request("resource_specs", "unsupported type");
                        }
                        let project = any["metadata"]["parentId"].as_str().unwrap_or_default();
                        let Some(region) = project_region(project) else {
                            return not_found(project);
                        };
                        let resources = &any["spec"]["resources"];
                        let cost = hourly(
                            region,
                            resources["platform"].as_str().unwrap_or_default(),
                            resources["preset"].as_str().unwrap_or_default(),
                        );
                        costs.push(json!({
                            "metadata": any["metadata"],
                            "aggregationUnit": {"unit": "hour", "unitType": "UNIT_TYPE_TIME_BASED"},
                            "general": {"total": {"cost": format!("{cost}"), "costRounded": format!("{cost:.2}"),
                                                  "skuCosts": []}}
                        }));
                    }
                    json(
                        200,
                        json!({"currency": "usd", "resourceCosts": costs, "totalCosts": []}),
                    )
                },
            )
            .await;
        // GET /billing/v1alpha1/spot-price — SpotPriceService/List (public).
        server
            .route("GET", r"^/billing/v1alpha1/spot-price$", |req, _| {
                let prices = [
                    ("eu-north1", "gpu-h200-sxm", "1.70"),
                    ("eu-north1", "gpu-h100-sxm", "0.79"),
                    ("us-central1", "gpu-h200-sxm", "0.79"),
                ]
                .iter()
                .enumerate()
                .map(|(n, (region, platform, value))| {
                    json!({"skuId": format!("sku-e00{n}"), "productTitle": "Preemptible NVIDIA®",
                           "region": region, "computeInstance": {"platform": platform},
                           "price": {"value": value, "currency": "usd", "unitTitle": "GPU hour"},
                           "effectiveTime": "2026-10-19T00:00:00Z"})
                })
                .collect();
                page(req, prices)
            })
            .await;
        // GET /capacity/v1/resource-advice?parentId= — ResourceAdviceService/List.
        server
            .route("GET", r"^/capacity/v1/resource-advice$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                if query(req, "parentId").as_deref() != Some(TENANT) {
                    return bad_request("parent_id", "must be a tenant");
                }
                let availability = json!({"available": "8", "limit": "16",
                                          "availabilityLevel": "AVAILABILITY_LEVEL_HIGH",
                                          "dataState": "DATA_STATE_FRESH",
                                          "effectiveAt": "2026-10-19T00:00:00Z"});
                let mut items = Vec::new();
                for (region, _) in PROJECTS {
                    for platform in platforms_in(region) {
                        for (preset, gpus) in platform.presets.into_iter().filter(|(_, g)| *g > 0) {
                            items.push(json!({
                                "metadata": {"parentId": TENANT},
                                "spec": {"region": region, "fabric": "fabric-2",
                                         "computeInstance": {"platform": platform.name,
                                                             "preset": {"name": preset, "resources": {"gpuCount": gpus}}}},
                                "status": {"onDemand": availability, "preemptible": availability}
                            }));
                        }
                    }
                }
                page(req, items)
            })
            .await;
        // GET /vpc/v1/subnets?parentId= — SubnetService/List.
        server
            .route("GET", r"^/vpc/v1/subnets$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let project = query(req, "parentId").unwrap_or_default();
                if project_region(&project).is_none() {
                    return bad_request("parent_id", "unknown project");
                }
                page(
                    req,
                    vec![json!({
                        "metadata": {"id": format!("vpcsubnet-e00{project}"), "parentId": project,
                                     "name": format!("default-subnet-{project}")},
                        "spec": {"networkId": format!("vpcnetwork-e00{project}")},
                        "status": {"state": "READY", "ipv4PrivateCidrs": ["192.168.0.0/16"]}
                    })],
                )
            })
            .await;
        // POST /compute/v1/instances — InstanceService/Create.
        server
            .route("POST", r"^/compute/v1/instances$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                if let Err((field, message)) = validate_create(&spec) {
                    return bad_request(field, &message);
                }
                let project = spec["metadata"]["parentId"].as_str().unwrap().to_string();
                let name = spec["metadata"]["name"].as_str().unwrap().to_string();
                let taken = account
                    .instances
                    .iter()
                    .any(|i| i.project == project && i.name == name && i.deleting.is_none());
                if taken {
                    return error(
                        409,
                        6,
                        "AlreadyExists",
                        &format!("instance with name {name} already exists"),
                    );
                }
                let op_id = account.ids.next("computeoperation-e00");
                let instance_id = account.ids.next("computeinstance-e00");
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    let op = operation(
                        &op_id,
                        &instance_id,
                        "create instance",
                        json!({"code": 8, "message": "Not enough resources to create the instance",
                               "details": [{"@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                                            "service": "compute", "code": "NotEnoughResources",
                                            "notEnoughResources": {"violations": [{
                                                "resourceType": "gpu",
                                                "message": "not enough gpu-h200-sxm in the region",
                                                "requested": "1"}]},
                                            "retryType": "UNIT_OF_WORK"}]}),
                    );
                    account.operations.insert(op_id, op.clone());
                    return json(200, op);
                }
                let resources = &spec["spec"]["resources"];
                let n = account.creates + 10;
                account.instances.push(Instance {
                    id: instance_id.clone(),
                    project,
                    name,
                    labels: spec["metadata"]["labels"].clone(),
                    platform: resources["platform"].as_str().unwrap().to_string(),
                    preset: resources["preset"].as_str().unwrap().to_string(),
                    preemptible: !spec["spec"]["preemptible"].is_null(),
                    state: "STOPPED",
                    reconciling: true,
                    held: account.hold_boot,
                    deleting: None,
                    ip: format!("198.51.100.{n}"),
                });
                if let Some(user_data) = spec["spec"]["cloudInitUserData"].as_str() {
                    account.boot_payloads.push(user_data.to_string());
                }
                account.create_bodies.push(spec);
                account.creates += 1;
                let op = operation(&op_id, &instance_id, "create instance", json!({}));
                account.operations.insert(op_id, op.clone());
                json(200, op)
            })
            .await;
        // GET /compute/v1/instances?parentId= — InstanceService/List.
        server
            .route("GET", r"^/compute/v1/instances$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                // Advance the simulation: one first-page list = one tick.
                if query(req, "pageToken").is_none_or(|t| t.is_empty()) {
                    account.instances.retain(|i| i.deleting != Some(0));
                    for i in account.instances.iter_mut() {
                        if let Some(left) = i.deleting.as_mut() {
                            *left -= 1;
                            continue;
                        }
                        match (i.state, i.reconciling) {
                            ("STOPPED", true) => i.state = "STARTING",
                            ("STARTING", _) if !i.held => {
                                i.state = "RUNNING";
                                i.reconciling = false;
                            }
                            _ => {}
                        }
                    }
                }
                let project = query(req, "parentId").unwrap_or_default();
                if project_region(&project).is_none() {
                    return bad_request("parent_id", "unknown project");
                }
                let items = account
                    .instances
                    .iter()
                    .filter(|i| i.project == project)
                    .map(instance_json)
                    .collect();
                page(req, items)
            })
            .await;
        // GET /compute/v1/instances/operations/{id} — OperationService/Get.
        server
            .route(
                "GET",
                r"^/compute/v1/instances/operations/[^/]+$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let id = last_segment(req);
                    match account.operations.get(&id) {
                        Some(op) => json(200, op.clone()),
                        None => not_found(&id),
                    }
                },
            )
            .await;
        // DELETE /compute/v1/instances/{id} — InstanceService/Delete.
        server
            .route(
                "DELETE",
                r"^/compute/v1/instances/[^/]+$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let id = last_segment(req);
                    let Some(instance) = account.instances.iter_mut().find(|i| i.id == id) else {
                        return not_found(&id);
                    };
                    instance.state = "DELETING";
                    instance.reconciling = true;
                    instance.deleting.get_or_insert(1);
                    let op_id = account.ids.next("computeoperation-e00");
                    let mut op = operation(&op_id, &id, "delete instance", json!({}));
                    op["finishedAt"] = Value::Null;
                    op["status"] = Value::Null;
                    account.operations.insert(op_id, op.clone());
                    json(200, op)
                },
            )
            .await;
        NebiusStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            nebius: NebiusConfig {
                token: Some(TOKEN.into()),
                tenant_id: Some(TENANT.into()),
                base_url: Some(self.server.uri()),
                ..NebiusConfig::default()
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            gpus: Some("H200:1".into()),
            ..Ask::default()
        }
    }

    fn fail_next_create_with_capacity(&self) {
        self.server.state().capacity_faults = 1;
    }

    fn hold_boot(&self, hold: bool) {
        let mut account = self.server.state();
        account.hold_boot = hold;
        for i in account.instances.iter_mut() {
            i.held = hold;
        }
    }

    fn leftovers(&self) -> Vec<String> {
        // A VM-managed boot disk lives exactly as long as its instance.
        self.server
            .state()
            .instances
            .iter()
            .map(|i| {
                format!(
                    "instance {} ({}) with boot disk {}-boot",
                    i.id, i.state, i.name
                )
            })
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        self.server.state().instances.clear();
    }

    fn boot_payloads(&self) -> Vec<String> {
        self.server.state().boot_payloads.clone()
    }

    fn expected_login(&self) -> (String, u16) {
        ("ubuntu".into(), 22)
    }
}

vendor_suite!(NebiusStub);

/// Nebius-specific: without a configured tenant, the adapter discovers the
/// caller's only tenant and maps its projects by region.
#[tokio::test]
async fn tenant_and_projects_are_discovered() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = NebiusStub::start().await;
    let mut config = stub.config();
    config.nebius.tenant_id = None;
    let provider = gpu_provisioner::provider::build("nebius", &config)
        .await
        .unwrap();
    let mut run = Run::new(&stub, "stub-disc").await;
    run.provider = provider;
    run.spec.regions = vec!["us-central1".into()];
    let end = run.launch(Cancel::new()).await;
    let LaunchState::Ready(vm) = end else {
        panic!("{end:?}\n{}", run.journal());
    };
    assert_eq!(vm.offer.region, "us-central1");
    let calls = stub.server.calls();
    assert!(
        calls.contains(&"GET /iam/v1/tenants".to_string()),
        "{calls:?}"
    );
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert!(stub.leftovers().is_empty());
}

/// Spot: the create asks for a preemptible VM that follows the spot price,
/// and a preemption (Nebius stops the VM) makes `gpuprov health` report it
/// unhealthy and preempted.
#[tokio::test]
async fn spot_vms_are_preemptible_and_preemption_is_diagnosed() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::health::{self, HealthOptions, Verdict};
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = NebiusStub::start().await;
    let mut run = Run::new(&stub, "stub-spot").await;
    run.spec.spot = true;
    let end = run.launch(Cancel::new()).await;
    assert!(
        matches!(end, LaunchState::Ready(_)),
        "{end:?}\n{}",
        run.journal()
    );
    let bodies = stub.create_bodies();
    assert_eq!(bodies[0]["spec"]["preemptible"]["onPreemption"], "STOP");
    assert_eq!(bodies[0]["spec"]["followsSpotPrice"], json!({}));

    stub.preempt();
    let record = run.db.get(&run.name).unwrap().unwrap();
    let report = health::check(
        Some(&*run.provider),
        &record,
        &probes::Reachable,
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Unhealthy, "{report:#?}");
    assert!(
        report.checks.iter().any(|c| c.detail.contains("preempted")),
        "{report:#?}"
    );
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert!(stub.leftovers().is_empty(), "{:?}", stub.leftovers());
}

/// Probes that always succeed: the stub has no real host behind its IPs.
mod probes {
    use std::path::Path;

    use async_trait::async_trait;
    use gpu_provisioner::provider::SshEndpoint;
    use gpu_provisioner::vm::health::HealthProbes;

    pub struct Reachable;

    #[async_trait]
    impl HealthProbes for Reachable {
        async fn ping(&self, _: &str) -> bool {
            true
        }
        async fn tcp(&self, _: &str, _: u16) -> bool {
            true
        }
        async fn ssh(&self, _: &SshEndpoint, _: &str, _: &Path) -> Result<(), String> {
            Ok(())
        }
        async fn exec(
            &self,
            _: &SshEndpoint,
            _: &str,
            _: &Path,
            _: &str,
        ) -> Result<(bool, String), String> {
            Ok((true, String::new()))
        }
    }
}
