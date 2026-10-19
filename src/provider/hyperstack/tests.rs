//! Hyperstack adapter tests: pure helpers, plus each HTTP primitive against
//! a `wiremock` server standing in for the Infrahub API.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{BootScript, ErrorKind, GpuRequest, ResourceAsk};

const KEY: &str = "ssh-ed25519 AAAAour gpuprov";
const API_KEY: &str = "hs-key";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn vm(value: serde_json::Value) -> Vm {
    serde_json::from_value(value).unwrap()
}

#[test]
fn names_are_stable_and_scoped() {
    assert_eq!(environment_name("CANADA-1"), "gpuprov-canada-1");
    assert_eq!(vm_resource_name("dev"), "gpuprov-dev");
    assert_eq!(fnv1a_hex(""), "811c9dc5");
    // The comment does not change the key pair's identity.
    assert_eq!(
        keypair_name("ssh-ed25519 AAAA one"),
        keypair_name("ssh-ed25519 AAAA two")
    );
    assert_ne!(
        keypair_name("ssh-ed25519 AAAA"),
        keypair_name("ssh-ed25519 BBBB")
    );
}

#[test]
fn vm_status_and_endpoint_mapping() {
    let active = to_observed(&vm(json!({
        "id": 42, "name": "gpuprov-dev", "status": "ACTIVE",
        "floating_ip": "38.80.1.2", "fixed_ip": "10.0.0.5"
    })))
    .unwrap();
    assert!(active.is_ready());
    assert_eq!(active.instance_id, "42");
    assert_eq!(
        active.endpoint,
        Some(SshEndpoint {
            host: "38.80.1.2".into(),
            port: 22,
            user: None,
        })
    );
    assert_eq!(active.private_ip.as_deref(), Some("10.0.0.5"));
    // Running but the public IP is not attached yet: not reachable.
    let no_ip = to_observed(&vm(json!({"id": 1, "status": "ACTIVE", "floating_ip": ""}))).unwrap();
    assert!(!no_ip.is_ready());
    for (status, expected) in [
        ("CREATING", InstanceStatus::Pending),
        ("BUILD", InstanceStatus::Pending),
        ("RESTORING", InstanceStatus::Pending),
        ("STOPPING", InstanceStatus::Stopping),
        ("SHUTOFF", InstanceStatus::Stopped),
        ("HIBERNATED", InstanceStatus::Stopped),
        ("DELETING", InstanceStatus::Terminating),
        ("ERROR", InstanceStatus::Failed("ERROR".into())),
    ] {
        let o = to_observed(&vm(json!({"id": 1, "status": status}))).unwrap();
        assert_eq!(o.status, expected, "{status}");
        assert!(o.endpoint.is_none());
    }
    assert!(to_observed(&vm(json!({"id": 1, "status": "DELETED"}))).is_none());
}

#[test]
fn capabilities_support_everything_and_boot_scripts() {
    let p = HyperstackProvider::with_api("http://unused", API_KEY).unwrap();
    let caps = p.capabilities();
    assert!(caps.unsupported.is_empty());
    assert!(caps.boot_script);
    assert_eq!(caps.boot_timeout, BOOT_TIMEOUT);
}

#[test]
fn missing_key_is_a_credentials_error() {
    std::env::remove_var(API_KEY_ENV);
    let Err(err) = HyperstackProvider::new(&ApiKeyConfig::default()) else {
        panic!("expected a credentials error");
    };
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert!(err.msg.contains("HYPERSTACK_API_KEY"));
}

// ---------------------------------------------------------------------------
// HTTP primitives
// ---------------------------------------------------------------------------

fn provider(server: &MockServer) -> HyperstackProvider {
    HyperstackProvider::with_api(&server.uri(), API_KEY).unwrap()
}

fn flavors_body() -> serde_json::Value {
    json!({"status": true, "data": [
        {"gpu": "RTX-A6000", "region_name": "CANADA-1", "flavors": [
            {"name": "n3-RTX-A6000x1", "gpu": "RTX-A6000", "gpu_count": 1, "cpu": 28, "ram": 58.0,
             "disk": 100, "region_name": "CANADA-1", "stock_available": true},
            {"name": "n3-RTX-A6000x1", "gpu": "RTX-A6000", "gpu_count": 1, "cpu": 28, "ram": 58.0,
             "disk": 100, "region_name": "NORWAY-1", "stock_available": true}
        ]},
        {"gpu": "L40", "region_name": "CANADA-1", "flavors": [
            {"name": "n3-L40x1", "gpu": "L40", "gpu_count": 1, "region_name": "CANADA-1",
             "stock_available": true}
        ]}
    ]})
}

