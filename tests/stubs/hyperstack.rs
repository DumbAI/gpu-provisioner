//! Stateful stub of the **Hyperstack Infrahub API**
//! (`https://infrahub-api.nexgencloud.com/v1`, key in the `api_key` header),
//! limited to the endpoints gpuprov's Hyperstack adapter uses.
//!
//! Vendor reference: <https://docs.hyperstack.cloud/docs/api-reference/>
//! (OpenAPI, as shipped in the official Go SDK:
//! <https://github.com/NexGenCloud/hyperstack-sdk-go/blob/main/api/openapi.yaml>).
//! Shapes below — including the `{status, message, …}` success envelopes and
//! the `{status: false, message, error_reason}` error body — follow that
//! spec, not gpuprov's parser, so the stub independently checks the adapter.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /core/flavors` | [List flavors](https://docs.hyperstack.cloud/docs/api-reference/core-resources/flavors/) | RTX-A6000 1× in `CANADA-1` and `US-1`, RTX-A4000 1× in `NORWAY-1` (grouped `{gpu, region_name, flavors}`) |
//! | `GET /core/stocks` | [GPU stock](https://docs.hyperstack.cloud/docs/api-reference/core-resources/stock/) | per-region model stock keyed by configuration (`1x`, `8x`, …) |
//! | `GET /pricebook` | [Pricebook](https://docs.hyperstack.cloud/docs/billing/pricebook) | bare array of `{name, value}` per-GPU hourly prices |
//! | `GET /core/images` | [List images](https://docs.hyperstack.cloud/docs/api-reference/core-resources/images/) | Ubuntu 22.04/24.04 with and without CUDA, grouped per region |
//! | `GET /core/environments` | [List environments](https://docs.hyperstack.cloud/docs/api-reference/core-resources/environments/) | paginated (`page`, `pageSize`) |
//! | `POST /core/environments` | [Create environment](https://docs.hyperstack.cloud/docs/api-reference/core-resources/environments/) | `{name, region}`; duplicate name → `409` |
//! | `GET /core/keypairs` | [List key pairs](https://docs.hyperstack.cloud/docs/api-reference/core-resources/keypairs/) | paginated, each with its environment |
//! | `POST /core/keypairs` | [Import key pair](https://docs.hyperstack.cloud/docs/api-reference/core-resources/keypairs/) | `{name, environment_name, public_key}`; unknown environment → `404` |
//! | `POST /core/virtual-machines` | [Create VMs](https://docs.hyperstack.cloud/docs/api-reference/core-resources/virtual-machines/) | validates the required fields; `{status, message, instances: [vm]}`; capacity fault → `400` stock-out |
//! | `GET /core/virtual-machines` | [List VMs](https://docs.hyperstack.cloud/docs/api-reference/core-resources/virtual-machines/) | filters `environment` (+ `exact_environment_match`) and `search` |
//! | `DELETE /core/virtual-machines/{vm_id}` | [Delete VM](https://docs.hyperstack.cloud/docs/api-reference/core-resources/virtual-machines/) | `DELETING` for one list, then gone; unknown id → `404` |
//!
//! VMs start `CREATING`, become `ACTIVE` with a floating IP after one list
//! (VM states: <https://docs.hyperstack.cloud/docs/virtual-machines/manage-vm-state>),
//! and are gone after deletion. Environments and key pairs are shared
//! per-region account resources, never per-VM leftovers.
//!
//! The exact wording of a stock-out on create is not documented; the stub
//! answers `400` with an `error_reason` of `out_of_stock` and a message
//! saying the flavor has no available capacity.

use async_trait::async_trait;
use gpu_provisioner::config::{ApiKeyConfig, AppConfig};
use serde_json::{json, Value};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, last_segment, query, StubServer};

const API_KEY: &str = "hs-test-key";

#[derive(Debug)]
struct Vm {
    id: i64,
    name: String,
    environment: String,
    region: String,
    flavor: String,
    labels: Vec<String>,
    status: &'static str,
    /// Lists left before `CREATING` becomes `ACTIVE` (`u32::MAX` = held).
    boot_lists: u32,
}

#[derive(Debug, Default)]
pub struct Account {
    next_id: i64,
    /// (id, name, region)
    environments: Vec<(i64, String, String)>,
    /// (id, name, environment, public_key)
    keypairs: Vec<(i64, String, String, String)>,
    vms: Vec<Vm>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    /// Boot payloads (user data / startup scripts) attached at create.
    boot_payloads: Vec<String>,
}

impl Account {
    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }
}

/// Simulated Hyperstack account. See the module docs for the endpoint map.
pub struct HyperstackStub {
    server: StubServer<Account>,
}

/// The documented error body.
fn error(status: u16, message: &str, reason: &str) -> wiremock::ResponseTemplate {
    json(
        status,
        json!({"status": false, "message": message, "error_reason": reason}),
    )
}

