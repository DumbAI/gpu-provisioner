//! Stateful stub of the **DigitalOcean API v2** (`https://api.digitalocean.com/v2`),
//! limited to the endpoints gpuprov's DigitalOcean adapter uses.
//!
//! Vendor reference: <https://docs.digitalocean.com/reference/api/digitalocean/>
//! (OpenAPI: <https://api-engineering.nyc3.cdn.digitaloceanspaces.com/spec-ci/DigitalOcean-public.v2.yaml>).
//! Response shapes follow that spec — `{"<resource>": …}` envelopes,
//! `links.pages` / `meta.total` on lists, `{"id", "message", "request_id"}`
//! on errors — not gpuprov's parser, so the stub independently checks the
//! adapter's reading of the API. Every request must carry
//! `Authorization: Bearer <token>` (else `401 unauthorized`).
//!
//! | endpoint | vendor docs | simulated behavior |
//! |---|---|---|
//! | `GET /v2/sizes` | [List All Droplet Sizes](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Sizes/operation/sizes_list) | `gpu-h100x1-80gb` in `tor1` and `nyc2`, a non-matching `gpu-l40sx1-48gb`, and a CPU size |
//! | `GET /v2/account/keys` | [List All SSH Keys](https://docs.digitalocean.com/reference/api/digitalocean/#tag/SSH-Keys/operation/sshKeys_list) | the account's keys |
//! | `POST /v2/account/keys` | [Create a New SSH Key](https://docs.digitalocean.com/reference/api/digitalocean/#tag/SSH-Keys/operation/sshKeys_create) | `201 {ssh_key}`; a key already on the account → `422` |
//! | `GET /v2/tags/{tag_id}` | [Retrieve a Tag](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Tags/operation/tags_get) | `404 not_found` when absent |
//! | `POST /v2/tags` | [Create a New Tag](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Tags/operation/tags_create) | `201 {tag}` |
//! | `DELETE /v2/tags/{tag_id}` | [Delete a Tag](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Tags/operation/tags_delete) | `204`; unknown → `404` |
//! | `GET /v2/firewalls` | [List All Firewalls](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Firewalls/operation/firewalls_list) | the account's firewalls |
//! | `POST /v2/firewalls` | [Create a New Firewall](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Firewalls/operation/firewalls_create) | `202 {firewall}`; a tag that does not exist → `422` |
//! | `PUT /v2/firewalls/{firewall_id}` | [Update a Firewall](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Firewalls/operation/firewalls_update) | replaces rules and tags |
//! | `DELETE /v2/firewalls/{firewall_id}` | [Delete a Firewall](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Firewalls/operation/firewalls_delete) | `204`; unknown → `404` |
//! | `POST /v2/droplets` | [Create a New Droplet](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Droplets/operation/droplets_create) | `202 {droplet, links}`, status `new`; creates its tags; capacity fault → `422 unprocessable_entity` "Size is not available in this region." |
//! | `GET /v2/droplets?tag_name=` | [List All Droplets](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Droplets/operation/droplets_list) | filtered by tag |
//! | `DELETE /v2/droplets/{droplet_id}` | [Delete an Existing Droplet](https://docs.digitalocean.com/reference/api/digitalocean/#tag/Droplets/operation/droplets_destroy) | `204`; unknown → `404 not_found` |
//!
//! Every list is paginated by `page` / `per_page` with absolute
//! `links.pages.next` URLs; the stub serves **one item per page** so the
//! adapter's paging is exercised on every call.
//!
//! Droplets start `new`, become `active` with public and private `networks.v4`
//! after one list, and after deletion stay listed (`locked`) for one more
//! list, then are gone. Login is `root`@22: DigitalOcean installs the
//! create's `ssh_keys` for root.
//!
//! Documented assumptions (not in the spec): the capacity error's status and
//! wording (no capacity response is documented for `POST /v2/droplets`), and
//! that a firewall can only reference an existing tag.

use async_trait::async_trait;
use gpu_provisioner::config::{ApiKeyConfig, AppConfig};
use serde_json::{json, Value};
use wiremock::{Request, ResponseTemplate};

use super::harness::{vendor_suite, Ask, VendorStub};
use super::route::{body, empty, header, json, last_segment, query, Ids, StubServer};

const TOKEN: &str = "dop_v1_stub";

#[derive(Debug)]
struct Droplet {
    id: u64,
    name: String,
    region: String,
    size: String,
    tags: Vec<String>,
    status: &'static str,
    /// Lists left before `new` becomes `active` (`u32::MAX` = held).
    boot_lists: u32,
    /// Deleted: listed (locked) once more, then gone.
    destroyed: bool,
    ip: String,
}

