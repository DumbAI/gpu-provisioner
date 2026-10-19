//! Stateful stub of the **Prime Intellect REST API** (`https://api.primeintellect.ai`,
//! paths under `/api/v1`), limited to the endpoints gpuprov's Prime Intellect
//! adapter uses.
//!
//! Vendor reference: <https://docs.primeintellect.ai/api-reference>. Shapes
//! below follow that reference (`GpuAvailability`, `APIPodConfig`,
//! `APISSHKeyConfig`, the offset-paginated envelope) — not gpuprov's parser.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /api/v1/availability/gpus` | [Get GPU availability](https://docs.primeintellect.ai/api-reference/availability/get-gpu-availability) | two in-stock `H100_80GB` offers (two providers/data centers); honors `gpu_count`, `regions`, `cloud_id`, `data_center_id`; `page`/`page_size` envelope `{items, totalCount}` |
//! | `GET /api/v1/ssh_keys/` | [Get SSH keys](https://docs.primeintellect.ai/api-reference/ssh-keys/get-ssh-keys) | offset-paginated `{total_count, offset, limit, data}` |
//! | `POST /api/v1/ssh_keys/` | [Upload SSH key](https://docs.primeintellect.ai/api-reference/ssh-keys/upload-ssh-key) | stores `{name, publicKey}`; returns `APISSHKeyConfig` |
//! | `POST /api/v1/pods/` | [Create pod](https://docs.primeintellect.ai/api-reference/pods/create-pod) | returns `APIPodConfig` (`PROVISIONING`); capacity fault → error body |
//! | `GET /api/v1/pods/` | [Get pods](https://docs.primeintellect.ai/api-reference/pods/get-pods) | offset-paginated `{total_count, offset, limit, data}` |
//! | `DELETE /api/v1/pods/{id}` | [Delete pod](https://docs.primeintellect.ai/api-reference/pods/delete-pod) | `{status: "DELETING"}`; the pod lists as `DELETING` once, then is gone |
//!
//! Pods start `PROVISIONING` (no `sshConnection`/`ip`), become `ACTIVE`
//! after one list with `sshConnection: "root@203.0.113.20 -p 2222"` — a
//! non-22 port, as marketplace hosts often NAT SSH.
//!
//! The reference documents no capacity error body for pod creation; the stub
//! answers `400 {"detail": "No capacity available for the requested
//! configuration"}`, the FastAPI-style shape the API uses for errors.

use async_trait::async_trait;
use gpu_provisioner::config::{AppConfig, PrimeIntellectConfig};
use serde_json::{json, Value};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, last_segment, query, query_all, Ids, StubServer};

const API_KEY: &str = "pi-test-key";
const SSH_PORT: u16 = 2222;

#[derive(Debug)]
struct Pod {
    id: String,
    name: String,
    cloud_id: String,
    provider: String,
    status: &'static str,
    /// Lists left before `PROVISIONING` becomes `ACTIVE`.
    boot_lists: u32,
    /// A deleted pod has been listed as `DELETING` once (next list drops it).
    deletion_listed: bool,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    pods: Vec<Pod>,
    ssh_keys: Vec<(String, String, String)>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
}

/// Simulated Prime Intellect account. See the module docs for the endpoint map.
pub struct PrimeIntellectStub {
    server: StubServer<Account>,
}

/// One `GpuAvailability` item (spec: `GpuAvailability`).
fn offer(cloud_id: &str, provider: &str, data_center: &str, country: &str, price: f64) -> Value {
    json!({
        "cloudId": cloud_id,
        "gpuType": "H100_80GB",
        "socket": "SXM5",
        "provider": provider,
        "region": "united_states",
        "dataCenter": data_center,
        "country": country,
        "gpuCount": 1,
        "gpuMemory": 80,
        "disk": {"minCount": 50, "defaultCount": 100, "maxCount": 1000, "pricePerUnit": 0.0001,
                 "step": 10, "defaultIncludedInPrice": true, "additionalInfo": null},
        "vcpu": {"defaultCount": 16},
        "memory": {"defaultCount": 128},
        "internetSpeed": null,
        "interconnect": null,
        "interconnectType": null,
        "provisioningTime": null,
        "stockStatus": "Available",
        "security": "secure_cloud",
        "prices": {"onDemand": price, "communityPrice": null, "isVariable": false, "currency": "USD"},
        "images": ["ubuntu_22_cuda_12"],
        "isSpot": false,
        "prepaidTime": null,
    })
}

