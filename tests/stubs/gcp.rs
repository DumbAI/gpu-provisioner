//! Stateful stub of the **Google Compute Engine API v1**
//! (`https://compute.googleapis.com/compute/v1`), limited to the methods
//! gpuprov's GCP adapter uses, for one project (`stub-project`).
//!
//! Vendor reference: <https://cloud.google.com/compute/docs/reference/rest/v1>.
//! Resource shapes follow that reference — `compute#instance`,
//! `compute#operation` (status `PENDING`/`RUNNING`/`DONE`, failures in
//! `error.errors[{code, message}]`), aggregated lists as `items` maps keyed
//! by scope, and `{"error": {code, message, errors[{reason}]}}` on HTTP
//! errors — not gpuprov's parser. Every request must carry
//! `Authorization: Bearer <token>` (else `401`).
//!
//! | method (path under `/compute/v1`) | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /projects/{p}/regions/{region}` | [regions.get](https://cloud.google.com/compute/docs/reference/rest/v1/regions/get) | `us-central1` with zones `a`, `b`, `c` |
//! | `GET /projects/{p}/aggregated/machineTypes` | [machineTypes.aggregatedList](https://cloud.google.com/compute/docs/reference/rest/v1/machineTypes/aggregatedList) | `g2-standard-*` (built-in L4) in zones `a` and `b`; honors `filter=name = "…"` |
//! | `GET /projects/{p}/aggregated/acceleratorTypes` | [acceleratorTypes.aggregatedList](https://cloud.google.com/compute/docs/reference/rest/v1/acceleratorTypes/aggregatedList) | `nvidia-l4` in zones `a` and `b` |
//! | `GET /projects/{p}/global/firewalls/{name}` | [firewalls.get](https://cloud.google.com/compute/docs/reference/rest/v1/firewalls/get) | `404` until inserted |
//! | `POST /projects/{p}/global/firewalls` | [firewalls.insert](https://cloud.google.com/compute/docs/reference/rest/v1/firewalls/insert) | global `Operation` (`RUNNING`); `409` if it exists |
//! | `PATCH /projects/{p}/global/firewalls/{name}` | [firewalls.patch](https://cloud.google.com/compute/docs/reference/rest/v1/firewalls/patch) | replaces `allowed` |
//! | `DELETE /projects/{p}/global/firewalls/{name}` | [firewalls.delete](https://cloud.google.com/compute/docs/reference/rest/v1/firewalls/delete) | global `Operation`; `404` if absent |
//! | `POST /projects/{p}/global/operations/{op}/wait` | [globalOperations.wait](https://cloud.google.com/compute/docs/reference/rest/v1/globalOperations/wait) | completes the operation (`DONE`) |
//! | `GET /projects/{family-project}/global/images/family/{family}` | [images.getFromFamily](https://cloud.google.com/compute/docs/reference/rest/v1/images/getFromFamily) | an image with a `selfLink` |
//! | `POST /projects/{p}/zones/{zone}/instances` | [instances.insert](https://cloud.google.com/compute/docs/reference/rest/v1/instances/insert) | zonal `Operation` (`RUNNING`); `409 alreadyExists` on a duplicate name |
//! | `POST /projects/{p}/zones/{zone}/operations/{op}/wait` | [zoneOperations.wait](https://cloud.google.com/compute/docs/reference/rest/v1/zoneOperations/wait) | `DONE`; a stockout finishes `DONE` with `ZONE_RESOURCE_POOL_EXHAUSTED` and no instance |
//! | `GET /projects/{p}/zones/{zone}/instances` | [instances.list](https://cloud.google.com/compute/docs/reference/rest/v1/instances/list) | honors `filter=labels.<k> = "<v>"` |
//! | `GET /projects/{p}/aggregated/instances` | [instances.aggregatedList](https://cloud.google.com/compute/docs/reference/rest/v1/instances/aggregatedList) | same, grouped by `zones/<zone>` |
//! | `DELETE /projects/{p}/zones/{zone}/instances/{name}` | [instances.delete](https://cloud.google.com/compute/docs/reference/rest/v1/instances/delete) | zonal `Operation`; instance `STOPPING` for one list, then gone |
//!
//! A successful insert creates the instance `PROVISIONING`; it is `STAGING`
//! (with a `natIP`) after one list and `RUNNING` after the next. Login is
//! `ubuntu`@22 (from the `ssh-keys` metadata).

use std::collections::BTreeMap;