#[derive(Debug)]
struct Firewall {
    id: String,
    name: String,
    tags: Vec<String>,
    inbound_rules: Value,
    outbound_rules: Value,
}

#[derive(Debug, Default)]
pub struct Account {
    ids: Ids,
    next_droplet_id: u64,
    droplets: Vec<Droplet>,
    firewalls: Vec<Firewall>,
    tags: Vec<String>,
    /// `(id, name, public_key)`.
    ssh_keys: Vec<(u64, String, String)>,
    capacity_faults: u32,
    hold_boot: bool,
    creates: usize,
    /// Cloud-init `user_data` attached at create.
    boot_payloads: Vec<String>,
}

/// Simulated DigitalOcean account. See the module docs for the endpoint map.
pub struct DigitalOceanStub {
    server: StubServer<Account>,
}

/// `{"id", "message", "request_id"}` (spec: `components/schemas/error`).
fn error(status: u16, id: &str, message: &str) -> ResponseTemplate {
    json(
        status,
        json!({"id": id, "message": message,
               "request_id": "4851a473-1621-42ea-b2f9-5071c0ea8414"}),
    )
}

fn not_found() -> ResponseTemplate {
    error(
        404,
        "not_found",
        "The resource you were accessing could not be found.",
    )
}

fn unauthorized(req: &Request) -> Option<ResponseTemplate> {
    let ok = header(req, "authorization").as_deref() == Some(&format!("Bearer {TOKEN}"));
    (!ok).then(|| error(401, "unauthorized", "Unable to authenticate you"))
}

/// One page of `items` under `key`, by `page` / `per_page` (capped at one
/// item so paging always runs), with absolute `links.pages` URLs.
fn page(req: &Request, key: &str, items: Vec<Value>) -> ResponseTemplate {
    // The adapter's `per_page` is ignored on purpose (see the doc above).
    let per_page = 1;
    let number = query(req, "page")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let total = items.len();
    let last = total.div_ceil(per_page).max(1);
    let url = |n: usize| {
        let mut u = req.url.clone();
        let pairs: Vec<(String, String)> = req
            .url
            .query_pairs()
            .filter(|(k, _)| k != "page")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        u.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("page", &n.to_string());
        u.to_string()
    };
    let mut pages = json!({});
    if number < last {
        pages["next"] = json!(url(number + 1));
        pages["last"] = json!(url(last));
    }
    if number > 1 {
        pages["first"] = json!(url(1));
        pages["prev"] = json!(url(number - 1));
    }
    let chunk: Vec<Value> = items
        .into_iter()
        .skip((number - 1) * per_page)
        .take(per_page)
        .collect();
    let mut out = json!({"links": {"pages": pages}, "meta": {"total": total}});
    out[key] = Value::Array(chunk);
    json(200, out)
}

/// One `size` object.
fn size_json(slug: &str, model: Option<&str>, hourly: f64, regions: &[&str]) -> Value {
    let mut v = json!({
        "slug": slug, "memory": 245760, "vcpus": 20, "disk": 720, "transfer": 15.0,
        "price_monthly": (hourly * 672.0).round(), "price_hourly": hourly,
        "regions": regions, "available": true,
        "description": if model.is_some() { "GPU Droplet" } else { "Basic" },
        "disk_info": [{"type": "local", "size": {"amount": 720, "unit": "gib"}}],
    });
    if let Some(model) = model {
        v["gpu_info"] = json!({"count": 1, "model": model, "vram": {"amount": 80, "unit": "gib"}});
    }
    v
}

fn droplet_json(d: &Droplet) -> Value {
    let v4 = if d.status == "new" {
        json!([])
    } else {
        json!([
            {"ip_address": "10.118.0.2", "netmask": "255.255.240.0",
             "gateway": "10.118.0.1", "type": "private"},
            {"ip_address": d.ip, "netmask": "255.255.240.0",
             "gateway": "203.0.113.1", "type": "public"},
        ])
    };
    json!({
        "id": d.id,
        "name": d.name,
        "memory": 245760,
        "vcpus": 20,
        "disk": 720,
        "locked": d.destroyed,
        "status": d.status,
        "created_at": "2026-10-19T00:00:00Z",
        "features": ["droplet_agent", "private_networking"],
        "backup_ids": [],
        "snapshot_ids": [],
        "image": {"slug": "gpu-h100x1-base", "distribution": "Ubuntu"},
        "volume_ids": [],
        "size": {"slug": d.size},
        "size_slug": d.size,
        "networks": {"v4": v4, "v6": []},
        "region": {"slug": d.region, "name": d.region, "available": true},
        "tags": d.tags,
        "vpc_uuid": "760e09ef-dc84-11e8-981e-3cfdfeaae000",
    })
}

