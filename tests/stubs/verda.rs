//! Stateful stub of the **Verda REST API v1** (`https://api.verda.com/v1`),
//! limited to the endpoints gpuprov's Verda adapter uses.
//!
//! Vendor reference: <https://api.verda.com/v1/docs> (OpenAPI:
//! <https://api.verda.com/v1/openapi.json>). Shapes below follow that spec —
//! bare-string ids from creates, the bare boolean of the single-type
//! availability check, `{code, message}` errors — not gpuprov's parser.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `POST /v1/oauth2/token` | [Get access token](https://api.verda.com/v1/docs#tag/authentication/POST/v1/oauth2/token) | client-credentials grant → bearer token; every other call requires it |
//! | `GET /v1/instance-types` | [Get instance types](https://api.verda.com/v1/docs#tag/instance-types/GET/v1/instance-types) | `1A6000.10V` (1× RTX A6000) and a CPU type |
//! | `GET /v1/instance-availability` | [Get all instance type availabilities](https://api.verda.com/v1/docs#tag/instance-availability/GET/v1/instance-availability) | `1A6000.10V` in `FIN-01` and `FIN-02` (so failover has a second offer) |
//! | `GET /v1/instance-availability/{instance_type}` | [Get instance type availability](https://api.verda.com/v1/docs#tag/instance-availability/GET/v1/instance-availability/{instance_type}) | bare `true`/`false` for `location_code` |
//! | `GET /v1/locations` | [Get locations](https://api.verda.com/v1/docs#tag/locations/GET/v1/locations) | `FIN-01`, `FIN-02` (country `FI`) |
//! | `GET /v1/ssh-keys`, `POST /v1/ssh-keys` | [Get SSH keys](https://api.verda.com/v1/docs#tag/ssh-keys/GET/v1/ssh-keys), [Add new SSH key](https://api.verda.com/v1/docs#tag/ssh-keys/POST/v1/ssh-keys) | account keys; create returns the id as a bare JSON string (`201`) |
//! | `GET /v1/scripts`, `POST /v1/scripts`, `DELETE /v1/scripts/{id}` | [Startup scripts](https://api.verda.com/v1/docs#tag/startup-scripts) | create returns the id as a bare string (`201`) |
//! | `POST /v1/instances` | [Deploy instance](https://api.verda.com/v1/docs#tag/instances/POST/v1/instances) | `202` + bare id; creates the tagged OS volume; capacity fault → `503 {code: service_unavailable}` |
//! | `GET /v1/instances?tag=k=v` | [Get instances](https://api.verda.com/v1/docs#tag/instances/GET/v1/instances) | instances carrying every listed tag (`key=value` exact) |
//! | `PUT /v1/instances` (`action: delete`) | [Perform action on instances](https://api.verda.com/v1/docs#tag/instances/PUT/v1/instances) | `202` + per-instance results; `volume_ids` + `delete_permanently` delete those volumes for good, others detach |
//! | `GET /v1/volumes`, `DELETE /v1/volumes/{id}` | [Get all volumes](https://api.verda.com/v1/docs#tag/volumes/GET/v1/volumes), [Delete volume](https://api.verda.com/v1/docs#tag/volumes/DELETE/v1/volumes/{volume_id}) | delete with `{is_permanent: true}` removes it; otherwise it goes to the trash (still a leftover) |
//!
//! Health signals: `VerdaStub::discontinue_all` evicts instances the way a
//! spot reclaim does — they stay listed with status `discontinued` and their
//! volumes are detached (`GET /v1/instances` status enum, `is_spot`).
//!
//! Instances start `provisioning` (no `ip`), become `running` with an IP
//! after one list, list as `deleting` once after a delete, then are gone.
//! Login is `root` on port 22.

use async_trait::async_trait;
use gpu_provisioner::config::{AppConfig, VerdaConfig};
use serde_json::{json, Value};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, empty, header, json, last_segment, query, query_all, Ids, StubServer};

