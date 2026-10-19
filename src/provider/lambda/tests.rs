//! Lambda adapter tests: pure helpers, plus each HTTP primitive against a
//! `wiremock` server standing in for the Cloud API v1.

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{BootScript, ErrorKind, GpuRequest, ResourceAsk};

const OUR_KEY: &str = "ssh-ed25519 AAAAour gpuprov";
const API_KEY: &str = "secret_key";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn instance(value: serde_json::Value) -> Instance {
    serde_json::from_value(value).unwrap()
}

fn base_instance(status: &str) -> serde_json::Value {
    json!({
        "id": "i-1", "name": "gpuprov-dev", "status": status, "ip": "203.0.113.5",
        "private_ip": "10.0.0.5", "region": {"name": "us-east-1", "description": "Virginia"},
        "tags": [{"key": "gpuprov-vm-name", "value": "dev"}]
    })
}

#[test]
fn instance_status_and_endpoint_mapping() {
    let active = to_observed(&instance(base_instance("active"))).unwrap();
    assert!(active.is_ready());
    assert_eq!(
        active.endpoint,
        Some(SshEndpoint {
            host: "203.0.113.5".into(),
            port: 22,
            user: None,
        })
    );
    assert_eq!(active.private_ip.as_deref(), Some("10.0.0.5"));

    for (status, expected) in [
        ("booting", InstanceStatus::Pending),
        ("unhealthy", InstanceStatus::Pending),
        ("terminating", InstanceStatus::Terminating),
        ("preempted", InstanceStatus::Failed("preempted".into())),
    ] {
        let o = to_observed(&instance(base_instance(status))).unwrap();
        assert_eq!(o.status, expected, "{status}");
        assert!(o.endpoint.is_none(), "{status} has no endpoint");
    }
    assert!(to_observed(&instance(base_instance("terminated"))).is_none());

    // Active but no address yet: not reachable.
    let mut no_ip = base_instance("active");
    no_ip["ip"] = serde_json::Value::Null;
    assert!(!to_observed(&instance(no_ip)).unwrap().is_ready());
}

#[test]
fn instances_belong_to_a_vm_by_tag_or_name() {
    let tagged = instance(base_instance("active"));
    assert!(is_vm_instance(&tagged, "dev"));
    assert!(!is_vm_instance(&tagged, "dev-2"));
    let untagged = instance(json!({
        "id": "i-2", "name": "gpuprov-dev", "status": "active",
        "region": {"name": "us-east-1", "description": ""}
    }));
    assert!(is_vm_instance(&untagged, "dev"));
    let foreign = instance(json!({
        "id": "i-3", "name": "my-box", "status": "active",
        "region": {"name": "us-east-1", "description": ""}
    }));
    assert!(!is_vm_instance(&foreign, "dev"));
}

#[test]
fn ruleset_opens_ssh_plus_requested_ports_once() {
    let rules = ruleset_rules(&[6443, 22, 80]);
    let ports: Vec<[u16; 2]> = rules.iter().map(|r| r.port_range.unwrap()).collect();
    assert_eq!(ports, [[22, 22], [80, 80], [6443, 6443]]);
    assert!(rules
        .iter()
        .all(|r| r.protocol == "tcp" && r.source_network == "0.0.0.0/0"));
}

#[test]
fn image_overrides_are_ids_or_families() {
    assert_eq!(
        image_ref("0e5e7b3c-7f7e-4d6a-9d5e-123456789abc"),
        ImageRef::Id {
            id: "0e5e7b3c-7f7e-4d6a-9d5e-123456789abc".into()
        }
    );
    assert_eq!(
        image_ref("lambda-stack-24-04"),
        ImageRef::Family {
            family: "lambda-stack-24-04".into()
        }
    );
}

#[test]
fn capabilities_reject_spot_and_run_boot_scripts() {
    let provider = LambdaProvider::with_api("http://localhost:1", API_KEY).unwrap();
    let caps = provider.capabilities();
    assert!(caps.boot_script);
    assert!(!caps.supports(Feature::Spot));
    assert!(caps.supports(Feature::OpenPorts));
    assert!(caps.supports(Feature::Kubernetes));
    assert!(caps.supports(Feature::ImageOverride));
}

