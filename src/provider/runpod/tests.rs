//! RunPod adapter tests: pure helpers, plus each HTTP primitive against a
//! `wiremock` server standing in for the REST API v2.

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{ErrorKind, ResourceAsk};

const OUR_KEY: &str = "ssh-ed25519 AAAAour gpuprov";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn reads_cli_credentials_file_in_both_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let profiled = dir.path().join("profiled.toml");
    std::fs::write(&profiled, "[default]\napi_key = \"rp-profile\"\n").unwrap();
    assert_eq!(read_cli_api_key(&profiled).as_deref(), Some("rp-profile"));

    let flat = dir.path().join("flat.toml");
    std::fs::write(&flat, "api_key = \"rp-flat\"\n").unwrap();
    assert_eq!(read_cli_api_key(&flat).as_deref(), Some("rp-flat"));

    assert!(read_cli_api_key(&dir.path().join("missing.toml")).is_none());
}

#[test]
fn merged_keys_put_ours_first_and_dedupe_by_material() {
    let merged = merge_keys(
        OUR_KEY,
        &[
            "ssh-ed25519 AAAAour other-comment".into(),
            "ssh-rsa BBBBuser laptop".into(),
            "  ".into(),
        ],
    );
    assert_eq!(
        merged,
        "ssh-ed25519 AAAAour gpuprov\nssh-rsa BBBBuser laptop"
    );
}

fn pod(value: serde_json::Value) -> Pod {
    serde_json::from_value(value).unwrap()
}

#[test]
fn pod_status_and_endpoint_mapping() {
    let running = pod(json!({
        "id": "p1", "name": "gpuprov-vm1", "status": "RUNNING", "dataCenterId": "US-TX-3",
        "ssh": {"direct": {"host": "203.0.113.9", "port": 40123, "username": "root",
                           "command": "ssh root@203.0.113.9 -p 40123"},
                "proxy": null}
    }));
    let o = to_observed(&running).unwrap();
    assert!(o.is_ready());
    assert_eq!(o.zone.as_deref(), Some("US-TX-3"));
    assert_eq!(
        o.endpoint.unwrap(),
        SshEndpoint {
            host: "203.0.113.9".into(),
            port: 40123,
            user: Some("root".into()),
        }
    );

    // `ssh.direct` not populated yet: fall back to the live 22/tcp mapping.
    let via_runtime = pod(json!({
        "id": "p2", "name": "x", "status": "RUNNING", "ssh": {"direct": null, "proxy": null},
        "runtime": {"ports": [
            {"private": 8888, "public": 1, "type": "http", "ip": "1.1.1.1"},
            {"private": 22, "public": 40999, "type": "tcp", "ip": "198.51.100.4"}
        ]}
    }));
    let endpoint = to_observed(&via_runtime).unwrap().endpoint.unwrap();
    assert_eq!(
        (endpoint.host.as_str(), endpoint.port),
        ("198.51.100.4", 40999)
    );

    // Running but no port assigned yet: not ready, so the core keeps polling.
    let unmapped = pod(json!({
        "id": "p3", "name": "x", "status": "RUNNING",
        "runtime": {"ports": [{"private": 22, "public": null, "ip": null}]}
    }));
    assert!(!to_observed(&unmapped).unwrap().is_ready());

    for (status, expected) in [
        ("PROVISIONING", InstanceStatus::Pending),
        ("STARTING", InstanceStatus::Pending),
        ("EXITED", InstanceStatus::Failed("container exited".into())),
        ("ERROR", InstanceStatus::Failed("pod error".into())),
    ] {
        let p = pod(json!({"id": "p", "name": "x", "status": status}));
        let o = to_observed(&p).unwrap();
        assert_eq!(o.status, expected, "{status}");
        assert!(o.endpoint.is_none(), "{status}: endpoint only when RUNNING");
    }
    assert!(to_observed(&pod(
        json!({"id": "p", "name": "x", "status": "TERMINATED"})
    ))
    .is_none());
}

#[test]
fn capabilities_reject_ports_spot_and_kubernetes_but_allow_images() {
    let caps = RunpodProvider::with_api("http://localhost", "k", DEFAULT_IMAGE)
        .unwrap()
        .capabilities();
    for feature in [Feature::OpenPorts, Feature::Spot, Feature::Kubernetes] {
        assert!(!caps.supports(feature), "{feature}");
    }
    assert!(caps.supports(Feature::ImageOverride));
    assert!(!caps.boot_script);
}

// ---------------------------------------------------------------------------
// HTTP primitives against a mock server
// ---------------------------------------------------------------------------

