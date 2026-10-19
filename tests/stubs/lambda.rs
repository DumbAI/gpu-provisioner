//! Stateful stub of the **Lambda Cloud API** (`https://cloud.lambda.ai/api/v1`),
//! limited to the endpoints gpuprov's Lambda adapter uses.
//!
//! Vendor reference: <https://docs.lambda.ai/api/cloud> (OpenAPI 3.1:
//! <https://cloud.lambda.ai/api/v1/openapi.json>). Response shapes follow
//! that spec — `{"data": …}` envelopes on success, `{"error": {code, message,
//! suggestion}}` on failure — not gpuprov's parser, so the stub independently
//! checks the adapter's reading of the API. Every request must carry
//! `Authorization: Bearer <key>` (else `401 global/invalid-api-key`).
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /api/v1/instance-types` | [List available instance types](https://docs.lambda.ai/api/cloud#listInstanceTypes) | `gpu_1x_a6000` with capacity in `us-east-1` and `us-west-1`, plus a non-matching `gpu_1x_a10` |
//! | `GET /api/v1/ssh-keys` | [List your SSH keys](https://docs.lambda.ai/api/cloud#listSSHKeys) | the account's keys |
//! | `POST /api/v1/ssh-keys` | [Add an SSH key](https://docs.lambda.ai/api/cloud#addSSHKey) | registers `{name, public_key}` |
//! | `GET /api/v1/firewall-rulesets` | [List firewall rulesets](https://docs.lambda.ai/api/cloud#listFirewallRulesets) | per-region rulesets |
//! | `POST /api/v1/firewall-rulesets` | [Create firewall ruleset](https://docs.lambda.ai/api/cloud#createFirewallRuleset) | `{name, region, rules}` → ruleset |
//! | `PATCH /api/v1/firewall-rulesets/{id}` | [Update firewall ruleset](https://docs.lambda.ai/api/cloud#updateFirewallRuleset) | replaces `rules` |
//! | `DELETE /api/v1/firewall-rulesets/{id}` | [Delete firewall ruleset](https://docs.lambda.ai/api/cloud#deleteFirewallRuleset) | `409 firewall-rulesets/firewall-ruleset-in-use` while a live instance uses it |
//! | `POST /api/v1/instance-operations/launch` | [Launch instances](https://docs.lambda.ai/api/cloud#launchInstance) | `{data: {instance_ids}}`; capacity fault → `400 instance-operations/launch/insufficient-capacity` |
//! | `GET /api/v1/instances` | [List running instances](https://docs.lambda.ai/api/cloud#listInstances) | `page_token` pagination (honors `page_size`) |
//! | `POST /api/v1/instance-operations/terminate` | [Terminate instances](https://docs.lambda.ai/api/cloud#terminateInstance) | `{data: {terminated_instances}}`; unknown/terminated ids → `404 global/object-does-not-exist` |
//!
//! Instances start `booting`, become `active` with a public `ip` after one
//! list, and after termination are `terminating` for one list, `terminated`
//! for the next, then gone. Login is `ubuntu`@22 (created by cloud-init).

use async_trait::async_trait;
use gpu_provisioner::config::{ApiKeyConfig, AppConfig};
use serde_json::{json, Value};
use wiremock::{Request, ResponseTemplate};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, header, json, last_segment, query, Ids, StubServer};

const API_KEY: &str = "lambda-test-key";

#[derive(Debug)]
struct Instance {
    id: String,
    name: String,
    region: String,
    instance_type: String,
    tags: Vec<(String, String)>,
    rulesets: Vec<String>,
    status: &'static str,
    /// Lists left before `booting` becomes `active` (`u32::MAX` = held).
    boot_lists: u32,
    ip: String,
}

#[derive(Debug)]
struct Ruleset {
    id: String,
    name: String,
    region: String,
    rules: Value,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    instances: Vec<Instance>,
    rulesets: Vec<Ruleset>,
    ssh_keys: Vec<(String, String, String)>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    /// Boot payloads (user data / startup scripts) attached at create.
    boot_payloads: Vec<String>,
}

/// Simulated Lambda account. See the module docs for the endpoint map.
pub struct LambdaStub {
    server: StubServer<Account>,
}

/// `{"error": {code, message, suggestion}}` (spec: `ApiError*` schemas).
fn error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    json(
        status,
        json!({"error": {"code": code, "message": message,
                         "suggestion": "See the API documentation."}}),
    )
}

fn unauthorized(req: &Request) -> Option<ResponseTemplate> {
    let ok = header(req, "authorization").as_deref() == Some(&format!("Bearer {API_KEY}"));
    (!ok).then(|| {
        error(
            401,
            "global/invalid-api-key",
            "API key was invalid, expired, or deleted.",
        )
    })
}

fn region_json(name: &str) -> Value {
    json!({"name": name, "description": format!("Region {name}")})
}

/// One `InstanceTypesItem`.
fn instance_type_item(name: &str, gpu: &str, cents: u64, regions: &[&str]) -> Value {
    json!({
        "instance_type": {
            "name": name,
            "description": format!("1x {gpu}"),
            "gpu_description": gpu,
            "price_cents_per_hour": cents,
            "specs": {"vcpus": 14, "memory_gib": 46, "storage_gib": 512, "gpus": 1},
            "architecture": "x86_64",
        },
        "regions_with_capacity_available": regions.iter().map(|r| region_json(r)).collect::<Vec<_>>(),
    })
}

fn ruleset_json(r: &Ruleset) -> Value {
    json!({
        "id": r.id,
        "name": r.name,
        "region": region_json(&r.region),
        "rules": r.rules,
        "is_global": false,
        "instance_ids": [],
        "created": "2026-10-19T00:00:00Z",
    })
}

fn instance_json(i: &Instance) -> Value {
    let mut v = json!({
        "id": i.id,
        "name": i.name,
        "status": i.status,
        "ssh_key_names": ["gpuprov"],
        "file_system_names": [],
        "region": region_json(&i.region),
        "instance_type": {
            "name": i.instance_type,
            "description": "1x RTX A6000 (48 GB)",
            "gpu_description": "RTX A6000 (48 GB)",
            "price_cents_per_hour": 80,
            "specs": {"vcpus": 14, "memory_gib": 46, "storage_gib": 512, "gpus": 1},
            "architecture": "x86_64",
        },
        "hostname": i.name,
        "first_healthy": null,
        "actions": {},
        "tags": i.tags.iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>(),
    });
    if i.status == "active" {
        v["ip"] = json!(i.ip);
        v["private_ip"] = json!("10.0.0.5");
    }
    v
}

#[async_trait]
impl VendorStub for LambdaStub {
    const CLOUD: &'static str = "lambda";

    async fn start() -> Self {
        let server = StubServer::start(Account::default()).await;

        // GET /api/v1/instance-types — List available instance types.
        server
            .route("GET", r"^/api/v1/instance-types$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                json(
                    200,
                    json!({"data": {
                        "gpu_1x_a6000": instance_type_item(
                            "gpu_1x_a6000", "RTX A6000 (48 GB)", 80, &["us-east-1", "us-west-1"]),
                        "gpu_1x_a10": instance_type_item(
                            "gpu_1x_a10", "A10 (24 GB PCIe)", 129, &["us-east-1"]),
                    }}),
                )
            })
            .await;
        // GET /api/v1/ssh-keys — List your SSH keys.
        server
            .route("GET", r"^/api/v1/ssh-keys$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let keys: Vec<Value> = account
                    .ssh_keys
                    .iter()
                    .map(|(id, name, key)| json!({"id": id, "name": name, "public_key": key}))
                    .collect();
                json(200, json!({"data": keys}))
            })
            .await;
        // POST /api/v1/ssh-keys — Add an SSH key.
        server
            .route("POST", r"^/api/v1/ssh-keys$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let name = spec["name"].as_str().unwrap_or_default().to_string();
                let key = spec["public_key"].as_str().unwrap_or_default().to_string();
                if name.is_empty() || key.is_empty() {
                    return error(400, "global/invalid-parameters", "Invalid request data.");
                }
                let id = account.ids.next("key");
                account
                    .ssh_keys
                    .push((id.clone(), name.clone(), key.clone()));
                json(
                    200,
                    json!({"data": {"id": id, "name": name, "public_key": key}}),
                )
            })
            .await;
        // GET /api/v1/firewall-rulesets — List firewall rulesets.
        server
            .route("GET", r"^/api/v1/firewall-rulesets$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let data: Vec<Value> = account.rulesets.iter().map(ruleset_json).collect();
                json(200, json!({"data": data}))
            })
            .await;
        // POST /api/v1/firewall-rulesets — Create firewall ruleset.
        server
            .route("POST", r"^/api/v1/firewall-rulesets$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let ruleset = Ruleset {
                    id: account.ids.next("fwrs"),
                    name: spec["name"].as_str().unwrap_or_default().to_string(),
                    region: spec["region"].as_str().unwrap_or_default().to_string(),
                    rules: spec["rules"].clone(),
                };
                let created = ruleset_json(&ruleset);
                account.rulesets.push(ruleset);
                json(200, json!({"data": created}))
            })
            .await;
        // PATCH /api/v1/firewall-rulesets/{id} — Update firewall ruleset.
        server
            .route(
                "PATCH",
                r"^/api/v1/firewall-rulesets/[^/]+$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let id = last_segment(req);
                    let spec = body(req);
                    match account.rulesets.iter_mut().find(|r| r.id == id) {
                        Some(r) => {
                            if !spec["rules"].is_null() {
                                r.rules = spec["rules"].clone();
                            }
                            json(200, json!({"data": ruleset_json(r)}))
                        }
                        None => error(
                            404,
                            "global/object-does-not-exist",
                            "Specified firewall ruleset does not exist.",
                        ),
                    }
                },
            )
            .await;
        // DELETE /api/v1/firewall-rulesets/{id} — Delete firewall ruleset.
        server
            .route(
                "DELETE",
                r"^/api/v1/firewall-rulesets/[^/]+$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let id = last_segment(req);
                    let in_use = account
                        .instances
                        .iter()
                        .any(|i| i.status != "terminated" && i.rulesets.contains(&id));
                    if in_use {
                        return error(
                            409,
                            "firewall-rulesets/firewall-ruleset-in-use",
                            "Firewall ruleset is in use by one or more instances.",
                        );
                    }
                    let before = account.rulesets.len();
                    account.rulesets.retain(|r| r.id != id);
                    if account.rulesets.len() == before {
                        return error(
                            404,
                            "global/object-does-not-exist",
                            "Specified firewall ruleset does not exist.",
                        );
                    }
                    json(200, json!({"data": {}}))
                },
            )
            .await;
        // POST /api/v1/instance-operations/launch — Launch instances.
        server
            .route(
                "POST",
                r"^/api/v1/instance-operations/launch$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    if account.capacity_faults > 0 {
                        account.capacity_faults -= 1;
                        return error(
                            400,
                            "instance-operations/launch/insufficient-capacity",
                            "Not enough capacity to fulfill launch request.",
                        );
                    }
                    let spec = body(req);
                    let region = spec["region_name"].as_str().unwrap_or_default().to_string();
                    let ssh_keys_ok = spec["ssh_key_names"]
                        .as_array()
                        .is_some_and(|names| names.len() == 1);
                    if region.is_empty() || !ssh_keys_ok {
                        return error(400, "global/invalid-parameters", "Invalid request data.");
                    }
                    let id = account.ids.next("lambda-instance");
                    let n = account.creates + 10;
                    let tags = spec["tags"]
                        .as_array()
                        .map(|tags| {
                            tags.iter()
                                .map(|t| {
                                    (
                                        t["key"].as_str().unwrap_or_default().to_string(),
                                        t["value"].as_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let rulesets = spec["firewall_rulesets"]
                        .as_array()
                        .map(|r| {
                            r.iter()
                                .filter_map(|x| x["id"].as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    account.instances.push(Instance {
                        id: id.clone(),
                        name: spec["name"].as_str().unwrap_or_default().to_string(),
                        region,
                        instance_type: spec["instance_type_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        tags,
                        rulesets,
                        status: "booting",
                        boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                        ip: format!("198.51.100.{n}"),
                    });
                    if let Some(user_data) = spec["user_data"].as_str() {
                        account.boot_payloads.push(user_data.to_string());
                    }
                    account.creates += 1;
                    json(200, json!({"data": {"instance_ids": [id]}}))
                },
            )
            .await;
        // GET /api/v1/instances — List running instances (page_token paging).
        server
            .route("GET", r"^/api/v1/instances$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                // Advance the simulation: one first-page list = one tick.
                if query(req, "page_token").is_none() {
                    account.instances.retain(|i| i.status != "terminated");
                    for i in account.instances.iter_mut() {
                        match i.status {
                            "booting" if i.boot_lists == u32::MAX => {}
                            "booting" if i.boot_lists == 0 => i.status = "active",
                            "booting" => i.boot_lists -= 1,
                            "terminating" => i.status = "terminated",
                            _ => {}
                        }
                    }
                }
                let page_size: usize = query(req, "page_size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100)
                    .max(1);
                let start: usize = query(req, "page_token")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let page: Vec<Value> = account
                    .instances
                    .iter()
                    .skip(start)
                    .take(page_size)
                    .map(instance_json)
                    .collect();
                let next = start + page_size;
                let token = if next < account.instances.len() {
                    json!(next.to_string())
                } else {
                    Value::Null
                };
                json(200, json!({"data": page, "page_token": token}))
            })
            .await;
        // POST /api/v1/instance-operations/terminate — Terminate instances.
        server
            .route(
                "POST",
                r"^/api/v1/instance-operations/terminate$",
                |req, account| {
                    if let Some(denied) = unauthorized(req) {
                        return denied;
                    }
                    let ids: Vec<String> = body(req)["instance_ids"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let all_live = ids.iter().all(|id| {
                        account.instances.iter().any(|i| {
                            &i.id == id && !matches!(i.status, "terminating" | "terminated")
                        })
                    });
                    if ids.is_empty() || !all_live {
                        return error(
                            404,
                            "global/object-does-not-exist",
                            "Specified instance does not exist.",
                        );
                    }
                    let mut terminated = Vec::new();
                    for i in account.instances.iter_mut().filter(|i| ids.contains(&i.id)) {
                        i.status = "terminating";
                        terminated.push(instance_json(i));
                    }
                    json(200, json!({"data": {"terminated_instances": terminated}}))
                },
            )
            .await;
        LambdaStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            lambda: ApiKeyConfig {
                api_key: Some(API_KEY.into()),
                base_url: Some(self.server.uri()),
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
        let account = self.server.state();
        account
            .instances
            .iter()
            .filter(|i| i.status != "terminated")
            .map(|i| format!("instance {} ({})", i.id, i.status))
            .chain(
                account
                    .rulesets
                    .iter()
                    .map(|r| format!("firewall ruleset {} ({})", r.id, r.name)),
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

vendor_suite!(LambdaStub);

/// Lambda-specific: opening a port creates a per-VM firewall ruleset at
/// prepare, attaches it at launch, and teardown removes it (after the
/// instance is gone, since an in-use ruleset cannot be deleted).
#[tokio::test]
async fn open_port_ruleset_is_attached_and_removed() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = LambdaStub::start().await;
    let mut run = Run::new(&stub, "stub-ports").await;
    run.spec.ports = vec![8080];
    let end = run.launch(Cancel::new()).await;
    assert!(
        matches!(end, LaunchState::Ready(_)),
        "{end:?}\n{}",
        run.journal()
    );
    {
        let account = stub.server.state();
        assert_eq!(account.rulesets.len(), 1, "one per-VM ruleset");
        let ruleset = &account.rulesets[0];
        assert_eq!(ruleset.name, "gpuprov-stub-ports");
        let ports: Vec<Value> = ruleset
            .rules
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["port_range"].clone())
            .collect();
        assert!(
            ports.contains(&json!([22, 22])) && ports.contains(&json!([8080, 8080])),
            "{ports:?}"
        );
        assert_eq!(
            account.instances[0].rulesets,
            vec![ruleset.id.clone()],
            "attached at launch"
        );
    }
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert!(stub.leftovers().is_empty(), "{:?}", stub.leftovers());
}
