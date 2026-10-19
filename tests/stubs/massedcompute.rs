//! Stateful stub of the **Massed Compute VM API**
//! (`https://vm.massedcompute.com/api/v1`, `Authorization: Bearer <token>`),
//! limited to the endpoints gpuprov's Massed Compute adapter uses.
//!
//! Vendor reference: <https://vm-docs.massedcompute.com/api/v1> (OpenAPI:
//! <https://vm-docs.massedcompute.com/redocusaurus/plugin-redoc-0.yaml>).
//! Shapes below follow that spec — not gpuprov's parser — so the stub
//! independently checks the adapter's reading of the API.
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /gpu-inventory` | [Retrieve available GPU configurations](https://vm-docs.massedcompute.com/api/v1#tag/Inventory/operation/getGpuInventory) | two in-stock 1× RTX A6000 products (different vCPU/RAM) plus a sold-out L40S |
//! | `GET /images` | [Retrieve available images](https://vm-docs.massedcompute.com/api/v1#tag/Images) | an app stack, Ubuntu 22.04 and Ubuntu 24.04 |
//! | `GET /ssh-keys` | [Retrieve SSH keys](https://vm-docs.massedcompute.com/api/v1#tag/SSH-Keys) | `{sshKeys: [{id, name, public_key}]}` |
//! | `POST /ssh-keys` | [Add an SSH key](https://vm-docs.massedcompute.com/api/v1#tag/SSH-Keys) | body `{name, publicKey}` → `{sshKey: {id, name}}` |
//! | `POST /instance/launch` | [Deploy new instances](https://vm-docs.massedcompute.com/api/v1#tag/Instances) | `202 {response: <uuid>}`; capacity fault → the documented "No Availability" deploy error |
//! | `GET /instance` | [Retrieve running instances](https://vm-docs.massedcompute.com/api/v1#tag/Instances) | `{runningInstances: [...]}`; booting = `os_booted: 0`, no IP |
//! | `POST /instance/terminate` | [Terminate instances](https://vm-docs.massedcompute.com/api/v1#tag/Instances) | `202 {response: {data: {terminated_instances}}}`; `terminating` for one list, then gone |
//!
//! Deploy error wording comes from the "Deployment Errors" page
//! (<https://vm-docs.massedcompute.com/docs/deploy/deployment-errors>). That
//! page gives the messages but neither the HTTP status nor the body shape;
//! this stub assumes `400 {"message": "<text>"}`.
//!
//! Instances start `rented` with `os_booted: 0` and no IP (the spec's example
//! instance is `rented` with `os_booted: 1`), become reachable — `os_booted:
//! 1`, an IP, user `Ubuntu` — after one list, and disappear after
//! termination.

use async_trait::async_trait;
use gpu_provisioner::config::{ApiKeyConfig, AppConfig};
use serde_json::{json, Value};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, Ids, StubServer};

const TOKEN: &str = "mc-test-token";
/// The login user the API reports for its Ubuntu images.
const LOGIN_USER: &str = "Ubuntu";

#[derive(Debug)]
struct Instance {
    uuid: String,
    name: String,
    product: String,
    image_id: i64,
    /// `rented` while alive, `terminating` after a terminate request.
    status: &'static str,
    /// Lists left before the OS reports booted (`u32::MAX` = held).
    boot_lists: u32,
}

impl Instance {
    fn booted(&self) -> bool {
        self.boot_lists == 0
    }
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    instances: Vec<Instance>,
    ssh_keys: Vec<(String, String, String)>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
}

/// Simulated Massed Compute account. See the module docs for the endpoint map.
pub struct MassedComputeStub {
    server: StubServer<Account>,
}

/// One `gpu_inventory` entry (spec: `additionalProperties` of `gpu_inventory`).
fn product(name: &str, cents: u64, vcpu: u32, ram: u32, capacity: u32) -> Value {
    json!({
        "instance_type": {
            "name": name,
            "price_cents_per_hour": cents,
            "specs": {"vcpu_count": vcpu, "memory_gib": ram, "storage_gb": 256},
        },
        "regions_with_capacity_available": if capacity > 0 {
            json!([{"name": "us-central-3"}])
        } else {
            json!([])
        },
        "capacity_available": capacity,
    })
}

/// A deploy error in the assumed `400 {"message": …}` shape (see module docs).
fn deploy_error(message: &str) -> wiremock::ResponseTemplate {
    json(400, json!({ "message": message }))
}

fn instance_json(instance: &Instance) -> Value {
    let booted = instance.booted();
    json!({
        "uuid": instance.uuid,
        "name": instance.name,
        "ip": if booted { json!("198.51.100.20") } else { json!("") },
        "username": LOGIN_USER,
        "password": "not-used",
        "status": instance.status,
        "os_booted": i64::from(booted),
        "command_startup": "",
        "created": "2026-10-19T00:00:00.000Z",
        "active": 1,
        "image": {"id": instance.image_id, "name": "Ubuntu 24.04"},
        "product": {
            "name": instance.product,
            "gpu_count": 1,
            "vcpu": 12,
            "ram": 64,
            "storage": 256,
            "price_hr": "0.57",
            "final_price_hr": "0.57",
        },
    })
}