#[test]
fn missing_api_key_is_a_credentials_error() {
    std::env::remove_var(API_KEY_ENV);
    let err = match LambdaProvider::new(&ApiKeyConfig::default()) {
        Err(e) => e,
        Ok(_) => panic!("constructed without a key"),
    };
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert!(err.msg.contains("LAMBDA_API_KEY"), "{}", err.msg);
}

// ---------------------------------------------------------------------------
// HTTP primitives against a mock API
// ---------------------------------------------------------------------------

fn provider(server: &MockServer) -> LambdaProvider {
    LambdaProvider::with_api(&server.uri(), API_KEY).unwrap()
}

fn catalog_body(a10_regions: &[&str]) -> serde_json::Value {
    let regions = |names: &[&str]| -> Vec<serde_json::Value> {
        names
            .iter()
            .map(|n| json!({"name": n, "description": n}))
            .collect()
    };
    json!({"data": {
        "gpu_1x_a10": {
            "instance_type": {
                "name": "gpu_1x_a10", "description": "1x A10 (24 GB PCIe)",
                "gpu_description": "A10 (24 GB PCIe)", "price_cents_per_hour": 129,
                "specs": {"vcpus": 30, "memory_gib": 200, "storage_gib": 1400, "gpus": 1},
                "architecture": "x86_64"
            },
            "regions_with_capacity_available": regions(a10_regions)
        },
        "gpu_1x_a6000": {
            "instance_type": {
                "name": "gpu_1x_a6000", "description": "1x RTX A6000 (48 GB)",
                "gpu_description": "RTX A6000 (48 GB)", "price_cents_per_hour": 109,
                "specs": {"vcpus": 14, "memory_gib": 100, "storage_gib": 512, "gpus": 1},
                "architecture": "x86_64"
            },
            "regions_with_capacity_available": regions(&["us-south-1"])
        }
    }})
}

async fn mount_catalog(server: &MockServer, a10_regions: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/api/v1/instance-types"))
        .and(header(
            "authorization",
            format!("Bearer {API_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(catalog_body(a10_regions)))
        .mount(server)
        .await;
}

fn request(gpu: &str) -> ResourceRequest {
    ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: gpu.into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    }
}

fn spec(ports: Vec<u16>) -> InstanceSpec {
    InstanceSpec {
        vm_name: "dev".into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: ports,
        user_data: crate::userdata::build_cloud_init("ubuntu", OUR_KEY),
        ssh_public_key: OUR_KEY.into(),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

async fn offer_for(server: &MockServer, gpu: &str) -> Offer {
    provider(server)
        .plan(&request(gpu))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("an offer")
}

#[tokio::test]
async fn plan_builds_priced_offers_per_region_with_capacity() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-west-1", "us-east-1", "europe-central-1"]).await;
    let offers = provider(&server).plan(&request("A10")).await.unwrap();
    let rendered: Vec<String> = offers
        .iter()
        .map(|o| format!("{}@{}", o.instance_type, o.region))
        .collect();
    // US regions only by default, ordered by region at equal price.
    assert_eq!(rendered, ["gpu_1x_a10@us-east-1", "gpu_1x_a10@us-west-1"]);
    assert_eq!(offers[0].hourly_price, Some(1.29));
}

#[tokio::test]
async fn plan_errors_are_classified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/instance-types"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {
            "code": "global/invalid-api-key", "message": "API key was invalid, expired, or deleted."
        }})))
        .mount(&server)
        .await;
    let err = provider(&server).plan(&request("A10")).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert!(err.msg.contains("invalid-api-key"), "{}", err.msg);
}

#[tokio::test]
async fn prepare_reuses_a_registered_key_and_skips_the_ruleset_without_ports() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "k1", "name": "laptop", "public_key": "ssh-ed25519 AAAAother me"},
            {"id": "k2", "name": "existing-gpuprov", "public_key": "ssh-ed25519 AAAAour renamed"}
        ]})))
        .mount(&server)
        .await;
    // Neither a new key nor a ruleset may be created.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let offer = offer_for(&server, "A10").await;
    let prepared = provider(&server)
        .prepare(&offer, &spec(Vec::new()))
        .await
        .unwrap();
    assert_eq!(
        prepared,
        json!({"ssh_key_name": "existing-gpuprov", "firewall_ruleset_id": null})
    );
}

