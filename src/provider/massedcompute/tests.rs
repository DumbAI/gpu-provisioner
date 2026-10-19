//! Massed Compute adapter tests: pure helpers, plus each HTTP primitive
//! against a `wiremock` server standing in for the VM API.

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{ErrorKind, GpuRequest, ResourceAsk};

const OUR_KEY: &str = "ssh-ed25519 AAAAour gpuprov";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn key_name_is_stable_and_ignores_the_comment() {
    let a = key_name("ssh-ed25519 AAAAour gpuprov");
    let b = key_name("ssh-ed25519 AAAAour other-comment\n");
    assert_eq!(a, b);
    assert!(
        a.starts_with("gpuprov-") && a.len() == "gpuprov-".len() + 8,
        "{a}"
    );
    assert_ne!(a, key_name("ssh-ed25519 BBBBother"));
}

fn instance(value: serde_json::Value) -> Instance {
    serde_json::from_value(value).unwrap()
}

#[test]
fn instance_status_and_endpoint_mapping() {
    let running = instance(json!({
        "uuid": "u1", "name": "gpuprov-vm1", "ip": "203.0.113.5", "username": "Ubuntu",
        "password": "secret", "status": "rented", "os_booted": 1, "active": 1,
        "product": {"name": "gpu_1x_a6000"}
    }));
    let o = to_observed(&running).unwrap();
    assert!(o.is_ready());
    assert_eq!(
        o.endpoint.unwrap(),
        SshEndpoint {
            host: "203.0.113.5".into(),
            port: 22,
            user: Some("Ubuntu".into()),
        }
    );

    // Still booting, or no IP yet: pending, no endpoint.
    for pending in [
        json!({"uuid": "u2", "name": "x", "ip": "203.0.113.5", "status": "rented", "os_booted": 0, "active": 1}),
        json!({"uuid": "u3", "name": "x", "ip": "", "status": "booting", "os_booted": 1, "active": 1}),
        json!({"uuid": "u4", "name": "x", "status": "initializing"}),
    ] {
        let o = to_observed(&instance(pending)).unwrap();
        assert_eq!(o.status, InstanceStatus::Pending);
        assert!(o.endpoint.is_none());
    }

    // No username reported: the Ubuntu images' user.
    let unnamed = instance(json!({
        "uuid": "u5", "name": "x", "ip": "198.51.100.1", "username": "", "status": "running",
        "os_booted": 1, "active": 1
    }));
    assert_eq!(
        to_observed(&unnamed)
            .unwrap()
            .endpoint
            .unwrap()
            .user
            .as_deref(),
        Some("Ubuntu")
    );

    let stopped = instance(json!({"uuid": "u6", "name": "x", "status": "Stopped"}));
    assert_eq!(
        to_observed(&stopped).unwrap().status,
        InstanceStatus::Stopped
    );
    let failed = instance(json!({"uuid": "u7", "name": "x", "status": "error"}));
    assert!(matches!(
        to_observed(&failed).unwrap().status,
        InstanceStatus::Failed(_)
    ));
    let gone = instance(json!({"uuid": "u8", "name": "x", "status": "terminated"}));
    assert!(to_observed(&gone).is_none());
}

#[test]
fn capabilities_are_honest() {
    let provider = MassedComputeProvider::with_api("http://unused", "t").unwrap();
    let caps = provider.capabilities();
    assert!(!caps.supports(Feature::OpenPorts));
    assert!(!caps.supports(Feature::Spot));
    // Full VMs: k3s runs; the installer goes over SSH.
    assert!(caps.supports(Feature::Kubernetes));
    assert!(caps.supports(Feature::ImageOverride));
    assert!(!caps.boot_script);
}

#[test]
fn explicit_token_is_used() {
    let config = ApiKeyConfig {
        api_key: Some("tok".into()),
        base_url: Some("http://localhost:1".into()),
        regions: Vec::new(),
    };
    assert!(MassedComputeProvider::new(&config).is_ok());
}

// ---------------------------------------------------------------------------
// HTTP primitives
// ---------------------------------------------------------------------------

