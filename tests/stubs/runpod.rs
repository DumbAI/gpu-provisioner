//! Stateful stub of the **RunPod REST API v2** (`https://api.runpod.io/v2`),
//! limited to the endpoints gpuprov's RunPod adapter uses.
//!
//! Vendor reference: <https://docs.runpod.io/api-reference-v2> (OpenAPI:
//! <https://docs.runpod.io/api-reference-v2/openapi.json>). Shapes below
//! follow that spec — not gpuprov's parser — so the stub independently checks
//! the adapter's reading of the API.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /v2/catalog/gpus` | [List GPU types](https://docs.runpod.io/api-reference-v2/catalog/list-gpu-types) | two GPU types; `include=AVAILABILITY` adds per-data-center stock |
//! | `GET /v2/catalog/gpus/{id}` | [Get a GPU type](https://docs.runpod.io/api-reference-v2/catalog/get-a-gpu-type) | one type with stock |
//! | `GET /v2/catalog/datacenters` | [List data centers](https://docs.runpod.io/api-reference-v2/catalog/list-data-centers) | `US-TX-3`, `EU-RO-1` |
//! | `GET /v2/account/ssh-keys` | [List SSH keys](https://docs.runpod.io/api-reference-v2/account/list-registered-ssh-public-keys) | the account's keys (read-only for gpuprov) |
//! | `POST /v2/pods` | [Create a pod](https://docs.runpod.io/api-reference-v2/pods/create-a-pod) | `201` + pod; capacity fault → `ErrorResponse` |
//! | `GET /v2/pods` | [List pods](https://docs.runpod.io/api-reference-v2/pods/list-pods) | cursor-paginated (page size 1, to exercise paging) |
//! | `DELETE /v2/pods/{id}` | [Terminate a pod](https://docs.runpod.io/api-reference-v2/pods/terminate-a-pod) | `204`; pod `TERMINATED` on next list, then gone |
//!
//! Pods start `PROVISIONING`, become `RUNNING` with an `ssh.direct` endpoint
//! (`root` @ a public port) after one list, and are gone after termination.

use async_trait::async_trait;
use gpu_provisioner::config::{AppConfig, RunpodConfig};
use serde_json::{json, Value};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, empty, json, last_segment, query, Ids, StubServer};

const API_KEY: &str = "rp-test-key";
const SSH_PORT: u16 = 40123;

#[derive(Debug)]
struct Pod {
    id: String,
    name: String,
    gpu_id: String,
    data_center: String,
    status: &'static str,
    /// Lists left before `PROVISIONING` becomes `RUNNING`.
    boot_lists: u32,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    pods: Vec<Pod>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
}

/// Simulated RunPod account. See the module docs for the endpoint map.
pub struct RunpodStub {
    server: StubServer<Account>,
}

/// Two GPU types matching `RTX4090:1` (secure is pricier than community), so
/// the plan has several offers to fail over across.
fn gpu_type(id: &str, with_stock: bool) -> Value {
    let mut gpu = json!({
        "id": id,
        "name": "RTX 4090",
        "pool": null,
        "manufacturer": "NVIDIA",
        "memory": 24,
        "secure": true,
        "community": true,
        "price": {"secure": 0.69, "community": 0.34},
        "maxCount": {"secure": 8, "community": 8},
    });
    if with_stock {
        gpu["availability"] = json!("HIGH");
        gpu["dataCenters"] = json!([
            {"id": "US-TX-3", "name": "Texas 3", "availability": "HIGH"},
            {"id": "EU-RO-1", "name": "Romania 1", "availability": "MEDIUM"},
        ]);
    }
    gpu
}

/// RFC 7807-style `ErrorResponse` (spec: `components/schemas/ErrorResponse`).
fn error(status: u16, title: &str, detail: &str) -> wiremock::ResponseTemplate {
    json(
        status,
        json!({"title": title, "status": status, "detail": detail, "errors": []}),
    )
}

fn pod_json(pod: &Pod) -> Value {
    let running = pod.status == "RUNNING";
    json!({
        "id": pod.id,
        "name": pod.name,
        "status": pod.status,
        "actions": ["terminate"],
        "image": "runpod/base:1.4.1-cuda1290-ubuntu2204",
        "args": "",
        "disk": 100,
        "mounts": {},
        "ports": ["22/tcp"],
        "env": {},
        "registry": null,
        "cloud": "SECURE",
        "dataCenterId": pod.data_center,
        "cudaVersion": "12.9",
        "gpu": {"id": pod.gpu_id, "count": 1, "vcpuCount": 8, "memory": 46},
        "ssh": {
            "proxy": null,
            "direct": if running {
                json!({"host": "203.0.113.10", "port": SSH_PORT, "username": "root",
                       "command": format!("ssh root@203.0.113.10 -p {SSH_PORT}")})
            } else {
                Value::Null
            },
        },
        "template": null,
        "cost": 0.34,
        "locked": false,
        "runtime": if running {
            json!({"uptime": 5, "ports": [
                {"private": 22, "public": SSH_PORT, "type": "tcp", "ip": "203.0.113.10"}
            ]})
        } else {
            Value::Null
        },
        "createdAt": "2026-10-19T00:00:00Z",
        "startedAt": null,
        "globalNetworking": false,
    })
}