async fn provider(server: &MockServer) -> RunpodProvider {
    RunpodProvider::with_api(&server.uri(), "rp-test", "img:test").unwrap()
}

fn gpu_type(id: &str, name: &str, secure: f64, dcs: serde_json::Value) -> serde_json::Value {
    json!({
        "id": id, "name": name, "pool": null, "manufacturer": "NVIDIA", "memory": 80,
        "secure": true, "community": true,
        "price": {"secure": secure, "community": secure / 2.0},
        "maxCount": {"secure": 8, "community": 8},
        "dataCenters": dcs
    })
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        vm_name: "vm1".into(),
        image_id: None,
        disk_gb: 120,
        spot: false,
        open_ports: Vec::new(),
        user_data: String::new(),
        ssh_public_key: OUR_KEY.into(),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

fn h100_offer() -> Offer {
    let mut offer = Offer::new("NVIDIA H100 80GB HBM3 x1 secure", "north_america")
        .with_zone(Some("US-TX-3".into()));
    offer.detail = json!({
        "gpu_id": "NVIDIA H100 80GB HBM3", "count": 1, "cloud": "SECURE",
        "data_center_id": "US-TX-3", "memory_gb": 80
    });
    offer
}

#[tokio::test]
async fn plan_queries_each_tier_and_maps_data_centers() {
    let server = MockServer::start().await;
    for (cloud, dc_stock) in [("SECURE", "HIGH"), ("COMMUNITY", "LOW")] {
        Mock::given(method("GET"))
            .and(path("/v2/catalog/gpus"))
            .and(header("authorization", "Bearer rp-test"))
            .and(query_param("include", "AVAILABILITY"))
            .and(query_param("product", "POD"))
            .and(query_param("count", "1"))
            .and(query_param("cloud", cloud))
            .and(query_param("countryCodes", "US"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"gpus": [
                gpu_type("NVIDIA H100 80GB HBM3", "H100 SXM", 2.99,
                         json!([{"id": "US-TX-3", "name": "Texas", "availability": dc_stock}])),
                gpu_type("NVIDIA A40", "A40", 0.4,
                         json!([{"id": "US-TX-3", "name": "Texas", "availability": "HIGH"}])),
            ]})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/v2/catalog/datacenters"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"dataCenters": [
                {"id": "US-TX-3", "name": "Texas", "region": "NORTH_AMERICA",
                 "networkVolumeTypes": [], "compliance": [], "globalNetwork": false}
            ]})),
        )
        .mount(&server)
        .await;

    let offers = provider(&server)
        .await
        .plan(&ResourceRequest {
            ask: ResourceAsk::Gpus("H100:1".parse().unwrap()),
            regions: vec!["US".into()],
            zone: None,
            spot: false,
        })
        .await
        .unwrap();
    let rendered: Vec<String> = offers.iter().map(|o| o.to_string()).collect();
    assert_eq!(
        rendered,
        vec![
            "NVIDIA H100 80GB HBM3 x1 community@north_america/US-TX-3 ($1.50/h)",
            "NVIDIA H100 80GB HBM3 x1 secure@north_america/US-TX-3 ($2.99/h)",
        ]
    );
}

#[tokio::test]
async fn plan_rejected_query_is_invalid_and_401_is_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/catalog/gpus"))
        .and(query_param("cloud", "SECURE"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "title": "Unprocessable Entity", "status": 422, "detail": "invalid countryCodes"
        })))
        .mount(&server)
        .await;
    let request = ResourceRequest {
        ask: ResourceAsk::Gpus("H100:1".parse().unwrap()),
        regions: vec!["ZZ".into()],
        zone: None,
        spot: false,
    };
    let err = provider(&server).await.plan(&request).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    assert!(err.msg.contains("invalid countryCodes"), "{}", err.msg);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "title": "Unauthorized", "status": 401, "detail": "invalid api key"
        })))
        .mount(&server)
        .await;
    let err = provider(&server).await.plan(&request).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
}

async fn mount_stock(server: &MockServer, availability: &str) {
    Mock::given(method("GET"))
        .and(path("/v2/catalog/gpus/NVIDIA%20H100%2080GB%20HBM3"))
        .and(query_param("include", "AVAILABILITY"))
        .and(query_param("cloud", "SECURE"))
        .and(query_param("count", "1"))
        .and(query_param_is_missing("countryCodes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gpu_type(
            "NVIDIA H100 80GB HBM3",
            "H100 SXM",
            2.99,
            json!([{"id": "US-TX-3", "name": "Texas", "availability": availability}]),
        )))
        .mount(server)
        .await;
}