const CLIENT_ID: &str = "verda-client";
const CLIENT_SECRET: &str = "verda-secret";
const TOKEN: &str = "verda-access-token";
const TYPE: &str = "1A6000.10V";

#[derive(Debug, Clone)]
struct Tag {
    key: String,
    value: String,
}

#[derive(Debug)]
struct Instance {
    id: String,
    status: &'static str,
    location: String,
    tags: Vec<Tag>,
    os_volume_id: String,
    /// Lists left before `provisioning` becomes `running`.
    boot_lists: u32,
    /// A deleted instance has been listed as `deleting` once.
    deletion_listed: bool,
    /// Deployed with `is_spot: true`.
    is_spot: bool,
}

#[derive(Debug)]
struct Volume {
    id: String,
    name: String,
    status: &'static str,
    instance_id: Option<String>,
    tags: Vec<Tag>,
    location: String,
    size: f64,
    /// Deleted without `is_permanent`: sits in the trash (still billed).
    trashed: bool,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    instances: Vec<Instance>,
    volumes: Vec<Volume>,
    ssh_keys: Vec<(String, String, String)>,
    scripts: Vec<(String, String, String)>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    /// Boot payloads (user data / startup scripts) attached at create.
    boot_payloads: Vec<String>,
}

/// Simulated Verda project. See the module docs for the endpoint map.
pub struct VerdaStub {
    server: StubServer<Account>,
}

/// `{code, message}` error body (spec: the shared error schema).
fn error(status: u16, code: &str, message: &str) -> wiremock::ResponseTemplate {
    json(status, json!({"code": code, "message": message}))
}

fn authed(req: &wiremock::Request) -> bool {
    header(req, "authorization").as_deref() == Some(&format!("Bearer {TOKEN}"))
}

fn unauthorized() -> wiremock::ResponseTemplate {
    error(
        401,
        "unauthorized_request",
        "Invalid or expired access token",
    )
}

fn tags_json(tags: &[Tag]) -> Value {
    Value::Array(
        tags.iter()
            .enumerate()
            .map(|(i, t)| json!({"id": format!("tag_{i}"), "key": t.key, "value": t.value}))
            .collect(),
    )
}