/// The seeded marketplace: two `H100_80GB` offers, cheapest first.
fn catalog() -> Vec<Value> {
    vec![
        offer("n3-H100x1", "hyperstack", "CANADA-1", "CA", 1.90),
        offer("gpu_1x_h100_sxm5", "lambdalabs", "us-south-2", "US", 2.49),
    ]
}

fn error(status: u16, detail: &str) -> wiremock::ResponseTemplate {
    json(status, json!({"detail": detail}))
}

fn pod_json(pod: &Pod) -> Value {
    let active = pod.status == "ACTIVE";
    json!({
        "id": pod.id,
        "userId": "user-1",
        "teamId": null,
        "walletId": null,
        "name": pod.name,
        "type": "HOSTED",
        "providerType": pod.provider,
        "status": pod.status,
        "installationStatus": if active { "FINISHED" } else { "PENDING" },
        "installationFailure": null,
        "installationProgress": if active { 100 } else { 0 },
        "createdAt": "2026-10-19T00:00:00Z",
        "updatedAt": "2026-10-19T00:00:00Z",
        "terminatedAt": null,
        "gpuName": "H100_80GB",
        "gpuCount": 1,
        "socket": "SXM5",
        "priceHr": 1.90,
        "environmentType": "ubuntu_22_cuda_12",
        "primePortMapping": [],
        "sshConnection": if active {
            json!(format!("root@203.0.113.20 -p {SSH_PORT}"))
        } else {
            Value::Null
        },
        "ip": if active { json!("203.0.113.20") } else { Value::Null },
        "resources": null,
        "attachedResources": [],
        "isSpot": false,
        "autoRestart": false,
        "cloudId": pod.cloud_id,
    })
}