#[tokio::test]
async fn prepare_merges_account_keys_and_rechecks_stock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/account/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": ["ssh-rsa BBBBuser laptop", "ssh-ed25519 AAAAour dup"]
        })))
        .mount(&server)
        .await;
    mount_stock(&server, "LOW").await;
    // The account's keys are only read, never replaced.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let prepared = provider(&server)
        .await
        .prepare(&h100_offer(), &spec())
        .await
        .unwrap();
    assert_eq!(
        prepared["public_key"],
        "ssh-ed25519 AAAAour gpuprov\nssh-rsa BBBBuser laptop"
    );
}

#[tokio::test]
async fn prepare_tolerates_unreadable_keys_and_fails_cleanly_when_sold_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/account/ssh-keys"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "title": "Forbidden", "status": 403, "detail": "key lacks account scope"
        })))
        .mount(&server)
        .await;
    mount_stock(&server, "HIGH").await;
    let prepared = provider(&server)
        .await
        .prepare(&h100_offer(), &spec())
        .await
        .unwrap();
    assert_eq!(prepared["public_key"], OUR_KEY);

    let server = MockServer::start().await;
    mount_stock(&server, "NONE").await;
    let err = provider(&server)
        .await
        .prepare(&h100_offer(), &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable, "{err}");
}

#[tokio::test]
async fn create_posts_the_exact_pod_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/pods"))
        .and(body_json(json!({
            "name": "gpuprov-vm1",
            "image": "img:test",
            "disk": 120,
            "ports": ["22/tcp"],
            "env": {"PUBLIC_KEY": OUR_KEY},
            "startSsh": true,
            "cloud": "SECURE",
            "dataCenterIds": ["US-TX-3"],
            "gpu": {"id": "NVIDIA H100 80GB HBM3", "count": 1}
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "pod-123"})))
        .expect(1)
        .mount(&server)
        .await;
    let id = provider(&server)
        .await
        .create(&h100_offer(), &json!({"public_key": OUR_KEY}), &spec())
        .await
        .unwrap();
    assert_eq!(id, "pod-123");
}

#[tokio::test]
async fn create_maps_stockouts_to_capacity_and_honors_image_override() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/pods"))
        .and(wiremock::matchers::body_partial_json(json!({"image": "my/image:1"})))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "title": "Conflict", "status": 409,
            "detail": "There are no longer any instances available with the requested specifications."
        })))
        .mount(&server)
        .await;
    let mut spec = spec();
    spec.image_id = Some("my/image:1".into());
    let err = provider(&server)
        .await
        .create(&h100_offer(), &json!({"public_key": OUR_KEY}), &spec)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity, "{err}");
}

#[tokio::test]
async fn observe_follows_the_cursor_and_filters_by_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/pods"))
        .and(query_param_is_missing("cursor"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pods": [
                {"id": "other", "name": "someone-else", "status": "RUNNING"},
                {"id": "old", "name": "gpuprov-vm1", "status": "TERMINATED"}
            ],
            "pagination": {"nextCursor": "c2", "hasNextPage": true}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/pods"))
        .and(query_param("cursor", "c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pods": [{
                "id": "pod-123", "name": "gpuprov-vm1", "status": "RUNNING",
                "dataCenterId": "US-TX-3",
                "ssh": {"direct": {"host": "203.0.113.9", "port": 40123, "username": "root",
                                   "command": "ssh"}, "proxy": null}
            }],
            "pagination": {"nextCursor": null, "hasNextPage": false}
        })))
        .mount(&server)
        .await;
    let observed = provider(&server)
        .await
        .observe(
            &Location {
                region: "anywhere".into(),
                zone: None,
            },
            "vm1",
        )
        .await
        .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].instance_id, "pod-123");
    assert_eq!(observed[0].endpoint.as_ref().unwrap().port, 40123);
}

#[tokio::test]
async fn delete_treats_missing_pods_as_gone_and_conflicts_as_transient() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v2/pods/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "title": "Not Found", "status": 404, "detail": "pod not found"
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/pods/ok"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v2/pods/busy"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "title": "Conflict", "status": 409, "detail": "pod is starting"
        })))
        .mount(&server)
        .await;
    let provider = provider(&server).await;
    let at = Location {
        region: "x".into(),
        zone: None,
    };
    provider
        .delete(&at, "vm1", &["gone".into(), "ok".into()])
        .await
        .unwrap();
    let err = provider
        .delete(&at, "vm1", &["busy".into()])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Transient);
}