#[async_trait]
impl VendorStub for RunpodStub {
    const CLOUD: &'static str = "runpod";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;
        let authed = |req: &wiremock::Request| {
            super::route::header(req, "authorization").as_deref()
                == Some(&format!("Bearer {API_KEY}"))
        };

        // GET /v2/catalog/gpus — List GPU types.
        server
            .route("GET", r"^/v2/catalog/gpus$", move |req, _| {
                if !authed(req) {
                    return error(401, "Unauthorized", "invalid API key");
                }
                let with_stock = query(req, "include").as_deref() == Some("AVAILABILITY");
                json(
                    200,
                    json!({"gpus": [gpu_type("NVIDIA GeForce RTX 4090", with_stock)]}),
                )
            })
            .await;
        // GET /v2/catalog/gpus/{id} — Get a GPU type.
        server
            .route("GET", r"^/v2/catalog/gpus/[^/]+$", move |req, _| {
                let id = last_segment(req);
                if id != "NVIDIA GeForce RTX 4090" {
                    return error(404, "Not Found", "gpu type not found");
                }
                let with_stock = query(req, "include").as_deref() == Some("AVAILABILITY");
                json(200, gpu_type(&id, with_stock))
            })
            .await;
        // GET /v2/catalog/datacenters — List data centers.
        server
            .route("GET", r"^/v2/catalog/datacenters$", |_, _| {
                json(
                    200,
                    json!({"dataCenters": [
                        {"id": "US-TX-3", "name": "Texas 3", "region": "NORTH_AMERICA",
                         "networkVolumeTypes": [], "compliance": [], "globalNetwork": false},
                        {"id": "EU-RO-1", "name": "Romania 1", "region": "EUROPE",
                         "networkVolumeTypes": [], "compliance": [], "globalNetwork": false},
                    ]}),
                )
            })
            .await;
        // GET /v2/account/ssh-keys — List registered SSH public keys.
        server
            .route("GET", r"^/v2/account/ssh-keys$", |_, _| {
                json(
                    200,
                    json!({"keys": ["ssh-ed25519 AAAAexisting user@laptop"]}),
                )
            })
            .await;
        // POST /v2/pods — Create a pod.
        server
            .route("POST", r"^/v2/pods$", |req, account| {
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    return error(
                        400,
                        "Bad Request",
                        "There are no longer any instances available with the requested \
                         specifications. Please refresh and try again.",
                    );
                }
                let spec = body(req);
                let id = account.ids.next("pod");
                let pod = Pod {
                    id: id.clone(),
                    name: spec["name"].as_str().unwrap_or_default().to_string(),
                    gpu_id: spec["gpu"]["id"].as_str().unwrap_or_default().to_string(),
                    data_center: spec["dataCenterIds"][0]
                        .as_str()
                        .unwrap_or("US-TX-3")
                        .to_string(),
                    status: "PROVISIONING",
                    boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                };
                let created = pod_json(&pod);
                account.pods.push(pod);
                account.creates += 1;
                json(201, created)
            })
            .await;
        // GET /v2/pods — List pods (cursor pagination, one pod per page).
        server
            .route("GET", r"^/v2/pods$", |req, account| {
                // Advance the simulation: one list = one tick.
                account.pods.retain(|p| p.status != "TERMINATED");
                for pod in account.pods.iter_mut() {
                    if pod.status == "PROVISIONING" && pod.boot_lists != u32::MAX {
                        if pod.boot_lists == 0 {
                            pod.status = "RUNNING";
                        } else {
                            pod.boot_lists -= 1;
                        }
                    }
                }
                let start: usize = query(req, "cursor")
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);
                let page: Vec<Value> = account.pods.iter().skip(start).take(1).map(pod_json).collect();
                let next = start + 1;
                let has_next = next < account.pods.len();
                json(
                    200,
                    json!({
                        "pods": page,
                        "pagination": {
                            "nextCursor": if has_next { json!(next.to_string()) } else { Value::Null },
                            "hasNextPage": has_next,
                        },
                    }),
                )
            })
            .await;
        // DELETE /v2/pods/{id} — Terminate a pod.
        server
            .route("DELETE", r"^/v2/pods/[^/]+$", |req, account| {
                let id = last_segment(req);
                match account.pods.iter_mut().find(|p| p.id == id) {
                    Some(pod) => {
                        pod.status = "TERMINATED";
                        empty(204)
                    }
                    None => error(404, "Not Found", "pod not found"),
                }
            })
            .await;
        RunpodStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            runpod: RunpodConfig {
                api_key: Some(API_KEY.into()),
                base_url: Some(self.server.uri()),
                ..RunpodConfig::default()
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            gpus: Some("RTX4090:1".into()),
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
        self.server
            .state()
            .pods
            .iter()
            .filter(|p| p.status != "TERMINATED")
            .map(|p| format!("pod {} ({})", p.id, p.status))
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn expected_login(&self) -> (String, u16) {
        ("root".into(), SSH_PORT)
    }
}

vendor_suite!(RunpodStub);