use async_trait::async_trait;
use gpu_provisioner::config::{AppConfig, GcpConfig};
use serde_json::{json, Value};
use wiremock::{Request, ResponseTemplate};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, last_segment, query, segment, Ids, StubServer};

const TOKEN: &str = "gcp-test-token";
const PROJECT: &str = "stub-project";
const REGION: &str = "us-central1";
/// Zones whose stock includes the G2 (L4) machine family.
const G2_ZONES: [&str; 2] = ["us-central1-a", "us-central1-b"];
const LABEL: &str = "gpuprov-vm-name";

#[derive(Debug)]
struct Instance {
    id: u64,
    name: String,
    zone: String,
    machine_type: String,
    labels: BTreeMap<String, String>,
    tags: Vec<String>,
    status: &'static str,
    /// Lists left in the current boot stage (`u32::MAX` = held).
    boot_lists: u32,
    nat_ip: String,
}

#[derive(Debug)]
struct Operation {
    name: String,
    /// `zones/<zone>` or `global`.
    scope: String,
    target: String,
    kind: &'static str,
    status: &'static str,
    error: Option<(String, String)>,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    next_instance_id: u64,
    instances: Vec<Instance>,
    firewalls: BTreeMap<String, Value>,
    operations: Vec<Operation>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    /// Boot payloads (user data / startup scripts) attached at create.
    boot_payloads: Vec<String>,
}

/// Simulated GCP project. See the module docs for the method map.
pub struct GcpStub {
    server: StubServer<Account>,
}

fn base(uri: &str) -> String {
    format!("{uri}/compute/v1")
}

/// HTTP error body (Google JSON error format).
fn error(status: u16, reason: &str, message: &str) -> ResponseTemplate {
    json(
        status,
        json!({"error": {
            "code": status,
            "message": message,
            "errors": [{"message": message, "domain": "global", "reason": reason}],
        }}),
    )
}

fn unauthorized(req: &Request) -> Option<ResponseTemplate> {
    let ok = header(req, "authorization").as_deref() == Some(&format!("Bearer {TOKEN}"));
    (!ok).then(|| {
        error(
            401,
            "authError",
            "Request had invalid authentication credentials.",
        )
    })
}

/// `selfLink`s use the canonical host, as the real API does.
fn link(path: &str) -> String {
    format!("https://www.googleapis.com/compute/v1/projects/{PROJECT}/{path}")
}

/// The value of `filter=<field> = "<value>"` (the only filter form gpuprov sends).
fn filter_value(req: &Request, field: &str) -> Option<String> {
    let filter = query(req, "filter")?;
    let (lhs, rhs) = filter.split_once('=')?;
    (lhs.trim() == field).then(|| rhs.trim().trim_matches('"').to_string())
}

fn operation_json(op: &Operation) -> Value {
    let mut v = json!({
        "kind": "compute#operation",
        "id": "1234567890",
        "name": op.name,
        "operationType": op.kind,
        "targetLink": op.target,
        "status": op.status,
        "progress": if op.status == "DONE" { 100 } else { 0 },
        "insertTime": "2026-10-19T00:00:00.000-07:00",
        "selfLink": link(&format!("{}/operations/{}", op.scope, op.name)),
    });
    if op.scope != "global" {
        v["zone"] = json!(link(&op.scope));
    }
    if let Some((code, message)) = &op.error {
        v["error"] = json!({"errors": [{"code": code, "message": message}]});
        v["httpErrorStatusCode"] = json!(503);
    }
    v
}

fn new_operation(
    account: &mut Account,
    scope: &str,
    kind: &'static str,
    target: String,
    error: Option<(String, String)>,
) -> Value {
    let op = Operation {
        name: account.ids.next("operation"),
        scope: scope.to_string(),
        target,
        kind,
        status: "RUNNING",
        error,
    };
    let v = operation_json(&op);
    account.operations.push(op);
    v
}

fn instance_json(i: &Instance) -> Value {
    let mut access = json!({"type": "ONE_TO_ONE_NAT", "name": "external-nat",
                            "networkTier": "PREMIUM"});
    if i.status != "PROVISIONING" {
        access["natIP"] = json!(i.nat_ip);
    }
    json!({
        "kind": "compute#instance",
        "id": i.id.to_string(),
        "name": i.name,
        "zone": link(&format!("zones/{}", i.zone)),
        "machineType": link(&format!("zones/{}/machineTypes/{}", i.zone, i.machine_type)),
        "status": i.status,
        "labels": i.labels,
        "tags": {"items": i.tags},
        "networkInterfaces": [{
            "network": link("global/networks/default"),
            "networkIP": "10.128.0.7",
            "accessConfigs": [access],
        }],
        "creationTimestamp": "2026-10-19T00:00:00.000-07:00",
        "selfLink": link(&format!("zones/{}/instances/{}", i.zone, i.name)),
    })
}