fn flavor(id: i64, name: &str, gpu: &str, region: &str, cpu: u32, ram: f64) -> Value {
    json!({
        "id": id,
        "name": name,
        "display_name": name,
        "region_name": region,
        "cpu": cpu,
        "ram": ram,
        "disk": 100,
        "ephemeral": 0,
        "gpu": gpu,
        "gpu_count": 1,
        "stock_available": true,
        "created_at": "2026-01-01T00:00:00",
        "labels": [{"id": 1, "label": "network_optimised"}],
        "features": {},
        "image_restrictions": {"has_image_restrictions": false, "restriction_type": null, "compatible_images": []},
    })
}

fn image(id: i64, name: &str, region: &str) -> Value {
    json!({
        "id": id, "name": name, "region_name": region, "type": "Ubuntu",
        "version": "", "size": 0, "display_size": "", "is_public": true, "labels": [],
        "flavor_restrictions": {"has_flavor_restrictions": false, "compatible_flavors": []},
    })
}

fn environment_json(id: i64, name: &str, region: &str) -> Value {
    json!({
        "id": id, "name": name, "region": region, "created_at": "2026-01-01T00:00:00",
        "features": {"network_optimised": true, "green_status": "GREEN"},
    })
}

fn vm_json(vm: &Vm) -> Value {
    let active = vm.status == "ACTIVE";
    json!({
        "id": vm.id,
        "name": vm.name,
        "status": vm.status,
        "power_state": if active { "RUNNING" } else { "NOSTATE" },
        "vm_state": if active { "active" } else { "building" },
        "fixed_ip": "10.0.0.5",
        "floating_ip": if active { json!("192.0.2.30") } else { Value::Null },
        "floating_ip_status": if active { "ATTACHED" } else { "ATTACHING" },
        "environment": {"id": 1, "name": vm.environment, "org_id": 1, "region": vm.region,
                        "features": {"network_optimised": true, "green_status": "GREEN"}},
        "flavor": {"id": 1, "name": vm.flavor, "cpu": 8, "ram": 58.0, "disk": 100, "ephemeral": 0,
                   "gpu": "RTX-A6000", "gpu_count": 1, "labels": [], "features": {}},
        "image": {"name": "Ubuntu Server 22.04 LTS R535 CUDA 12.2"},
        "keypair": {"name": "gpuprov"},
        "labels": vm.labels,
        "security_rules": [],
        "volume_attachments": [],
        "locked": false,
        "os": "ubuntu",
        "port_randomization": false,
        "requires_public_ip": true,
        "created_at": "2026-10-19T00:00:00",
        "features": {},
    })
}

