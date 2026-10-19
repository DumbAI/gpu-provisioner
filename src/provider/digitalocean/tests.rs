//! DigitalOcean adapter tests: pure helpers, plus each HTTP primitive
//! against a `wiremock` server standing in for API v2.

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{BootScript, ErrorKind, GpuRequest, ResourceAsk, Severity};

const OUR_KEY: &str = "ssh-ed25519 AAAAour gpuprov";
const TOKEN: &str = "dop_v1_secret";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn droplet(value: serde_json::Value) -> Droplet {
    serde_json::from_value(value).unwrap()
}

fn base_droplet(status: &str) -> serde_json::Value {
    json!({
        "id": 3164444, "name": "gpuprov-dev", "status": status, "locked": false,
        "networks": {"v4": [
            {"ip_address": "10.128.0.5", "netmask": "255.255.0.0", "gateway": "10.128.0.1", "type": "private"},
            {"ip_address": "203.0.113.5", "netmask": "255.255.255.0", "gateway": "203.0.113.1", "type": "public"}
        ], "v6": []},
        "region": {"slug": "tor1", "name": "Toronto 1"},
        "tags": ["gpuprov", "gpuprov-vm-dev"]
    })
}

#[test]
fn droplet_status_and_endpoint_mapping() {
    let active = to_observed(&droplet(base_droplet("active"))).unwrap();
    assert!(active.is_ready());
    assert_eq!(active.instance_id, "3164444");
    assert_eq!(
        active.endpoint,
        Some(SshEndpoint {
            host: "203.0.113.5".into(),
            port: 22,
            user: Some("root".into()),
        })
    );
    assert_eq!(active.private_ip.as_deref(), Some("10.128.0.5"));

    for (status, expected) in [
        ("new", InstanceStatus::Pending),
        ("off", InstanceStatus::Stopped),
    ] {
        let o = to_observed(&droplet(base_droplet(status))).unwrap();
        assert_eq!(o.status, expected, "{status}");
        assert!(o.endpoint.is_none(), "{status} has no endpoint");
    }
    assert!(to_observed(&droplet(base_droplet("archive"))).is_none());

    // Active but no public address yet: not reachable.
    let mut no_ip = base_droplet("active");
    no_ip["networks"]["v4"] = json!([]);
    assert!(!to_observed(&droplet(no_ip)).unwrap().is_ready());
}

#[test]
fn powered_off_and_locked_become_findings() {
    let off = findings_for(&droplet(base_droplet("off")));
    assert_eq!(off.len(), 1);
    assert_eq!(off[0].severity, Severity::Critical);
    assert!(off[0].message.contains("keeps billing"), "{off:?}");

    let mut locked = base_droplet("active");
    locked["locked"] = json!(true);
    let findings = findings_for(&droplet(locked));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Info);
    assert!(findings_for(&droplet(base_droplet("active"))).is_empty());
}

#[test]
fn diagnosis_picks_the_recorded_droplet_and_attaches_its_findings() {
    let mut other = base_droplet("active");
    other["id"] = json!(2);
    let droplets = [droplet(base_droplet("off")), droplet(other)];
    let diagnosis = diagnosis_from(&droplets, Some("3164444"));
    assert_eq!(diagnosis.instance.unwrap().instance_id, "3164444");
    assert_eq!(diagnosis.findings.len(), 1);

    let gone = diagnosis_from(&[droplet(base_droplet("archive"))], Some("3164444"));
    assert!(gone.instance.is_none() && gone.findings.is_empty());
}

#[test]
fn firewall_opens_ssh_ports_and_icmp_and_all_outbound() {
    let spec = firewall_spec("dev", &[6443, 22, 80]);
    assert_eq!(spec.name, "gpuprov-dev");
    assert_eq!(spec.tags, ["gpuprov-vm-dev"]);
    let inbound: Vec<(String, Option<String>)> = spec
        .inbound_rules
        .iter()
        .map(|r| (r.protocol.clone(), r.ports.clone()))
        .collect();
    assert_eq!(
        inbound,
        [
            ("tcp".to_string(), Some("22".to_string())),
            ("tcp".into(), Some("80".into())),
            ("tcp".into(), Some("6443".into())),
            ("icmp".into(), None),
        ]
    );
    assert!(spec
        .inbound_rules
        .iter()
        .all(|r| r.sources.addresses == ["0.0.0.0/0", "::/0"]));
    let outbound: Vec<&str> = spec
        .outbound_rules
        .iter()
        .map(|r| r.protocol.as_str())
        .collect();
    assert_eq!(outbound, ["tcp", "udp", "icmp"]);
    assert_eq!(spec.outbound_rules[0].ports.as_deref(), Some("0"));
}