fn inventory_body(a6000_capacity: u32) -> serde_json::Value {
    json!({"gpu_inventory": {
        "gpu_1x_a6000": {
            "instance_type": {"name": "gpu_1x_a6000", "description": "1x RTX A6000",
                              "price_cents_per_hour": 57,
                              "specs": {"vcpu_count": 6, "memory_gib": 48, "storage_gb": 256}},
            "regions_with_capacity_available": if a6000_capacity > 0 {
                json!([{"name": "us-central-3", "description": "Des Moines, IA"}])
            } else { json!([]) },
            "capacity_available": a6000_capacity
        },
        "gpu_1x_a5000": {
            "instance_type": {"name": "gpu_1x_a5000", "description": "1x RTX A5000",
                              "price_cents_per_hour": 44, "specs": {}},
            "regions_with_capacity_available": [{"name": "us-central-2", "description": "x"}],
            "capacity_available": 3
        }
    }})
}

async fn server() -> (MockServer, MassedComputeProvider) {
    let server = MockServer::start().await;
    let provider = MassedComputeProvider::with_api(&server.uri(), "tok").unwrap();
    (server, provider)
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

fn spec() -> InstanceSpec {
    InstanceSpec {
        vm_name: "vm1".into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: Vec::new(),
        user_data: String::new(),
        ssh_public_key: OUR_KEY.into(),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

fn images_body() -> serde_json::Value {
    json!({"images": [
        {"vm_image_id": 7, "vm_image_name": "Art", "vm_image_description": "AI tools"},
        {"vm_image_id": 11, "vm_image_name": "Ubuntu 24.04", "vm_image_description": "Base"}
    ]})
}

#[tokio::test]
async fn plan_reads_inventory_with_bearer_token() {
    let (server, provider) = server().await;
    Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inventory_body(2)))
        .expect(1)
        .mount(&server)
        .await;
    let offers = provider.plan(&request("A6000")).await.unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].instance_type, "gpu_1x_a6000");
    assert_eq!(offers[0].region, "any");
    assert_eq!(offers[0].hourly_price, Some(0.57));
}

#[tokio::test]
async fn plan_with_a_bad_token_is_a_credentials_error() {
    let (server, provider) = server().await;
    Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "Invalid token"})))
        .mount(&server)
        .await;
    let err = provider.plan(&request("A6000")).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
}

async fn planned_offer(server: &MockServer, provider: &MassedComputeProvider) -> Offer {
    let mock = Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inventory_body(2)))
        .mount_as_scoped(server)
        .await;
    let offer = provider.plan(&request("A6000")).await.unwrap().remove(0);
    drop(mock);
    offer
}