/// `page`/`pageSize` pagination over a list (spec defaults: 1 and 10).
fn paginate(req: &wiremock::Request, items: Vec<Value>) -> (Vec<Value>, usize, usize) {
    let page: usize = query(req, "page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let size: usize = query(req, "pageSize")
        .and_then(|p| p.parse().ok())
        .unwrap_or(10)
        .max(1);
    let slice = items
        .into_iter()
        .skip((page - 1) * size)
        .take(size)
        .collect();
    (slice, page, size)
}

#[async_trait]
impl VendorStub for HyperstackStub {
    const CLOUD: &'static str = "hyperstack";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;
        let unauthorized = |req: &wiremock::Request| {
            (header(req, "api_key").as_deref() != Some(API_KEY))
                .then(|| error(401, "Invalid API Key", "unauthorized"))
        };

        // GET /core/flavors — flavors grouped by GPU and region.
        server
            .route("GET", r"^/v1/core/flavors$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let groups = [
                    ("RTX-A6000", "CANADA-1", flavor(11, "n3-RTX-A6000x1", "RTX-A6000", "CANADA-1", 8, 58.0)),
                    ("RTX-A6000", "US-1", flavor(12, "n3-RTX-A6000x1", "RTX-A6000", "US-1", 8, 58.0)),
                    ("RTX-A4000", "NORWAY-1", flavor(13, "n3-RTX-A4000x1", "RTX-A4000", "NORWAY-1", 6, 24.0)),
                ];
                let region = query(req, "region");
                let data: Vec<Value> = groups
                    .into_iter()
                    .filter(|(_, r, _)| region.as_deref().is_none_or(|want| want == *r))
                    .map(|(gpu, region, f)| json!({"gpu": gpu, "region_name": region, "flavors": [f]}))
                    .collect();
                json(200, json!({"status": true, "message": "Getting flavors success", "data": data}))
            })
            .await;
        // GET /core/stocks — per-region GPU stock.
        server
            .route("GET", r"^/v1/core/stocks$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let model = |name: &str, one_x: i64| {
                    json!({"model": name, "available": one_x.to_string(),
                           "planned_7_days": "0", "planned_30_days": "0", "planned_100_days": "0",
                           "configurations": {"1x": one_x, "2x": 0, "4x": 0, "8x": 0, "10x": 0}})
                };
                json(
                    200,
                    json!({"stocks": [
                        {"region": "CANADA-1", "stock-type": "GPU", "models": [model("RTX-A6000", 3)]},
                        {"region": "US-1", "stock-type": "GPU", "models": [model("RTX-A6000", 2)]},
                        {"region": "NORWAY-1", "stock-type": "GPU", "models": [model("RTX-A4000", 5)]},
                    ]}),
                )
            })
            .await;
        // GET /pricebook — per-GPU hourly prices (a bare array).
        server
            .route("GET", r"^/v1/pricebook$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let price = |id: i64, name: &str, value: f64| {
                    json!({"id": id, "name": name, "value": value, "original_value": value,
                           "discount_applied": false, "start_time": "2026-01-01T00:00:00",
                           "end_time": null})
                };
                json(
                    200,
                    json!([price(1, "RTX-A6000", 0.5), price(2, "RTX-A4000", 0.15)]),
                )
            })
            .await;
        // GET /core/images — public images, grouped per region.
        server
            .route("GET", r"^/v1/core/images$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let region = query(req, "region").unwrap_or_else(|| "CANADA-1".into());
                let images = vec![
                    image(101, "Ubuntu Server 22.04 LTS R535 CUDA 12.2", &region),
                    image(
                        102,
                        "Ubuntu Server 24.04 LTS R570 CUDA 12.8 with Docker",
                        &region,
                    ),
                    image(103, "Ubuntu Server 24.04 LTS", &region),
                ];
                json(
                    200,
                    json!({"status": true, "message": "Getting images success", "images": [
                        {"region_name": region, "type": "Ubuntu", "logo": "", "images": images}
                    ]}),
                )
            })
            .await;
        // GET /core/environments — paginated environments.
        server
            .route("GET", r"^/v1/core/environments$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let all: Vec<Value> = account
                    .environments
                    .iter()
                    .map(|(id, name, region)| environment_json(*id, name, region))
                    .collect();
                let count = all.len();
                let (page, number, size) = paginate(req, all);
                json(
                    200,
                    json!({"status": true, "message": "Getting environments success",
                           "environments": page, "count": count, "page": number, "page_size": size}),
                )
            })
            .await;
        // POST /core/environments — create an environment in a region.
        server
            .route("POST", r"^/v1/core/environments$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let (Some(name), Some(region)) = (spec["name"].as_str(), spec["region"].as_str())
                else {
                    return error(400, "name and region are required", "invalid_request");
                };
                if account.environments.iter().any(|(_, n, _)| n == name) {
                    return error(409, "Environment with this name already exists", "conflict");
                }
                let id = account.next_id();
                account
                    .environments
                    .push((id, name.to_string(), region.to_string()));
                json(
                    200,
                    json!({"status": true, "message": "Environment created",
                           "environment": environment_json(id, name, region)}),
                )
            })
            .await;
        // GET /core/keypairs — paginated key pairs.
        server
            .route("GET", r"^/v1/core/keypairs$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let all: Vec<Value> = account
                    .keypairs
                    .iter()
                    .map(|(id, name, env, key)| {
                        let region = account
                            .environments
                            .iter()
                            .find(|(_, n, _)| n == env)
                            .map(|(_, _, r)| r.clone())
                            .unwrap_or_default();
                        json!({"id": id, "name": name, "public_key": key, "fingerprint": "aa:bb",
                               "created_at": "2026-01-01T00:00:00",
                               "environment": environment_json(0, env, &region)})
                    })
                    .collect();
                let count = all.len();
                let (page, number, size) = paginate(req, all);
                json(
                    200,
                    json!({"status": true, "message": "Getting keypairs success",
                           "keypairs": page, "count": count, "page": number, "page_size": size}),
                )
            })
            .await;
        // POST /core/keypairs — import a public key into an environment.
        server
            .route("POST", r"^/v1/core/keypairs$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let (Some(name), Some(env), Some(key)) = (
                    spec["name"].as_str(),
                    spec["environment_name"].as_str(),
                    spec["public_key"].as_str(),
                ) else {
                    return error(
                        400,
                        "name, environment_name and public_key are required",
                        "invalid_request",
                    );
                };
                if !account.environments.iter().any(|(_, n, _)| n == env) {
                    return error(404, "Environment not found", "not_found");
                }
                if account
                    .keypairs
                    .iter()
                    .any(|(_, n, e, _)| n == name && e == env)
                {
                    return error(409, "Keypair with this name already exists", "conflict");
                }
                let id = account.next_id();
                account
                    .keypairs
                    .push((id, name.to_string(), env.to_string(), key.to_string()));
                json(
                    200,
                    json!({"status": true, "message": "Keypair imported",
                           "keypair": {"id": id, "name": name, "public_key": key}}),
                )
            })
            .await;
        // POST /core/virtual-machines — create VMs.
        server
            .route(
                "POST",
                r"^/v1/core/virtual-machines$",
                move |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let spec = body(req);
                    let field = |k: &str| spec[k].as_str().map(str::to_string);
                    let (Some(name), Some(env), Some(flavor), Some(key)) = (
                        field("name"),
                        field("environment_name"),
                        field("flavor_name"),
                        field("key_name"),
                    ) else {
                        return error(
                            400,
                            "name, environment_name, flavor_name and key_name are required",
                            "invalid_request",
                        );
                    };
                    if spec["count"].as_u64().is_none() {
                        return error(400, "count is required", "invalid_request");
                    }
                    let Some(region) = account
                        .environments
                        .iter()
                        .find(|(_, n, _)| *n == env)
                        .map(|(_, _, r)| r.clone())
                    else {
                        return error(404, "Environment not found", "not_found");
                    };
                    if !account
                        .keypairs
                        .iter()
                        .any(|(_, n, e, _)| *n == key && *e == env)
                    {
                        return error(404, "Keypair not found in environment", "not_found");
                    }
                    if account.capacity_faults > 0 {
                        account.capacity_faults -= 1;
                        return error(
                            400,
                            &format!(
                                "There is no available capacity for flavor {flavor} in {region}"
                            ),
                            "out_of_stock",
                        );
                    }
                    let id = account.next_id();
                    let vm = Vm {
                        id,
                        name,
                        environment: env,
                        region,
                        flavor,
                        labels: spec["labels"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        status: "CREATING",
                        boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                    };
                    let created = vm_json(&vm);
                    account.vms.push(vm);
                    if let Some(user_data) = spec["user_data"].as_str() {
                        account.boot_payloads.push(user_data.to_string());
                    }
                    account.creates += 1;
                    json(
                        200,
                        json!({"status": true, "message": "Virtual machine creation started",
                           "instances": [created]}),
                    )
                },
            )
            .await;
        // GET /core/virtual-machines — list VMs (one list = one simulation tick).
        server
            .route(
                "GET",
                r"^/v1/core/virtual-machines$",
                move |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let env = query(req, "environment");
                    let exact = query(req, "exact_environment_match").as_deref() == Some("true");
                    let search = query(req, "search").unwrap_or_default();
                    let all: Vec<Value> = account
                        .vms
                        .iter()
                        .filter(|vm| match &env {
                            Some(e) if exact => vm.environment == *e,
                            Some(e) => vm.environment.contains(e.as_str()),
                            None => true,
                        })
                        .filter(|vm| vm.name.contains(&search))
                        .map(vm_json)
                        .collect();
                    let count = all.len();
                    let (page, number, size) = paginate(req, all);
                    // Advance the simulation.
                    account.vms.retain(|vm| vm.status != "DELETING");
                    for vm in account.vms.iter_mut() {
                        if vm.status == "CREATING" && vm.boot_lists != u32::MAX {
                            if vm.boot_lists <= 1 {
                                vm.status = "ACTIVE";
                            } else {
                                vm.boot_lists -= 1;
                            }
                        }
                    }
                    json(
                        200,
                        json!({"status": true, "message": "Getting virtual machines success",
                           "instances": page, "count": count, "page": number, "page_size": size}),
                    )
                },
            )
            .await;
        // DELETE /core/virtual-machines/{vm_id} — delete a VM.
        server
            .route("DELETE", r"^/v1/core/virtual-machines/[^/]+$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let id: i64 = last_segment(req).parse().unwrap_or(-1);
                match account.vms.iter_mut().find(|vm| vm.id == id) {
                    Some(vm) => {
                        vm.status = "DELETING";
                        json(200, json!({"status": true, "message": "Virtual machine deletion started"}))
                    }
                    None => error(404, "Virtual machine not found", "not_found"),
                }
            })
            .await;
        HyperstackStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            hyperstack: ApiKeyConfig {
                api_key: Some(API_KEY.into()),
                base_url: Some(format!("{}/v1", self.server.uri())),
                ..ApiKeyConfig::default()
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
        self.server
            .state()
            .vms
            .iter()
            .filter(|vm| vm.status != "DELETING")
            .map(|vm| format!("vm {} {} ({})", vm.id, vm.name, vm.status))
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        self.server.state().vms.clear();
    }

    fn boot_payloads(&self) -> Vec<String> {
        self.server.state().boot_payloads.clone()
    }

    fn expected_login(&self) -> (String, u16) {
        ("ubuntu".into(), 22)
    }
}

vendor_suite!(HyperstackStub);