#[test]
fn image_overrides_are_ids_or_slugs() {
    assert_eq!(image_ref("12345678"), json!(12345678));
    assert_eq!(image_ref("ubuntu-22-04-x64"), json!("ubuntu-22-04-x64"));
}

#[test]
fn ssh_key_material_ignores_comments() {
    assert_eq!(
        key_material("ssh-ed25519 AAAAour other-comment"),
        key_material(OUR_KEY)
    );
    assert_eq!(fnv1a_hex(""), "811c9dc5");
}

// ---------------------------------------------------------------------------
// HTTP primitives
// ---------------------------------------------------------------------------

fn provider(server: &MockServer) -> DigitalOceanProvider {
    DigitalOceanProvider::with_api(&server.uri(), TOKEN).unwrap()
}

fn sizes_page() -> serde_json::Value {
    json!({
        "sizes": [
            {"slug": "gpu-h100x1-80gb", "memory": 245760, "vcpus": 20, "disk": 720,
             "transfer": 15, "price_monthly": 2277.12, "price_hourly": 3.39,
             "regions": ["nyc2", "tor1"], "available": true, "description": "H100 GPU - 1X",
             "gpu_info": {"count": 1, "model": "nvidia_h100", "vram": {"amount": 80, "unit": "gib"}}},
            {"slug": "s-1vcpu-1gb", "memory": 1024, "vcpus": 1, "disk": 25, "transfer": 1,
             "price_monthly": 6, "price_hourly": 0.00893, "regions": ["nyc1"],
             "available": true, "description": "Basic"}
        ],
        "links": {},
        "meta": {"total": 2}
    })
}

fn offer() -> Offer {
    let detail = catalog::OfferDetail {
        size: "gpu-h100x1-80gb".into(),
        region: "tor1".into(),
        image: catalog::IMAGE_NVIDIA_SINGLE.into(),
    };
    let mut offer = Offer::new("gpu-h100x1-80gb", "tor1");
    offer.detail = serde_json::to_value(detail).unwrap();
    offer
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        vm_name: "dev".into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: vec![8080],
        user_data: "#cloud-config\n".into(),
        ssh_public_key: OUR_KEY.into(),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

fn h100() -> ResourceRequest {
    ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    }
}

#[tokio::test]
async fn plan_follows_pagination_with_bearer_auth() {
    let server = MockServer::start().await;
    let mut first = sizes_page();
    first["sizes"] = json!([first["sizes"][1].clone()]);
    first["links"] = json!({"pages": {"next": "https://api.digitalocean.com/v2/sizes?page=2"}});
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .and(query_param("page", "1"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .expect(1)
        .mount(&server)
        .await;
    let mut second = sizes_page();
    second["sizes"] = json!([second["sizes"][0].clone()]);
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(second))
        .expect(1)
        .mount(&server)
        .await;

    let offers = provider(&server).plan(&h100()).await.unwrap();
    let names: Vec<String> = offers.iter().map(|o| o.to_string()).collect();
    assert_eq!(offers.len(), 2, "{names:?}");
    assert_eq!(offers[0].region, "nyc2");
}

#[tokio::test]
async fn plan_with_a_bad_token_is_a_credentials_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(
                json!({"id": "unauthorized", "message": "Unable to authenticate you"}),
            ),
        )
        .mount(&server)
        .await;
    let err = provider(&server).plan(&h100()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
}