fn parse_tags(value: &Value) -> Vec<Tag> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|t| Tag {
                    // Keys are lowercased by the API.
                    key: t["key"].as_str().unwrap_or_default().to_lowercase(),
                    value: t["value"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn instance_json(instance: &Instance) -> Value {
    let running = instance.status == "running";
    json!({
        "id": instance.id,
        "ip": if running { json!("198.51.100.30") } else { Value::Null },
        "status": instance.status,
        "created_at": "2026-10-19T00:00:00.000Z",
        "created_by_user_id": "user-1",
        "cpu": {"description": "10 CPU", "number_of_cores": 10},
        "gpu": {"description": "1x RTX A6000 48GB", "number_of_gpus": 1},
        "gpu_memory": {"description": "48GB GPU RAM", "size_in_gigabytes": 48},
        "memory": {"description": "60GB RAM", "size_in_gigabytes": 60},
        "storage": {"description": "dynamic"},
        "hostname": "gpuprov-host",
        "description": "",
        "tags": tags_json(&instance.tags),
        "location": instance.location,
        "price_per_hour": 0.49,
        "is_spot": instance.is_spot,
        "instance_type": TYPE,
        "image": "ubuntu-24.04-cuda-12.8-open-docker",
        "os_name": "Ubuntu 24.04",
        "startup_script_id": null,
        "ssh_key_ids": [],
        "os_volume_id": instance.os_volume_id,
        "jupyter_token": null,
        "contract": "PAY_AS_YOU_GO",
        "pricing": "FIXED_PRICE",
        "volume_ids": [instance.os_volume_id],
    })
}

fn volume_json(volume: &Volume) -> Value {
    json!({
        "id": volume.id,
        "instance_id": volume.instance_id,
        "instances": volume.instance_id.iter().collect::<Vec<_>>(),
        "name": volume.name,
        "created_at": "2026-10-19T00:00:00.000Z",
        "created_by_user_id": "user-1",
        "status": volume.status,
        "size": volume.size,
        "is_os_volume": true,
        "target": null,
        "type": "NVMe",
        "location": volume.location,
        "ssh_key_ids": [],
        "pseudo_path": null,
        "tags": tags_json(&volume.tags),
    })
}

fn has_tags(tags: &[Tag], filters: &[String]) -> bool {
    filters.iter().all(|f| match f.split_once('=') {
        Some((k, v)) => tags
            .iter()
            .any(|t| t.key == k.to_lowercase() && t.value == v),
        None => tags.iter().any(|t| t.key == f.to_lowercase()),
    })
}

impl VerdaStub {
    /// Evict every instance as a spot reclaim does: listed as
    /// `discontinued`, volumes detached.
    pub fn discontinue_all(&self) {
        let mut account = self.server.state();
        for instance in account.instances.iter_mut() {
            instance.status = "discontinued";
        }
        for volume in account.volumes.iter_mut() {
            volume.status = "detached";
            volume.instance_id = None;
        }
    }
}

#[async_trait]
impl VendorStub for VerdaStub {
    const CLOUD: &'static str = "verda";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;

        // POST /v1/oauth2/token — Get access token (client credentials).
        server
            .route("POST", r"^/v1/oauth2/token$", |req, _| {
                let grant = body(req);
                if grant["grant_type"] == "client_credentials"
                    && grant["client_id"] == CLIENT_ID
                    && grant["client_secret"] == CLIENT_SECRET
                {
                    json(
                        200,
                        json!({"access_token": TOKEN, "token_type": "Bearer", "expires_in": 3600,
                               "refresh_token": "refresh", "scope": "cloud-api-v1"}),
                    )
                } else {
                    error(
                        401,
                        "unauthorized_request",
                        "Invalid client id or client secret",
                    )
                }
            })
            .await;
        // GET /v1/instance-types — Get instance types.
        server
            .route("GET", r"^/v1/instance-types$", |req, _| {
                if !authed(req) {
                    return unauthorized();
                }
                json(
                    200,
                    json!([
                        {
                            "best_for": ["Inference"], "cpu": {"description": "10 CPU", "number_of_cores": 10},
                            "deploy_warning": null, "description": "Dedicated Hardware Instance",
                            "gpu": {"description": "1x RTX A6000 48GB", "number_of_gpus": 1},
                            "gpu_memory": {"description": "48GB GPU RAM", "size_in_gigabytes": 48},
                            "id": "type-a6000", "instance_type": TYPE,
                            "memory": {"description": "60GB RAM", "size_in_gigabytes": 60},
                            "model": "A6000", "name": "RTX A6000 48GB", "p2p": null,
                            "price_per_hour": "0.49", "spot_price": "0.17",
                            "storage": {"description": "dynamic"}, "currency": "usd",
                            "manufacturer": "NVIDIA", "display_name": "NVIDIA RTX A6000 48GB",
                            "supported_os": ["ubuntu-24.04-cuda-12.8-open-docker", "ubuntu-24.04"]
                        },
                        {
                            "best_for": ["General"], "cpu": {"description": "4 CPU", "number_of_cores": 4},
                            "deploy_warning": null, "description": "CPU Instance",
                            "gpu": {"description": "", "number_of_gpus": 0},
                            "gpu_memory": {"description": "", "size_in_gigabytes": 0},
                            "id": "type-cpu", "instance_type": "CPU.4V.16G",
                            "memory": {"description": "16GB RAM", "size_in_gigabytes": 16},
                            "model": "CPU Node", "name": "CPU Node", "p2p": null,
                            "price_per_hour": "0.03", "spot_price": "0.01",
                            "storage": {"description": "dynamic"}, "currency": "usd",
                            "manufacturer": "AMD", "display_name": "CPU Node",
                            "supported_os": ["ubuntu-24.04"]
                        }
                    ]),
                )
            })
            .await;
        // GET /v1/instance-availability — all locations.
        server
            .route("GET", r"^/v1/instance-availability$", |req, _| {
                if !authed(req) {
                    return unauthorized();
                }
                json(
                    200,
                    json!([
                        {"location_code": "FIN-01", "availabilities": [TYPE, "CPU.4V.16G"]},
                        {"location_code": "FIN-02", "availabilities": [TYPE]},
                    ]),
                )
            })
            .await;
        // GET /v1/instance-availability/{instance_type} — bare boolean.
        server
            .route("GET", r"^/v1/instance-availability/[^/]+$", |req, _| {
                if !authed(req) {
                    return unauthorized();
                }
                let location = query(req, "location_code").unwrap_or_default();
                let available = last_segment(req) == TYPE
                    && matches!(location.as_str(), "FIN-01" | "FIN-02" | "");
                json(200, json!(available))
            })
            .await;
        // GET /v1/locations — Returns a list of available locations.
        server
            .route("GET", r"^/v1/locations$", |req, _| {
                if !authed(req) {
                    return unauthorized();
                }
                json(
                    200,
                    json!([
                        {"code": "FIN-01", "name": "Finland 1", "country_code": "FI"},
                        {"code": "FIN-02", "name": "Finland 2", "country_code": "FI"},
                    ]),
                )
            })
            .await;
        // GET /v1/ssh-keys — Get SSH keys.
        server
            .route("GET", r"^/v1/ssh-keys$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let keys: Vec<Value> = account
                    .ssh_keys
                    .iter()
                    .map(|(id, name, key)| {
                        json!({"id": id, "name": name, "key": key, "fingerprint": null,
                               "created_by_user_id": "user-1", "created_at": "2026-10-19T00:00:00.000Z"})
                    })
                    .collect();
                json(200, Value::Array(keys))
            })
            .await;
        // POST /v1/ssh-keys — Add new SSH key (201 + bare id).
        server
            .route("POST", r"^/v1/ssh-keys$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let spec = body(req);
                let id = account.ids.next("key");
                account.ssh_keys.push((
                    id.clone(),
                    spec["name"].as_str().unwrap_or_default().to_string(),
                    spec["key"].as_str().unwrap_or_default().to_string(),
                ));
                json(201, json!(id))
            })
            .await;
        // GET /v1/scripts — Get startup scripts.
        server
            .route("GET", r"^/v1/scripts$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let scripts: Vec<Value> = account
                    .scripts
                    .iter()
                    .map(|(id, name, script)| {
                        json!({"id": id, "name": name, "script": script,
                               "created_at": "2026-10-19T00:00:00.000Z"})
                    })
                    .collect();
                json(200, Value::Array(scripts))
            })
            .await;
        // POST /v1/scripts — Add new startup script (201 + bare id).
        server
            .route("POST", r"^/v1/scripts$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let spec = body(req);
                let id = account.ids.next("script");
                account.scripts.push((
                    id.clone(),
                    spec["name"].as_str().unwrap_or_default().to_string(),
                    spec["script"].as_str().unwrap_or_default().to_string(),
                ));
                json(201, json!(id))
            })
            .await;
        // DELETE /v1/scripts/{scriptId} — Delete single startup script.
        server
            .route("DELETE", r"^/v1/scripts/[^/]+$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let id = last_segment(req);
                let before = account.scripts.len();
                account.scripts.retain(|(sid, _, _)| *sid != id);
                if account.scripts.len() < before {
                    empty(200)
                } else {
                    error(404, "not_found", "Script not found")
                }
            })
            .await;
        // POST /v1/instances — Deploy instance (202 + bare id).
        server
            .route("POST", r"^/v1/instances$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    return error(503, "service_unavailable", "No capacity available");
                }
                let spec = body(req);
                let location = spec["location_code"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let instance_id = account.ids.next("inst");
                let volume_id = account.ids.next("vol");
                account.volumes.push(Volume {
                    id: volume_id.clone(),
                    name: spec["os_volume"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    status: "attached",
                    instance_id: Some(instance_id.clone()),
                    tags: parse_tags(&spec["os_volume"]["tags"]),
                    location: location.clone(),
                    size: spec["os_volume"]["size"].as_f64().unwrap_or(50.0),
                    trashed: false,
                });
                account.instances.push(Instance {
                    id: instance_id.clone(),
                    status: "provisioning",
                    location,
                    tags: parse_tags(&spec["tags"]),
                    os_volume_id: volume_id,
                    boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                    deletion_listed: false,
                    is_spot: spec["is_spot"].as_bool().unwrap_or(false),
                });
                // The startup script attached to the instance, as Verda
                // would run it at first boot.
                if let Some(script_id) = spec["startup_script_id"].as_str() {
                    if let Some((_, _, script)) =
                        account.scripts.iter().find(|(id, _, _)| id == script_id)
                    {
                        account.boot_payloads.push(script.clone());
                    }
                }
                account.creates += 1;
                json(202, json!(instance_id))
            })
            .await;
        // GET /v1/instances — Get instances (tag filter). One list = one tick.
        server
            .route("GET", r"^/v1/instances$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                account.instances.retain(|i| !i.deletion_listed);
                for instance in account.instances.iter_mut() {
                    if instance.status == "provisioning" && instance.boot_lists != u32::MAX {
                        if instance.boot_lists == 0 {
                            instance.status = "running";
                        } else {
                            instance.boot_lists -= 1;
                        }
                    }
                }
                let filters = query_all(req, "tag");
                let listed: Vec<Value> = account
                    .instances
                    .iter()
                    .filter(|i| has_tags(&i.tags, &filters))
                    .map(instance_json)
                    .collect();
                for instance in account.instances.iter_mut() {
                    if instance.status == "deleting" {
                        instance.deletion_listed = true;
                    }
                }
                json(200, Value::Array(listed))
            })
            .await;
        // PUT /v1/instances — Perform action (only `delete` is simulated).
        server
            .route("PUT", r"^/v1/instances$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let action = body(req);
                if action["action"] != "delete" {
                    return error(400, "invalid_request", "only delete is simulated");
                }
                let ids: Vec<String> = match &action["id"] {
                    Value::String(id) => vec![id.clone()],
                    Value::Array(ids) => ids.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                    _ => Vec::new(),
                };
                let volume_ids: Vec<String> = action["volume_ids"]
                    .as_array()
                    .map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let permanent = action["delete_permanently"].as_bool().unwrap_or(false);
                let mut results = Vec::new();
                for id in &ids {
                    match account.instances.iter_mut().find(|i| &i.id == id) {
                        Some(instance) => {
                            instance.status = "deleting";
                            results.push(json!({"instanceId": id, "action": "delete", "status": "success"}));
                        }
                        None => results.push(json!({"instanceId": id, "action": "delete", "status": "error",
                                                    "error": "Instance not found", "statusCode": 404})),
                    }
                }
                // Volumes named in the call are deleted; others only detach.
                for volume in account.volumes.iter_mut() {
                    let Some(owner) = volume.instance_id.clone() else { continue };
                    if !ids.contains(&owner) {
                        continue;
                    }
                    volume.instance_id = None;
                    if volume_ids.contains(&volume.id) {
                        volume.status = "deleted";
                        volume.trashed = !permanent;
                    } else {
                        volume.status = "detached";
                    }
                }
                account.volumes.retain(|v| v.status != "deleted" || v.trashed);
                json(202, Value::Array(results))
            })
            .await;
        // GET /v1/volumes — Get all volumes (trash excluded).
        server
            .route("GET", r"^/v1/volumes$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let volumes: Vec<Value> = account
                    .volumes
                    .iter()
                    .filter(|v| !v.trashed)
                    .map(volume_json)
                    .collect();
                json(200, Value::Array(volumes))
            })
            .await;
        // DELETE /v1/volumes/{volume_id} — Delete volume ({is_permanent}).
        server
            .route("DELETE", r"^/v1/volumes/[^/]+$", |req, account| {
                if !authed(req) {
                    return unauthorized();
                }
                let id = last_segment(req);
                let permanent = body(req)["is_permanent"].as_bool().unwrap_or(false);
                let Some(volume) = account.volumes.iter_mut().find(|v| v.id == id) else {
                    return error(404, "not_found", "Volume not found");
                };
                if volume.instance_id.is_some() {
                    return error(400, "invalid_request", "Volume is attached to an instance");
                }
                volume.status = "deleted";
                volume.trashed = !permanent;
                account
                    .volumes
                    .retain(|v| v.status != "deleted" || v.trashed);
                empty(202)
            })
            .await;
        VerdaStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            verda: VerdaConfig {
                client_id: Some(CLIENT_ID.into()),
                client_secret: Some(CLIENT_SECRET.into()),
                base_url: Some(self.server.uri()),
                ..VerdaConfig::default()
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            gpus: Some("A6000:1".into()),
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
        let mut left: Vec<String> = account
            .instances
            .iter()
            // `discontinued` instances no longer exist (nor bill) at Verda.
            .filter(|i| i.status != "deleting" && i.status != "discontinued")
            .map(|i| format!("instance {} ({})", i.id, i.status))
            .collect();
        left.extend(account.volumes.iter().map(|v| {
            format!(
                "volume {} ({}{})",
                v.id,
                v.status,
                if v.trashed { ", in trash" } else { "" }
            )
        }));
        left.extend(
            account
                .scripts
                .iter()
                .filter(|(_, name, _)| name.starts_with("gpuprov-"))
                .map(|(id, name, _)| format!("startup script {id} ({name})")),
        );
        left
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        // A spot discontinue: the instance is gone and its volumes are
        // left detached (gpuprov's cleanup sweeps them by tag).
        let mut account = self.server.state();
        account.instances.clear();
        for volume in account.volumes.iter_mut() {
            volume.status = "detached";
            volume.instance_id = None;
        }
    }

    fn boot_payloads(&self) -> Vec<String> {
        self.server.state().boot_payloads.clone()
    }

    fn expected_login(&self) -> (String, u16) {
        ("root".into(), 22)
    }
}