fn firewall_json(f: &Firewall) -> Value {
    json!({
        "id": f.id,
        "name": f.name,
        "status": "succeeded",
        "inbound_rules": f.inbound_rules,
        "outbound_rules": f.outbound_rules,
        "created_at": "2026-10-19T00:00:00Z",
        "droplet_ids": [],
        "tags": f.tags,
        "pending_changes": [],
    })
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Validate a firewall body: its tags must exist (assumed, see module
/// docs). The error is the `422` message.
fn firewall_from(spec: &Value, account: &Account) -> Result<(String, Vec<String>), String> {
    let name = spec["name"].as_str().unwrap_or_default().to_string();
    let tags = strings(&spec["tags"]);
    if name.is_empty() {
        return Err("name is required".into());
    }
    match tags.iter().find(|t| !account.tags.contains(t)) {
        Some(missing) => Err(format!("tag {missing} not found")),
        None => Ok((name, tags)),
    }
}

impl DigitalOceanStub {
    /// Give every Droplet a DigitalOcean status (e.g. `off`).
    pub fn set_status(&self, status: &'static str) {
        for droplet in self.server.state().droplets.iter_mut() {
            droplet.status = status;
        }
    }
}

#[async_trait]
impl VendorStub for DigitalOceanStub {
    const CLOUD: &'static str = "digitalocean";

    async fn start() -> Self {
        let server = StubServer::start(Account {
            next_droplet_id: 3164444,
            ..Account::default()
        })
        .await;

        // GET /v2/sizes — List All Droplet Sizes.
        server
            .route("GET", r"^/v2/sizes$", |req, _| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                page(
                    req,
                    "sizes",
                    vec![
                        size_json("s-2vcpu-4gb", None, 0.03571, &["nyc1", "tor1"]),
                        size_json("gpu-l40sx1-48gb", Some("nvidia_l40s"), 1.57, &["tor1"]),
                        size_json(
                            "gpu-h100x1-80gb",
                            Some("nvidia_h100"),
                            3.39,
                            &["nyc2", "tor1", "ams3"],
                        ),
                    ],
                )
            })
            .await;
        // GET /v2/account/keys — List All SSH Keys.
        server
            .route("GET", r"^/v2/account/keys$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let keys = account
                    .ssh_keys
                    .iter()
                    .map(|(id, name, key)| {
                        json!({"id": id, "fingerprint": format!("fp:{id}"),
                               "public_key": key, "name": name})
                    })
                    .collect();
                page(req, "ssh_keys", keys)
            })
            .await;
        // POST /v2/account/keys — Create a New SSH Key.
        server
            .route("POST", r"^/v2/account/keys$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let name = spec["name"].as_str().unwrap_or_default().to_string();
                let key = spec["public_key"].as_str().unwrap_or_default().to_string();
                if name.is_empty() || key.is_empty() {
                    return error(422, "unprocessable_entity", "Key invalid");
                }
                if account.ssh_keys.iter().any(|(_, _, k)| *k == key) {
                    return error(
                        422,
                        "unprocessable_entity",
                        "SSH Key is already in use on your account",
                    );
                }
                let id = 512189 + account.ssh_keys.len() as u64;
                account.ssh_keys.push((id, name.clone(), key.clone()));
                json(
                    201,
                    json!({"ssh_key": {"id": id, "fingerprint": format!("fp:{id}"),
                                       "public_key": key, "name": name}}),
                )
            })
            .await;
        // GET /v2/tags/{tag_id} — Retrieve a Tag.
        server
            .route("GET", r"^/v2/tags/[^/]+$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let name = last_segment(req);
                if !account.tags.contains(&name) {
                    return not_found();
                }
                json(
                    200,
                    json!({"tag": {"name": name, "resources": {"count": 0}}}),
                )
            })
            .await;
        // POST /v2/tags — Create a New Tag.
        server
            .route("POST", r"^/v2/tags$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let name = body(req)["name"].as_str().unwrap_or_default().to_string();
                if name.is_empty() {
                    return error(400, "bad_request", "name is required");
                }
                if !account.tags.contains(&name) {
                    account.tags.push(name.clone());
                }
                json(
                    201,
                    json!({"tag": {"name": name, "resources": {"count": 0}}}),
                )
            })
            .await;
        // DELETE /v2/tags/{tag_id} — Delete a Tag (untags its resources).
        server
            .route("DELETE", r"^/v2/tags/[^/]+$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let name = last_segment(req);
                if !account.tags.contains(&name) {
                    return not_found();
                }
                account.tags.retain(|t| *t != name);
                for d in account.droplets.iter_mut() {
                    d.tags.retain(|t| *t != name);
                }
                for f in account.firewalls.iter_mut() {
                    f.tags.retain(|t| *t != name);
                }
                empty(204)
            })
            .await;
        // GET /v2/firewalls — List All Firewalls.
        server
            .route("GET", r"^/v2/firewalls$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let firewalls = account.firewalls.iter().map(firewall_json).collect();
                page(req, "firewalls", firewalls)
            })
            .await;
        // POST /v2/firewalls — Create a New Firewall.
        server
            .route("POST", r"^/v2/firewalls$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let spec = body(req);
                let (name, tags) = match firewall_from(&spec, account) {
                    Ok(parts) => parts,
                    Err(message) => return error(422, "unprocessable_entity", &message),
                };
                let firewall = Firewall {
                    id: account.ids.next("fw"),
                    name,
                    tags,
                    inbound_rules: spec["inbound_rules"].clone(),
                    outbound_rules: spec["outbound_rules"].clone(),
                };
                let mut created = firewall_json(&firewall);
                created["status"] = json!("waiting");
                account.firewalls.push(firewall);
                json(202, json!({"firewall": created}))
            })
            .await;
        // PUT /v2/firewalls/{firewall_id} — Update a Firewall.
        server
            .route("PUT", r"^/v2/firewalls/[^/]+$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let id = last_segment(req);
                let spec = body(req);
                let (name, tags) = match firewall_from(&spec, account) {
                    Ok(parts) => parts,
                    Err(message) => return error(422, "unprocessable_entity", &message),
                };
                match account.firewalls.iter_mut().find(|f| f.id == id) {
                    Some(f) => {
                        f.name = name;
                        f.tags = tags;
                        f.inbound_rules = spec["inbound_rules"].clone();
                        f.outbound_rules = spec["outbound_rules"].clone();
                        json(200, json!({"firewall": firewall_json(f)}))
                    }
                    None => not_found(),
                }
            })
            .await;
        // DELETE /v2/firewalls/{firewall_id} — Delete a Firewall.
        server
            .route("DELETE", r"^/v2/firewalls/[^/]+$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let id = last_segment(req);
                let before = account.firewalls.len();
                account.firewalls.retain(|f| f.id != id);
                if account.firewalls.len() == before {
                    return not_found();
                }
                empty(204)
            })
            .await;
        // POST /v2/droplets — Create a New Droplet.
        server
            .route("POST", r"^/v2/droplets$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                if account.capacity_faults > 0 {
                    account.capacity_faults -= 1;
                    return error(
                        422,
                        "unprocessable_entity",
                        "Size is not available in this region.",
                    );
                }
                let spec = body(req);
                let field = |k: &str| spec[k].as_str().unwrap_or_default().to_string();
                let keys_ok = spec["ssh_keys"].as_array().is_some_and(|k| {
                    k.len() == 1
                        && k[0]
                            .as_u64()
                            .is_some_and(|id| account.ssh_keys.iter().any(|(i, _, _)| *i == id))
                });
                if field("name").is_empty()
                    || field("region").is_empty()
                    || field("size").is_empty()
                    || spec["image"].is_null()
                    || !keys_ok
                {
                    return error(422, "unprocessable_entity", "Droplet create is invalid");
                }
                let tags = strings(&spec["tags"]);
                for tag in &tags {
                    if !account.tags.contains(tag) {
                        account.tags.push(tag.clone());
                    }
                }
                let id = account.next_droplet_id;
                account.next_droplet_id += 1;
                let n = account.creates + 10;
                let droplet = Droplet {
                    id,
                    name: field("name"),
                    region: field("region"),
                    size: field("size"),
                    tags,
                    status: "new",
                    boot_lists: if account.hold_boot { u32::MAX } else { 1 },
                    destroyed: false,
                    ip: format!("198.51.100.{n}"),
                };
                let created = droplet_json(&droplet);
                account.droplets.push(droplet);
                if let Some(user_data) = spec["user_data"].as_str() {
                    account.boot_payloads.push(user_data.to_string());
                }
                account.creates += 1;
                json(
                    202,
                    json!({"droplet": created, "links": {"actions": [
                        {"id": 7515, "rel": "create",
                         "href": "https://api.digitalocean.com/v2/actions/7515"}
                    ]}}),
                )
            })
            .await;
        // GET /v2/droplets — List All Droplets (optionally by tag_name).
        server
            .route("GET", r"^/v2/droplets$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                // Advance the simulation: one first-page list = one tick.
                if query(req, "page").is_none_or(|p| p == "1") {
                    account.droplets.retain(|d| !d.destroyed);
                    for d in account.droplets.iter_mut() {
                        match d.status {
                            "new" if d.boot_lists == u32::MAX => {}
                            "new" if d.boot_lists == 0 => d.status = "active",
                            "new" => d.boot_lists -= 1,
                            _ => {}
                        }
                    }
                }
                let tag = query(req, "tag_name");
                let droplets = account
                    .droplets
                    .iter()
                    .filter(|d| tag.as_ref().is_none_or(|t| d.tags.contains(t)))
                    .map(droplet_json)
                    .collect();
                page(req, "droplets", droplets)
            })
            .await;
        // DELETE /v2/droplets/{droplet_id} — Delete an Existing Droplet.
        server
            .route("DELETE", r"^/v2/droplets/[^/]+$", |req, account| {
                if let Some(denied) = unauthorized(req) {
                    return denied;
                }
                let id = last_segment(req);
                match account.droplets.iter_mut().find(|d| d.id.to_string() == id) {
                    Some(d) => {
                        d.destroyed = true;
                        empty(204)
                    }
                    None => not_found(),
                }
            })
            .await;
        DigitalOceanStub { server }
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            digitalocean: ApiKeyConfig {
                api_key: Some(TOKEN.into()),
                base_url: Some(self.server.uri()),
                ..ApiKeyConfig::default()
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
        let account = self.server.state();
        account
            .droplets
            .iter()
            .map(|d| format!("droplet {} ({})", d.id, d.status))
            .chain(
                account
                    .firewalls
                    .iter()
                    .map(|f| format!("firewall {} ({})", f.id, f.name)),
            )
            .chain(
                account
                    .tags
                    .iter()
                    .filter(|t| t.starts_with("gpuprov-vm-"))
                    .map(|t| format!("tag {t}")),
            )
            .collect()
    }

    fn creates(&self) -> usize {
        self.server.state().creates
    }

    fn reclaim(&self) {
        self.server.state().droplets.clear();
    }

    fn boot_payloads(&self) -> Vec<String> {
        self.server.state().boot_payloads.clone()
    }

    fn expected_login(&self) -> (String, u16) {
        ("root".into(), 22)
    }
}