#[async_trait]
impl VendorStub for MassedComputeStub {
    const CLOUD: &'static str = "massedcompute";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;
        let unauthorized = |req: &wiremock::Request| {
            (header(req, "authorization").as_deref() != Some(&format!("Bearer {TOKEN}")))
                .then(|| json(401, json!({"message": "Authentication not correct."})))
        };

        // GET /gpu-inventory — available GPU configurations.
        server
            .route("GET", r"^/api/v1/gpu-inventory$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                json(
                    200,
                    json!({"gpu_inventory": {
                        "gpu_1x_a6000": product("gpu_1x_a6000", 57, 12, 64, 4),
                        "gpu_1x_a6000_32c": product("gpu_1x_a6000_32c", 69, 32, 128, 2),
                        "gpu_1x_l40s": product("gpu_1x_l40s", 97, 16, 96, 0),
                    }}),
                )
            })
            .await;
        // GET /images — deployable images.
        server
            .route("GET", r"^/api/v1/images$", move |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                json(
                    200,
                    json!({"images": [
                        {"vm_image_id": 7, "vm_image_name": "Art", "vm_image_description": "Stable Diffusion desktop"},
                        {"vm_image_id": 11, "vm_image_name": "Ubuntu 22.04", "vm_image_description": "Base image"},
                        {"vm_image_id": 15, "vm_image_name": "Ubuntu 24.04", "vm_image_description": "Base image"},
                    ]}),
                )
            })
            .await;
        // GET /ssh-keys — keys on the account.
        server
            .route("GET", r"^/api/v1/ssh-keys$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let keys: Vec<Value> = account
                    .ssh_keys
                    .iter()
                    .map(|(id, name, key)| json!({"id": id, "name": name, "public_key": key}))
                    .collect();
                json(200, json!({ "sshKeys": keys }))
            })
            .await;
        // POST /ssh-keys — add a key ({name, publicKey}).
        server
            .route("POST", r"^/api/v1/ssh-keys$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let (Some(name), Some(key)) = (spec["name"].as_str(), spec["publicKey"].as_str())
                else {
                    return json(400, json!({"message": "name and publicKey are required"}));
                };
                let id = account.ids.next("key");
                account
                    .ssh_keys
                    .push((id.clone(), name.to_string(), key.to_string()));
                json(200, json!({"sshKey": {"id": id, "name": name}}))
            })
            .await;
        // POST /instance/launch — deploy one instance.
        server
            .route("POST", r"^/api/v1/instance/launch$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let (Some(image_id), Some(product)) =
                    (spec["imageId"].as_i64(), spec["productName"].as_str())
                else {
                    return deploy_error("Please select an image");
                };
                if spec["regionName"].as_str().is_none() {
                    return deploy_error("regionName is required");
                }
                let keys: Vec<&str> = spec["sshKeys"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if keys
                    .iter()
                    .any(|k| !account.ssh_keys.iter().any(|(_, name, _)| name == k))
                {
                    return deploy_error("SSH Key does not exist");
                }
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    return deploy_error(
                        "No Availability. Select a different quantity or GPU type.",
                    );
                }
                let uuid = account.ids.next("8b52a46b");
                account.instances.push(Instance {
                    uuid: uuid.clone(),
                    name: spec["instanceName"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    product: product.to_string(),
                    image_id,
                    status: "rented",
                    boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                });
                account.creates += 1;
                json(202, json!({ "response": uuid }))
            })
            .await;
        // GET /instance — running instances (one list = one simulation tick).
        server
            .route("GET", r"^/api/v1/instance$", move |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let listed: Vec<Value> = account.instances.iter().map(instance_json).collect();
                account.instances.retain(|i| i.status != "terminating");
                for instance in account.instances.iter_mut() {
                    if instance.boot_lists != u32::MAX && instance.boot_lists > 0 {
                        instance.boot_lists -= 1;
                    }
                }
                json(200, json!({ "runningInstances": listed }))
            })
            .await;
        // POST /instance/terminate — terminate instances by uuid.
        server
            .route(
                "POST",
                r"^/api/v1/instance/terminate$",
                move |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let uuids: Vec<String> = body(req)["instanceUuids"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut terminated = Vec::new();
                    for instance in account.instances.iter_mut() {
                        if uuids.contains(&instance.uuid) {
                            instance.status = "terminating";
                            terminated.push(json!({
                                "id": instance.uuid,
                                "name": instance.name,
                                "ip": "198.51.100.20",
                                "status": "terminated",
                                "ssh_key_names": [],
                                "file_system_names": [],
                                "region": {"name": "us-central-3"},
                                "instance_type": {"name": instance.product},
                                "jupyter_token": "",
                                "jupyter_url": "",
                            }));
                        }
                    }
                    if terminated.is_empty() {
                        return json(404, json!({"message": "Instance not found"}));
                    }
                    json(
                        202,
                        json!({"response": {"data": {"terminated_instances": terminated}}}),
                    )
                },
            )
            .await;
        MassedComputeStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            massedcompute: ApiKeyConfig {
                api_key: Some(TOKEN.into()),
                base_url: Some(format!("{}/api/v1", self.server.uri())),
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
            .instances
            .iter()
            .filter(|i| i.status != "terminating")
            .map(|i| format!("instance {} ({})", i.uuid, i.name))
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        self.server.state().instances.clear();
    }

    fn expected_login(&self) -> (String, u16) {
        (LOGIN_USER.into(), 22)
    }
}

vendor_suite!(MassedComputeStub);