vendor_suite!(VerdaStub);

/// `gpuprov health` reads Verda's own signals: a spot instance is noted, and
/// an evicted (`discontinued`) instance is reported gone with Verda's reason.
#[tokio::test]
async fn health_reads_spot_and_discontinued() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::health::{self, HealthOptions, Verdict};
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = VerdaStub::start().await;
    let mut run = Run::new(&stub, "stub-diag").await;
    run.spec.spot = true;
    let end = run.launch(Cancel::new()).await;
    assert!(
        matches!(end, LaunchState::Ready(_)),
        "{end:?}\n{}",
        run.journal()
    );
    let record = run.db.get(&run.name).unwrap().unwrap();
    let check = || {
        health::check(
            Some(&*run.provider),
            &record,
            &probes::Reachable,
            HealthOptions::default(),
        )
    };
    let has = |report: &health::HealthReport, text: &str| {
        report.checks.iter().any(|c| c.detail.contains(text))
    };

    let report = check().await;
    assert_eq!(report.verdict, Verdict::Healthy, "{report:#?}");
    assert!(
        has(&report, "spot instance (Verda may evict it)"),
        "{report:#?}"
    );

    stub.discontinue_all();
    let report = check().await;
    assert_eq!(report.verdict, Verdict::Gone, "{report:#?}");
    assert!(has(&report, "discontinued by Verda"), "{report:#?}");

    // Teardown after an eviction: nothing to delete, detached volumes swept.
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