#[tokio::test]
async fn prepare_registers_the_key_and_creates_or_reconciles_the_ruleset() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    let key_name = format!("gpuprov-{}", fnv1a_hex("ssh-ed25519 AAAAour"));
    Mock::given(method("POST"))
        .and(path("/api/v1/ssh-keys"))
        .and(body_json(json!({"name": key_name, "public_key": OUR_KEY})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "id": "k9", "name": key_name, "public_key": OUR_KEY
        }})))
        .expect(1)
        .mount(&server)
        .await;
    // An existing ruleset with stale rules in another region is ignored; the
    // one in this region is updated in place.
    Mock::given(method("GET"))
        .and(path("/api/v1/firewall-rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "fw-other", "name": "gpuprov-dev", "region": {"name": "us-west-1", "description": ""}, "rules": []},
            {"id": "fw-1", "name": "gpuprov-dev", "region": {"name": "us-east-1", "description": ""},
             "rules": [{"protocol": "tcp", "port_range": [22, 22], "source_network": "0.0.0.0/0", "description": "gpuprov: tcp/22"}]}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/firewall-rulesets/fw-1"))
        .and(body_json(json!({"rules": [
            {"protocol": "tcp", "port_range": [22, 22], "source_network": "0.0.0.0/0", "description": "gpuprov: tcp/22"},
            {"protocol": "tcp", "port_range": [6443, 6443], "source_network": "0.0.0.0/0", "description": "gpuprov: tcp/6443"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "id": "fw-1", "name": "gpuprov-dev", "region": {"name": "us-east-1", "description": ""}, "rules": []
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let offer = offer_for(&server, "A10").await;
    let prepared = provider(&server)
        .prepare(&offer, &spec(vec![6443]))
        .await
        .unwrap();
    assert_eq!(
        prepared,
        json!({"ssh_key_name": key_name, "firewall_ruleset_id": "fw-1"})
    );
}

#[tokio::test]
async fn prepare_creates_a_ruleset_when_none_exists() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "k2", "name": "mine", "public_key": OUR_KEY}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/firewall-rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/firewall-rulesets"))
        .and(body_json(json!({"name": "gpuprov-dev", "region": "us-east-1", "rules": [
            {"protocol": "tcp", "port_range": [22, 22], "source_network": "0.0.0.0/0", "description": "gpuprov: tcp/22"},
            {"protocol": "tcp", "port_range": [8080, 8080], "source_network": "0.0.0.0/0", "description": "gpuprov: tcp/8080"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "id": "fw-new", "name": "gpuprov-dev", "region": {"name": "us-east-1", "description": ""}, "rules": []
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let offer = offer_for(&server, "A10").await;
    let prepared = provider(&server)
        .prepare(&offer, &spec(vec![8080]))
        .await
        .unwrap();
    assert_eq!(prepared["firewall_ruleset_id"], "fw-new");
}

#[tokio::test]
async fn prepare_reports_vanished_capacity_as_unavailable_before_touching_the_account() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    let offer = offer_for(&server, "A10").await;
    server.reset().await;
    mount_catalog(&server, &[]).await;
    let err = provider(&server)
        .prepare(&offer, &spec(Vec::new()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    // No SSH-key or ruleset call was made.
    let requests = server.received_requests().await.unwrap();
    assert!(requests
        .iter()
        .all(|r| r.url.path() == "/api/v1/instance-types"));
}

#[tokio::test]
async fn create_sends_the_tagged_launch_with_cloud_init_and_ruleset() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    let offer = offer_for(&server, "A10").await;
    let mut spec = spec(vec![6443]);
    spec.boot_script = Some(BootScript {
        script: "#!/usr/bin/env bash\necho k3s\n".into(),
        env: vec![("GPUPROV_INSTALL_ORIGIN".into(), "boot".into())],
    });
    spec.image_id = Some("lambda-stack-24-04".into());
    let expected = json!({
        "region_name": "us-east-1",
        "instance_type_name": "gpu_1x_a10",
        "ssh_key_names": ["gpuprov-key"],
        "name": "gpuprov-dev",
        "hostname": "gpuprov-dev",
        "image": {"family": "lambda-stack-24-04"},
        "user_data": crate::userdata::render(&spec),
        "tags": [
            {"key": "gpuprov-vm-name", "value": "dev"},
            {"key": "gpuprov-managed", "value": "true"}
        ],
        "firewall_rulesets": [{"id": "fw-1"}]
    });
    assert!(expected["user_data"]
        .as_str()
        .unwrap()
        .contains("GPUPROV_INSTALL_ORIGIN=boot"));
    Mock::given(method("POST"))
        .and(path("/api/v1/instance-operations/launch"))
        .and(body_json(expected))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"instance_ids": ["i-new"]}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_name": "gpuprov-key", "firewall_ruleset_id": "fw-1"});
    let id = provider(&server)
        .create(&offer, &prepared, &spec)
        .await
        .unwrap();
    assert_eq!(id, "i-new");
}

#[tokio::test]
async fn create_without_capacity_is_a_capacity_error() {
    let server = MockServer::start().await;
    mount_catalog(&server, &["us-east-1"]).await;
    let offer = offer_for(&server, "A10").await;
    Mock::given(method("POST"))
        .and(path("/api/v1/instance-operations/launch"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": "instance-operations/launch/insufficient-capacity",
            "message": "Not enough capacity to fulfill launch request.",
            "suggestion": "Choose an instance type with more availability, or try again later."
        }})))
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_name": "k", "firewall_ruleset_id": null});
    let err = provider(&server)
        .create(&offer, &prepared, &spec(Vec::new()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity);
    assert!(err.msg.contains("try again later"), "{}", err.msg);
}

async fn mount_instances_pages(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/instances"))
        .and(query_param("page_token", "next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "i-2", "name": "gpuprov-dev", "status": "booting", "ip": null,
                 "region": {"name": "us-east-1", "description": ""},
                 "tags": [{"key": "gpuprov-vm-name", "value": "dev"}]},
                {"id": "i-old", "name": "gpuprov-dev", "status": "terminated",
                 "region": {"name": "us-east-1", "description": ""},
                 "tags": [{"key": "gpuprov-vm-name", "value": "dev"}]}
            ],
            "page_token": null
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/instances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                base_instance("active"),
                {"id": "i-foreign", "name": "someone-else", "status": "active", "ip": "1.1.1.1",
                 "region": {"name": "us-east-1", "description": ""}, "tags": []}
            ],
            "page_token": "next"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn observe_follows_pages_and_keeps_only_the_vms_live_instances() {
    let server = MockServer::start().await;
    mount_instances_pages(&server).await;
    let at = Location {
        region: "us-east-1".into(),
        zone: None,
    };
    let observed = provider(&server).observe(&at, "dev").await.unwrap();
    let ids: Vec<&str> = observed.iter().map(|o| o.instance_id.as_str()).collect();
    assert_eq!(ids, ["i-1", "i-2"]);
    assert!(observed[0].is_ready());
    assert_eq!(observed[1].status, InstanceStatus::Pending);
}

#[tokio::test]
async fn delete_terminates_only_live_instances_and_tolerates_404() {
    let server = MockServer::start().await;
    mount_instances_pages(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/instance-operations/terminate"))
        .and(body_json(json!({"instance_ids": ["i-1", "i-2"]})))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": {
            "code": "global/object-does-not-exist", "message": "gone"
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let at = Location {
        region: "us-east-1".into(),
        zone: None,
    };
    let ids = vec!["i-1".to_string(), "i-2".to_string(), "i-old".to_string()];
    provider(&server).delete(&at, "dev", &ids).await.unwrap();
    // Nothing live left to terminate → no call at all.
    provider(&server)
        .delete(&at, "dev", &["i-old".to_string()])
        .await
        .unwrap();
}

#[tokio::test]
async fn cleanup_deletes_the_vms_ruleset_in_the_region_and_waits_while_in_use() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/firewall-rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "fw-1", "name": "gpuprov-dev", "region": {"name": "us-east-1", "description": ""}, "rules": []},
            {"id": "fw-2", "name": "gpuprov-dev", "region": {"name": "us-west-1", "description": ""}, "rules": []},
            {"id": "fw-3", "name": "gpuprov-dev-2", "region": {"name": "us-east-1", "description": ""}, "rules": []}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/firewall-rulesets/fw-1"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": "firewall-rulesets/firewall-ruleset-in-use", "message": "in use"
        }})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/firewall-rulesets/fw-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
        .mount(&server)
        .await;
    let at = Location {
        region: "us-east-1".into(),
        zone: None,
    };
    let err = provider(&server).cleanup(&at, "dev").await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Transient);
    provider(&server).cleanup(&at, "dev").await.unwrap();
    let deleted: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(
        deleted,
        [
            "/api/v1/firewall-rulesets/fw-1",
            "/api/v1/firewall-rulesets/fw-1"
        ]
    );
}