vendor_suite!(DigitalOceanStub);

/// DigitalOcean-specific: every VM gets a cloud firewall bound to its tag
/// (SSH plus requested ports, all outbound), and teardown removes both.
#[tokio::test]
async fn cloud_firewall_is_bound_by_tag_and_removed() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = DigitalOceanStub::start().await;
    let mut run = Run::new(&stub, "stub-fw").await;
    run.spec.ports = vec![8080];
    let end = run.launch(Cancel::new()).await;
    assert!(
        matches!(end, LaunchState::Ready(_)),
        "{end:?}\n{}",
        run.journal()
    );
    {
        let account = stub.server.state();
        assert_eq!(account.firewalls.len(), 1, "one per-VM firewall");
        let firewall = &account.firewalls[0];
        assert_eq!(firewall.name, "gpuprov-stub-fw");
        assert_eq!(firewall.tags, ["gpuprov-vm-stub-fw"]);
        let ports: Vec<&str> = firewall
            .inbound_rules
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["ports"].as_str())
            .collect();
        assert!(
            ports.contains(&"22") && ports.contains(&"8080"),
            "{ports:?}"
        );
        assert!(
            !firewall.outbound_rules.as_array().unwrap().is_empty(),
            "outbound traffic must stay open"
        );
        assert!(
            account.droplets[0].tags.contains(&firewall.tags[0]),
            "the Droplet carries the firewall's tag"
        );
    }
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert!(stub.leftovers().is_empty(), "{:?}", stub.leftovers());
}

/// `gpuprov health` flags a powered-off Droplet: unhealthy, and still
/// billing.
#[tokio::test]
async fn health_flags_powered_off_droplets() {
    use super::harness::Run;
    use gpu_provisioner::machine::Cancel;
    use gpu_provisioner::vm::health::{self, HealthOptions, Source, Verdict};
    use gpu_provisioner::vm::launch::LaunchState;
    use gpu_provisioner::vm::teardown::TeardownState;

    let stub = DigitalOceanStub::start().await;
    let run = Run::new(&stub, "stub-diag").await;
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

    let report = check().await;
    assert_eq!(
        (report.verdict, report.source),
        (Verdict::Healthy, Source::Vendor)
    );

    stub.set_status("off");
    let report = check().await;
    assert_eq!(report.verdict, Verdict::Unhealthy, "{report:#?}");
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.detail.contains("keeps billing powered-off Droplets")),
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