/// One simulation tick (each instance list advances the boot/delete states).
fn tick(account: &mut Account) {
    account.instances.retain(|i| i.status != "TERMINATED");
    for i in account.instances.iter_mut() {
        match i.status {
            _ if i.boot_lists == u32::MAX => {}
            "PROVISIONING" => i.status = "STAGING",
            "STAGING" => i.status = "RUNNING",
            "STOPPING" => i.status = "TERMINATED",
            _ => {}
        }
    }
    account.instances.retain(|i| i.status != "TERMINATED");
}

fn machine_type_json(zone: &str, name: &str) -> Value {
    let vcpus: u32 = name
        .rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4);
    json!({
        "kind": "compute#machineType",
        "name": name,
        "zone": zone,
        "guestCpus": vcpus,
        "memoryMb": vcpus * 4096,
        "accelerators": [{"guestAcceleratorType": "nvidia-l4", "guestAcceleratorCount": 1}],
        "selfLink": link(&format!("zones/{zone}/machineTypes/{name}")),
    })
}

/// An aggregated list: zones offering `name` get `[item]`, others a
/// `NO_RESULTS_ON_PAGE` warning (as the real API reports empty scopes).
fn aggregated(collection: &str, zones_with: &[&str], item: impl Fn(&str) -> Value) -> Value {
    let mut items = serde_json::Map::new();
    for zone in ["us-central1-a", "us-central1-b", "us-central1-c"] {
        let scope = if zones_with.contains(&zone) {
            json!({ collection: [item(zone)] })
        } else {
            json!({"warning": {
                "code": "NO_RESULTS_ON_PAGE",
                "message": format!("There are no results for scope 'zones/{zone}' on this page."),
                "data": [{"key": "scope", "value": format!("zones/{zone}")}],
            }})
        };
        items.insert(format!("zones/{zone}"), scope);
    }
    json!({"kind": format!("compute#{collection}AggregatedList"), "items": items})
}

#[async_trait]
impl VendorStub for GcpStub {
    const CLOUD: &'static str = "gcp";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;
        let p = format!("^/compute/v1/projects/{PROJECT}");