/// `{total_count, offset, limit, data}` for one offset page.
fn offset_page(req: &wiremock::Request, items: Vec<Value>) -> wiremock::ResponseTemplate {
    let offset: usize = query(req, "offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = query(req, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let total = items.len();
    let data: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
    json(
        200,
        json!({"total_count": total, "offset": offset, "limit": limit, "data": data, "status": null}),
    )
}

fn authed(req: &wiremock::Request) -> bool {
    header(req, "authorization").as_deref() == Some(&format!("Bearer {API_KEY}"))
}

#[async_trait]
impl VendorStub for PrimeIntellectStub {
    const CLOUD: &'static str = "primeintellect";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;

        // GET /api/v1/availability/gpus — Get GPU availability.
        server
            .route("GET", r"^/api/v1/availability/gpus/?$", |req, _| {
                if !authed(req) {
                    return error(401, "Authorization failed");
                }
                let gpu_count: Option<u32> = query(req, "gpu_count").and_then(|v| v.parse().ok());
                let regions = query_all(req, "regions");
                let cloud_id = query(req, "cloud_id");
                let dc = query(req, "data_center_id");
                let matching: Vec<Value> = catalog()
                    .into_iter()
                    .filter(|o| gpu_count.is_none_or(|c| o["gpuCount"] == c))
                    .filter(|o| {
                        regions.is_empty() || regions.iter().any(|r| o["region"] == r.as_str())
                    })
                    .filter(|o| cloud_id.as_deref().is_none_or(|c| o["cloudId"] == c))
                    .filter(|o| dc.as_deref().is_none_or(|d| o["dataCenter"] == d))
                    .collect();
                let page: usize = query(req, "page").and_then(|v| v.parse().ok()).unwrap_or(1);
                let size: usize = query(req, "page_size")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100);
                let total = matching.len();
                let items: Vec<Value> = matching
                    .into_iter()
                    .skip((page - 1) * size)
                    .take(size)
                    .collect();
                json(200, json!({"items": items, "totalCount": total}))
            })
            .await;
        // GET /api/v1/ssh_keys/ — Get SSH keys.
        server
            .route("GET", r"^/api/v1/ssh_keys/?$", |req, account| {
                if !authed(req) {
                    return error(401, "Authorization failed");
                }
                let keys = account
                    .ssh_keys
                    .iter()
                    .map(|(id, name, key)| {
                        json!({"id": id, "userId": "user-1", "name": name, "publicKey": key,
                               "isPrimary": false, "isUserKey": true})
                    })
                    .collect();
                offset_page(req, keys)
            })
            .await;
        // POST /api/v1/ssh_keys/ — Upload SSH key.
        server
            .route("POST", r"^/api/v1/ssh_keys/?$", |req, account| {
                let spec = body(req);
                let id = account.ids.next("key");
                let name = spec["name"].as_str().unwrap_or_default().to_string();
                let key = spec["publicKey"].as_str().unwrap_or_default().to_string();
                account
                    .ssh_keys
                    .push((id.clone(), name.clone(), key.clone()));
                json(
                    200,
                    json!({"id": id, "userId": "user-1", "name": name, "publicKey": key,
                           "isPrimary": false, "isUserKey": true}),
                )
            })
            .await;
        // POST /api/v1/pods/ — Create pod.
        server
            .route("POST", r"^/api/v1/pods/?$", |req, account| {
                if !authed(req) {
                    return error(401, "Authorization failed");
                }
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    return error(400, "No capacity available for the requested configuration");
                }
                let spec = body(req);
                let pod = Pod {
                    id: account.ids.next("pod"),
                    name: spec["pod"]["name"].as_str().unwrap_or_default().to_string(),
                    cloud_id: spec["pod"]["cloudId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    provider: spec["provider"]["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    status: "PROVISIONING",
                    boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                    deletion_listed: false,
                };
                let created = pod_json(&pod);
                account.pods.push(pod);
                account.creates += 1;
                json(200, created)
            })
            .await;
        // GET /api/v1/pods/ — Get pods (offset pagination). One list = one tick.
        server
            .route("GET", r"^/api/v1/pods/?$", |req, account| {
                if !authed(req) {
                    return error(401, "Authorization failed");
                }
                account.pods.retain(|p| !p.deletion_listed);
                for pod in account.pods.iter_mut() {
                    if pod.status == "PROVISIONING" && pod.boot_lists != u32::MAX {
                        if pod.boot_lists == 0 {
                            pod.status = "ACTIVE";
                        } else {
                            pod.boot_lists -= 1;
                        }
                    }
                }
                let pods = account.pods.iter().map(pod_json).collect();
                // A deleted pod is listed as DELETING exactly once.
                for pod in account.pods.iter_mut() {
                    if pod.status == "DELETING" {
                        pod.deletion_listed = true;
                    }
                }
                offset_page(req, pods)
            })
            .await;
        // DELETE /api/v1/pods/{pod_id} — Delete pod.
        server
            .route("DELETE", r"^/api/v1/pods/[^/]+/?$", |req, account| {
                let id = last_segment(req);
                match account.pods.iter_mut().find(|p| p.id == id) {
                    Some(pod) => {
                        // Lists as DELETING once (see GET), then disappears.
                        pod.status = "DELETING";
                        json(200, json!({"status": "DELETING"}))
                    }
                    None => error(404, "Pod not found"),
                }
            })
            .await;
        PrimeIntellectStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            primeintellect: PrimeIntellectConfig {
                api_key: Some(API_KEY.into()),
                base_url: Some(self.server.uri()),
                ..PrimeIntellectConfig::default()
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            gpus: Some("H100:1".into()),
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
            .filter(|p| p.status != "DELETING")
            .map(|p| format!("pod {} ({})", p.id, p.status))
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        self.server.state().pods.clear();
    }

    fn expected_login(&self) -> (String, u16) {
        ("root".into(), SSH_PORT)
    }
}

vendor_suite!(PrimeIntellectStub);