fn stocks_body() -> serde_json::Value {
    json!({"stocks": [{"region": "CANADA-1", "stock-type": "GPU", "models": [
        {"model": "RTX-A6000", "available": "10", "configurations": {"1x": 5, "8x": 0}},
        {"model": "L40", "available": "0", "configurations": {"1x": 0}}
    ]}]})
}

async fn mount_catalog(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/core/flavors"))
        .and(header("api_key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(flavors_body()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/stocks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stocks_body()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pricebook"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "RTX-A6000", "value": 0.5, "original_value": 0.5, "discount_applied": false},
            {"id": 2, "name": "L40", "value": 1.0}
        ])))
        .mount(server)
        .await;
}

fn gpu_request(name: &str) -> ResourceRequest {
    ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: name.into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    }
}

#[tokio::test]
async fn plan_joins_flavor_stock_and_price_in_north_america() {
    let server = MockServer::start().await;
    mount_catalog(&server).await;
    let p = provider(&server);
    let offers = p.plan(&gpu_request("A6000")).await.unwrap();
    // Norway is excluded by default; Canada is in stock and priced.
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].instance_type, "n3-RTX-A6000x1");
    assert_eq!(offers[0].region, "CANADA-1");
    assert_eq!(offers[0].hourly_price, Some(0.5));
    // L40: the flavor says in stock but the stock feed has no 1x free.
    assert!(p.plan(&gpu_request("L40")).await.unwrap().is_empty());
}

#[tokio::test]
async fn plan_bad_key_is_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"status": false, "message": "Invalid API key"})),
        )
        .mount(&server)
        .await;
    let err = provider(&server)
        .plan(&gpu_request("A6000"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
}

fn offer_for(region: &str) -> Offer {
    let detail = OfferDetail {
        flavor_name: "n3-RTX-A6000x1".into(),
        region: region.into(),
        gpu: "RTX-A6000".into(),
        gpu_count: 1,
        spot: false,
        hard_images: Vec::new(),
    };
    let mut offer = Offer::new("n3-RTX-A6000x1", region);
    offer.detail = serde_json::to_value(detail).unwrap();
    offer
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        vm_name: "dev".into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: vec![6443],
        user_data: "#cloud-config\nusers: []\n".into(),
        ssh_public_key: KEY.into(),
        ssh_user: "ubuntu".into(),
        boot_script: Some(BootScript {
            script: "#!/usr/bin/env bash\necho hi\n".into(),
            env: vec![("K".into(), "V".into())],
        }),
    }
}

async fn mount_prepare_lookups(
    server: &MockServer,
    envs: serde_json::Value,
    keys: serde_json::Value,
) {
    Mock::given(method("GET"))
        .and(path("/core/flavors"))
        .and(query_param("region", "CANADA-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(flavors_body()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/stocks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stocks_body()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/environments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envs))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/keypairs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(keys))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/images"))
        .and(query_param("region", "CANADA-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"images": [
            {"region_name": "CANADA-1", "type": "Ubuntu", "images": [
                {"name": "Ubuntu Server 22.04 LTS R535 CUDA 12.2"},
                {"name": "Ubuntu Server 24.04 LTS R570 CUDA 12.8"},
                {"name": "Ubuntu Server 24.04 LTS (Noble Numbat)"}
            ]}
        ]})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn prepare_reuses_existing_environment_and_keypair() {
    let server = MockServer::start().await;
    mount_prepare_lookups(
        &server,
        json!({"environments": [{"id": 1, "name": "gpuprov-canada-1", "region": "CANADA-1"}]}),
        json!({"keypairs": [{"id": 9, "name": "my-old-key", "public_key": "ssh-ed25519 AAAAour other-comment",
                             "environment": {"name": "gpuprov-canada-1"}}]}),
    )
    .await;
    // Nothing may be created.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let prepared = provider(&server)
        .prepare(&offer_for("CANADA-1"), &spec())
        .await
        .unwrap();
    assert_eq!(
        prepared,
        json!({"environment": "gpuprov-canada-1", "key_name": "my-old-key",
               "image": "Ubuntu Server 24.04 LTS R570 CUDA 12.8"})
    );
}