#[tokio::test]
async fn prepare_reuses_the_key_creates_tag_and_firewall() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sizes_page()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/account/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ssh_keys": [{"id": 512189, "fingerprint": "3b:16", "name": "laptop",
                          "public_key": "ssh-ed25519 AAAAour laptop"}],
            "links": {}, "meta": {"total": 1}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/tags/gpuprov-vm-dev"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"id": "not_found", "message": "The resource you requested could not be found."}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/tags"))
        .and(body_json(json!({"name": "gpuprov-vm-dev"})))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"tag": {"name": "gpuprov-vm-dev"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/firewalls"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"firewalls": [], "links": {}, "meta": {"total": 0}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/firewalls"))
        .and(body_json(
            serde_json::to_value(firewall_spec("dev", &[8080])).unwrap(),
        ))
        .respond_with(
            ResponseTemplate::new(202).set_body_json(json!({"firewall": {
                "id": "bb4b2611-3d72-467b-8602-280330ecd65c", "name": "gpuprov-dev",
                "status": "waiting", "tags": ["gpuprov-vm-dev"]
            }})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let prepared = provider(&server).prepare(&offer(), &spec()).await.unwrap();
    assert_eq!(
        prepared,
        json!({"ssh_key_id": 512189, "firewall_id": "bb4b2611-3d72-467b-8602-280330ecd65c"})
    );
}

#[tokio::test]
async fn prepare_fails_unavailable_before_creating_anything_when_stale() {
    let server = MockServer::start().await;
    let mut sizes = sizes_page();
    sizes["sizes"][0]["regions"] = json!(["nyc2"]);
    Mock::given(method("GET"))
        .and(path("/v2/sizes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sizes))
        .mount(&server)
        .await;
    let err = provider(&server)
        .prepare(&offer(), &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "only the size re-check ran");
}

#[tokio::test]
async fn create_sends_the_droplet_with_key_tags_image_and_boot_script() {
    let server = MockServer::start().await;
    let mut spec = spec();
    spec.boot_script = Some(BootScript {
        script: "echo hi\n".into(),
        env: Vec::new(),
    });
    let expected = json!({
        "name": "gpuprov-dev",
        "region": "tor1",
        "size": "gpu-h100x1-80gb",
        "image": "gpu-h100x1-base",
        "ssh_keys": [512189],
        "tags": ["gpuprov", "gpuprov-vm-dev"],
        "user_data": userdata::render(&spec),
    });
    Mock::given(method("POST"))
        .and(path("/v2/droplets"))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({
            "droplet": {"id": 3164444, "name": "gpuprov-dev", "status": "new",
                        "networks": {"v4": [], "v6": []}, "region": {"slug": "tor1"},
                        "tags": ["gpuprov", "gpuprov-vm-dev"]},
            "links": {"actions": [{"id": 7515, "rel": "create", "href": "https://api.digitalocean.com/v2/actions/7515"}]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_id": 512189, "firewall_id": "fw"});
    let id = provider(&server)
        .create(&offer(), &prepared, &spec)
        .await
        .unwrap();
    assert_eq!(id, "3164444");
    assert!(userdata::render(&spec).contains("echo hi"));
}

#[tokio::test]
async fn create_rejected_for_stock_is_a_capacity_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/droplets"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "id": "unprocessable_entity",
            "message": "Size is not available in this region."
        })))
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_id": 1, "firewall_id": "fw"});
    let err = provider(&server)
        .create(&offer(), &prepared, &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity, "{}", err.msg);
}

#[tokio::test]
async fn observe_lists_by_identity_tag() {
    let server = MockServer::start().await;
    let mut foreign = base_droplet("active");
    foreign["id"] = json!(1);
    foreign["tags"] = json!(["other"]);
    Mock::given(method("GET"))
        .and(path("/v2/droplets"))
        .and(query_param("tag_name", "gpuprov-vm-dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "droplets": [base_droplet("new"), foreign],
            "links": {}, "meta": {"total": 2}
        })))
        .mount(&server)
        .await;
    let observed = provider(&server)
        .observe(&offer().location(), "dev")
        .await
        .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].status, InstanceStatus::Pending);
}

#[tokio::test]
async fn delete_by_id_treats_not_found_as_done() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets/3164444"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/droplets/9"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"id": "not_found", "message": "The resource you were accessing could not be found."}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server)
        .delete(
            &offer().location(),
            "dev",
            &["3164444".to_string(), "9".to_string()],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn cleanup_deletes_the_vm_firewall_and_tag_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/firewalls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "firewalls": [
                {"id": "fw-ours", "name": "gpuprov-dev", "tags": ["gpuprov-vm-dev"]},
                {"id": "fw-other", "name": "gpuprov-dev-2", "tags": ["gpuprov-vm-dev-2"]}
            ],
            "links": {}, "meta": {"total": 2}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/firewalls/fw-ours"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/tags/gpuprov-vm-dev"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server)
        .cleanup(&offer().location(), "dev")
        .await
        .unwrap();
}

#[test]
fn spot_is_unsupported() {
    let server_less = DigitalOceanProvider::with_api("http://127.0.0.1:9", TOKEN).unwrap();
    let caps = server_less.capabilities();
    assert!(!caps.supports(Feature::Spot));
    assert!(caps.supports(Feature::OpenPorts) && caps.boot_script);
}