        // regions.get
        server
            .route("GET", &format!("{p}/regions/[^/]+$"), |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let region = last_segment(req);
                if region != REGION {
                    return error(404, "notFound", &format!("The resource 'projects/{PROJECT}/regions/{region}' was not found"));
                }
                let zones: Vec<String> = ["a", "b", "c"]
                    .iter()
                    .map(|z| link(&format!("zones/{REGION}-{z}")))
                    .collect();
                json(200, json!({"kind": "compute#region", "name": REGION, "status": "UP", "zones": zones}))
            })
            .await;
        // machineTypes.aggregatedList
        server
            .route("GET", &format!("{p}/aggregated/machineTypes$"), |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let wanted = filter_value(req, "name");
                let offered = wanted
                    .as_deref()
                    .is_some_and(|n| n.starts_with("g2-standard-"));
                let zones: &[&str] = if offered { &G2_ZONES } else { &[] };
                let name = wanted.unwrap_or_default();
                json(
                    200,
                    aggregated("machineTypes", zones, |z| machine_type_json(z, &name)),
                )
            })
            .await;
        // acceleratorTypes.aggregatedList
        server
            .route(
                "GET",
                &format!("{p}/aggregated/acceleratorTypes$"),
                |req, _| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let wanted = filter_value(req, "name").unwrap_or_default();
                    let zones: &[&str] = if wanted == "nvidia-l4" {
                        &G2_ZONES
                    } else {
                        &[]
                    };
                    json(
                        200,
                        aggregated("acceleratorTypes", zones, |z| {
                            json!({"kind": "compute#acceleratorType", "name": wanted, "zone": z,
                               "maximumCardsPerInstance": 8})
                        }),
                    )
                },
            )
            .await;
        // firewalls.get / patch / delete
        server
            .route(
                "GET",
                &format!("{p}/global/firewalls/[^/]+$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    match account.firewalls.get(&last_segment(req)) {
                        Some(rule) => json(200, rule.clone()),
                        None => error(404, "notFound", "The resource was not found"),
                    }
                },
            )
            .await;
        server
            .route(
                "PATCH",
                &format!("{p}/global/firewalls/[^/]+$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let name = last_segment(req);
                    let patch = body(req);
                    let Some(rule) = account.firewalls.get_mut(&name) else {
                        return error(404, "notFound", "The resource was not found");
                    };
                    if let Some(allowed) = patch.get("allowed") {
                        rule["allowed"] = allowed.clone();
                    }
                    let op = new_operation(
                        account,
                        "global",
                        "patch",
                        link(&format!("global/firewalls/{name}")),
                        None,
                    );
                    json(200, op)
                },
            )
            .await;
        server
            .route(
                "DELETE",
                &format!("{p}/global/firewalls/[^/]+$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let name = last_segment(req);
                    if account.firewalls.remove(&name).is_none() {
                        return error(404, "notFound", "The resource was not found");
                    }
                    let op = new_operation(
                        account,
                        "global",
                        "delete",
                        link(&format!("global/firewalls/{name}")),
                        None,
                    );
                    json(200, op)
                },
            )
            .await;
        // firewalls.insert
        server
            .route("POST", &format!("{p}/global/firewalls$"), |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let mut rule = body(req);
                let name = rule["name"].as_str().unwrap_or_default().to_string();
                if name.is_empty() {
                    return error(400, "invalid", "Invalid value for field 'resource.name'");
                }
                if account.firewalls.contains_key(&name) {
                    return error(409, "alreadyExists", "The resource already exists");
                }
                rule["kind"] = json!("compute#firewall");
                rule["selfLink"] = json!(link(&format!("global/firewalls/{name}")));
                account.firewalls.insert(name.clone(), rule);
                let op = new_operation(
                    account,
                    "global",
                    "insert",
                    link(&format!("global/firewalls/{name}")),
                    None,
                );
                json(200, op)
            })
            .await;
        // globalOperations.wait / zoneOperations.wait
        server
            .route(
                "POST",
                &format!("{p}/(global|zones/[^/]+)/operations/[^/]+/wait$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    // .../operations/{op}/wait → the op name is the 2nd-to-last segment.
                    let segments: Vec<String> = req
                        .url
                        .path_segments()
                        .map(|s| s.map(str::to_string).collect())
                        .unwrap_or_default();
                    let name = segments[segments.len() - 2].clone();
                    match account.operations.iter_mut().find(|o| o.name == name) {
                        Some(op) => {
                            op.status = "DONE";
                            json(200, operation_json(op))
                        }
                        None => error(404, "notFound", "The resource was not found"),
                    }
                },
            )
            .await;
        // images.getFromFamily (any image project)
        server
            .route(
                "GET",
                r"^/compute/v1/projects/[^/]+/global/images/family/[^/]+$",
                |req, _| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let project = segment(req, 2);
                    let family = last_segment(req);
                    json(
                        200,
                        json!({
                            "kind": "compute#image",
                            "name": format!("{family}-v20261001"),
                            "family": family,
                            "status": "READY",
                            "selfLink": format!(
                                "https://www.googleapis.com/compute/v1/projects/{project}/global/images/{family}-v20261001"
                            ),
                        }),
                    )
                },
            )
            .await;
        // instances.insert
        server
            .route(
                "POST",
                &format!("{p}/zones/[^/]+/instances$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let zone = segment(req, 5);
                    let spec = body(req);
                    let name = spec["name"].as_str().unwrap_or_default().to_string();
                    let target = link(&format!("zones/{zone}/instances/{name}"));
                    if account.capacity_faults > 0 {
                        // Stockouts surface asynchronously, in the finished operation.
                        account.capacity_faults -= 1;
                        let fault = (
                            "ZONE_RESOURCE_POOL_EXHAUSTED".to_string(),
                            format!(
                                "The zone 'projects/{PROJECT}/zones/{zone}' does not have enough \
                             resources available to fulfill the request. Try a different zone, \
                             or try again later."
                            ),
                        );
                        let op = new_operation(
                            account,
                            &format!("zones/{zone}"),
                            "insert",
                            target,
                            Some(fault),
                        );
                        return json(200, op);
                    }
                    if account
                        .instances
                        .iter()
                        .any(|i| i.name == name && i.zone == zone)
                    {
                        return error(409, "alreadyExists", "The resource already exists");
                    }
                    let labels: BTreeMap<String, String> = spec["labels"]
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| {
                                    (k.clone(), v.as_str().unwrap_or_default().to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let tags: Vec<String> = spec["tags"]["items"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let machine_type = spec["machineType"]
                        .as_str()
                        .unwrap_or_default()
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    account.next_instance_id += 1;
                    let id = account.next_instance_id;
                    account.instances.push(Instance {
                        id: 4_000_000_000 + id,
                        name,
                        zone: zone.clone(),
                        machine_type,
                        labels,
                        tags,
                        status: "PROVISIONING",
                        boot_lists: if account.hold_boot { u32::MAX } else { 0 },
                        nat_ip: format!("34.122.0.{id}"),
                    });
                    // `metadata.items[key=user-data]` — run by cloud-init.
                    if let Some(user_data) =
                        spec["metadata"]["items"].as_array().and_then(|items| {
                            items
                                .iter()
                                .find(|i| i["key"] == "user-data")
                                .and_then(|i| i["value"].as_str())
                        })
                    {
                        account.boot_payloads.push(user_data.to_string());
                    }
                    account.creates += 1;
                    let op =
                        new_operation(account, &format!("zones/{zone}"), "insert", target, None);
                    json(200, op)
                },
            )
            .await;
        // instances.list
        server
            .route("GET", &format!("{p}/zones/[^/]+/instances$"), |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                tick(account);
                let zone = segment(req, 5);
                let wanted = filter_value(req, &format!("labels.{LABEL}"));
                let items: Vec<Value> = account
                    .instances
                    .iter()
                    .filter(|i| i.zone == zone)
                    .filter(|i| wanted.as_ref().is_none_or(|w| i.labels.get(LABEL) == Some(w)))
                    .map(instance_json)
                    .collect();
                let mut page = json!({"kind": "compute#instanceList", "id": format!("projects/{PROJECT}/zones/{zone}/instances")});
                if !items.is_empty() {
                    page["items"] = json!(items);
                }
                json(200, page)
            })
            .await;
        // instances.aggregatedList
        server
            .route(
                "GET",
                &format!("{p}/aggregated/instances$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    tick(account);
                    let wanted = filter_value(req, &format!("labels.{LABEL}"));
                    let mut items = serde_json::Map::new();
                    for i in account.instances.iter().filter(|i| {
                        wanted
                            .as_ref()
                            .is_none_or(|w| i.labels.get(LABEL) == Some(w))
                    }) {
                        let scope = items
                            .entry(format!("zones/{}", i.zone))
                            .or_insert_with(|| json!({"instances": []}));
                        scope["instances"]
                            .as_array_mut()
                            .unwrap()
                            .push(instance_json(i));
                    }
                    json(
                        200,
                        json!({"kind": "compute#instanceAggregatedList", "items": items}),
                    )
                },
            )
            .await;
        // instances.delete
        server
            .route(
                "DELETE",
                &format!("{p}/zones/[^/]+/instances/[^/]+$"),
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let zone = segment(req, 5);
                    let name = last_segment(req);
                    let Some(instance) = account
                        .instances
                        .iter_mut()
                        .find(|i| i.zone == zone && i.name == name)
                    else {
                        return error(404, "notFound", "The resource was not found");
                    };
                    instance.status = "STOPPING";
                    instance.boot_lists = 0;
                    let target = link(&format!("zones/{zone}/instances/{name}"));
                    let op =
                        new_operation(account, &format!("zones/{zone}"), "delete", target, None);
                    json(200, op)
                },
            )
            .await;
        GcpStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            gcp: GcpConfig {
                project: Some(PROJECT.into()),
                access_token: Some(TOKEN.into()),
                base_url: Some(base(&self.server.uri())),
                regions: vec![REGION.into()],
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            gpus: Some("L4:1".into()),
            regions: vec![REGION.into()],
            ..Ask::default()
        }
    }

    fn fail_next_create_with_capacity(&self) {
        self.server.state().capacity_faults = 1;
    }

    fn hold_boot(&self, hold: bool) {
        self.server.state().hold_boot = hold;
    }

    fn leftovers(&self) -> Vec<String> {
        let account = self.server.state();
        account
            .instances
            .iter()
            .map(|i| format!("instance {}/{} ({})", i.zone, i.name, i.status))
            .chain(
                account
                    .firewalls
                    .keys()
                    .filter(|name| name.starts_with("gpuprov-"))
                    .map(|name| format!("firewall rule {name}")),
            )
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn boot_payloads(&self) -> Vec<String> {
        self.server.state().boot_payloads.clone()
    }

    fn expected_login(&self) -> (String, u16) {
        ("ubuntu".into(), 22)
    }
}

vendor_suite!(GcpStub);