#[tokio::test]
async fn prepare_creates_environment_and_imports_key_when_missing() {
    let server = MockServer::start().await;
    mount_prepare_lookups(
        &server,
        json!({"environments": []}),
        // Same key in a different environment does not count.
        json!({"keypairs": [{"id": 9, "name": "elsewhere", "public_key": KEY,
                             "environment": {"name": "other-env"}}]}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/core/environments"))
        .and(body_partial_json(
            json!({"name": "gpuprov-canada-1", "region": "CANADA-1"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/core/keypairs"))
        .and(body_partial_json(json!({
            "name": keypair_name(KEY), "environment_name": "gpuprov-canada-1", "public_key": KEY
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": true})))
        .expect(1)
        .mount(&server)
        .await;
    let prepared = provider(&server)
        .prepare(&offer_for("CANADA-1"), &spec())
        .await
        .unwrap();
    assert_eq!(prepared["key_name"], keypair_name(KEY));
}

#[tokio::test]
async fn prepare_sold_out_flavor_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/core/flavors"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"flavors": [
                {"name": "n3-RTX-A6000x1", "gpu": "RTX-A6000", "gpu_count": 1,
                 "region_name": "CANADA-1", "stock_available": false}
            ]}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/core/stocks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"stocks": []})))
        .mount(&server)
        .await;
    let err = provider(&server)
        .prepare(&offer_for("CANADA-1"), &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
}

#[tokio::test]
async fn create_sends_the_documented_body() {
    let server = MockServer::start().await;
    let s = spec();
    Mock::given(method("POST"))
        .and(path("/core/virtual-machines"))
        .and(header("api_key", API_KEY))
        .and(body_partial_json(json!({
            "name": "gpuprov-dev",
            "environment_name": "gpuprov-canada-1",
            "image_name": "Ubuntu Server 24.04 LTS R570 CUDA 12.8",
            "flavor_name": "n3-RTX-A6000x1",
            "key_name": "k",
            "count": 1,
            "assign_floating_ip": true,
            "enable_port_randomization": false,
            "user_data": userdata::render(&s),
            "labels": ["gpuprov-vm-name=dev", "gpuprov-managed"],
            "security_rules": [
                {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",
                 "remote_ip_prefix": "0.0.0.0/0", "port_range_min": 22, "port_range_max": 22},
                {"direction": "ingress", "ethertype": "IPv4", "protocol": "tcp",
                 "remote_ip_prefix": "0.0.0.0/0", "port_range_min": 6443, "port_range_max": 6443}
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": true, "instances": [{"id": 777, "name": "gpuprov-dev", "status": "CREATING"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let prepared = json!({"environment": "gpuprov-canada-1", "key_name": "k",
                          "image": "Ubuntu Server 24.04 LTS R570 CUDA 12.8"});
    let id = provider(&server)
        .create(&offer_for("CANADA-1"), &prepared, &s)
        .await
        .unwrap();
    assert_eq!(id, "777");
    // The boot script rides in the cloud-init user data.
    assert!(userdata::render(&s).contains("write_files"));
}

#[tokio::test]
async fn create_stock_out_is_capacity() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/virtual-machines"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "status": false, "message": "Not enough capacity for flavor n3-RTX-A6000x1",
            "error_reason": "insufficient resources"
        })))
        .mount(&server)
        .await;
    let prepared = json!({"environment": "e", "key_name": "k", "image": "i"});
    let err = provider(&server)
        .create(&offer_for("CANADA-1"), &prepared, &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity);
}

#[tokio::test]
async fn observe_filters_by_environment_and_exact_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/core/virtual-machines"))
        .and(query_param("environment", "gpuprov-canada-1"))
        .and(query_param("exact_environment_match", "true"))
        .and(query_param("search", "gpuprov-dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"status": true, "instances": [
                {"id": 1, "name": "gpuprov-dev", "status": "ACTIVE", "floating_ip": "38.80.1.2"},
                {"id": 2, "name": "gpuprov-dev-2", "status": "ACTIVE", "floating_ip": "38.80.1.3"}
            ]}),
        ))
        .mount(&server)
        .await;
    let at = Location {
        region: "CANADA-1".into(),
        zone: None,
    };
    let observed = provider(&server).observe(&at, "dev").await.unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].instance_id, "1");
    assert!(observed[0].is_ready());
}

#[tokio::test]
async fn delete_treats_missing_as_done_and_busy_as_transient() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/core/virtual-machines/1"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"status": false, "message": "not found"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/core/virtual-machines/2"))
        .respond_with(
            ResponseTemplate::new(409)
                .set_body_json(json!({"status": false, "message": "VM is building"})),
        )
        .mount(&server)
        .await;
    let p = provider(&server);
    let at = Location {
        region: "CANADA-1".into(),
        zone: None,
    };
    p.delete(&at, "dev", &["1".into()]).await.unwrap();
    let err = p.delete(&at, "dev", &["2".into()]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Transient);
    p.cleanup(&at, "dev").await.unwrap();
}