#[tokio::test]
async fn prepare_reuses_a_key_registered_under_any_name() {
    let (server, provider) = server().await;
    let offer = planned_offer(&server, &provider).await;
    Mock::given(method("GET"))
        .and(path("/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sshKeys": [
            {"id": "k1", "name": "laptop", "public_key": "ssh-rsa BBBB laptop"},
            {"id": "k2", "name": "my-gpuprov", "public_key": "ssh-ed25519 AAAAour renamed"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ssh-keys"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inventory_body(1)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(images_body()))
        .mount(&server)
        .await;
    let prepared = provider.prepare(&offer, &spec()).await.unwrap();
    assert_eq!(
        prepared,
        json!({"ssh_key_name": "my-gpuprov", "image_id": 11})
    );
}

#[tokio::test]
async fn prepare_registers_a_missing_key_and_honors_image_override() {
    let (server, provider) = server().await;
    let offer = planned_offer(&server, &provider).await;
    Mock::given(method("GET"))
        .and(path("/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sshKeys": []})))
        .mount(&server)
        .await;
    let name = key_name(OUR_KEY);
    Mock::given(method("POST"))
        .and(path("/ssh-keys"))
        .and(body_json(json!({"name": name, "publicKey": OUR_KEY})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"sshKey": {"id": "k9", "name": name}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inventory_body(1)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/images"))
        .respond_with(ResponseTemplate::new(200).set_body_json(images_body()))
        .mount(&server)
        .await;
    let mut spec = spec();
    spec.image_id = Some("Art".into());
    let prepared = provider.prepare(&offer, &spec).await.unwrap();
    assert_eq!(prepared, json!({"ssh_key_name": name, "image_id": 7}));
}

#[tokio::test]
async fn prepare_is_unavailable_when_capacity_is_gone() {
    let (server, provider) = server().await;
    let offer = planned_offer(&server, &provider).await;
    Mock::given(method("GET"))
        .and(path("/ssh-keys"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"sshKeys": [{"id": "k", "name": "n", "public_key": OUR_KEY}]}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gpu-inventory"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inventory_body(0)))
        .mount(&server)
        .await;
    let err = provider.prepare(&offer, &spec()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
}

#[tokio::test]
async fn create_sends_the_documented_launch_body() {
    let (server, provider) = server().await;
    let offer = planned_offer(&server, &provider).await;
    Mock::given(method("POST"))
        .and(path("/instance/launch"))
        .and(body_json(json!({
            "imageId": 11,
            "productName": "gpu_1x_a6000",
            "regionName": "any",
            "instanceName": "gpuprov-vm1",
            "sshKeys": ["my-gpuprov"]
        })))
        .respond_with(
            ResponseTemplate::new(202).set_body_json(json!({"response": "8b52a46b-uuid"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_name": "my-gpuprov", "image_id": 11});
    let uuid = provider.create(&offer, &prepared, &spec()).await.unwrap();
    assert_eq!(uuid, "8b52a46b-uuid");
}

#[tokio::test]
async fn create_without_stock_is_a_capacity_error() {
    let (server, provider) = server().await;
    let offer = planned_offer(&server, &provider).await;
    Mock::given(method("POST"))
        .and(path("/instance/launch"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "No Availability. Select a different quantity or GPU type."
        })))
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_name": "k", "image_id": 11});
    let err = provider
        .create(&offer, &prepared, &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity);
}

#[tokio::test]
async fn observe_matches_instances_by_name() {
    let (server, provider) = server().await;
    Mock::given(method("GET"))
        .and(path("/instance"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"runningInstances": [
                {"uuid": "b", "name": "gpuprov-vm1", "ip": "203.0.113.5", "username": "Ubuntu",
                 "status": "rented", "os_booted": 1, "active": 1},
                {"uuid": "a", "name": "gpuprov-vm1", "ip": "", "status": "booting", "os_booted": 0},
                {"uuid": "c", "name": "gpuprov-vm10", "ip": "203.0.113.6", "status": "rented",
                 "os_booted": 1, "active": 1},
                {"uuid": "d", "name": "someone-else", "status": "rented"}
            ]})),
        )
        .mount(&server)
        .await;
    let at = Location {
        region: "any".into(),
        zone: None,
    };
    let observed = provider.observe(&at, "vm1").await.unwrap();
    let ids: Vec<&str> = observed.iter().map(|o| o.instance_id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);
    assert!(observed[1].is_ready());
}

#[tokio::test]
async fn delete_terminates_and_treats_unknown_instances_as_gone() {
    let (server, provider) = server().await;
    let at = Location {
        region: "any".into(),
        zone: None,
    };
    let terminate = Mock::given(method("POST"))
        .and(path("/instance/terminate"))
        .and(body_json(json!({"instanceUuids": ["u1", "u2"]})))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"response": {"data": {}}})))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    provider
        .delete(&at, "vm1", &["u1".into(), "u2".into()])
        .await
        .unwrap();
    drop(terminate);

    let gone = Mock::given(method("POST"))
        .and(path("/instance/terminate"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"message": "Instance does not exist"})),
        )
        .mount_as_scoped(&server)
        .await;
    provider.delete(&at, "vm1", &["u1".into()]).await.unwrap();
    drop(gone);

    Mock::given(method("POST"))
        .and(path("/instance/terminate"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = provider
        .delete(&at, "vm1", &["u1".into()])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Transient);

    // Nothing to delete: no request at all.
    provider.delete(&at, "vm1", &[]).await.unwrap();
}
