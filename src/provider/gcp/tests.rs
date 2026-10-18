//! HTTP-level tests of the GCP adapter against a local wiremock server.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{ErrorKind, GpuRequest, InstanceStatus};

const PROJECT: &str = "proj";

async fn setup() -> (MockServer, GcpProvider) {
    let server = MockServer::start().await;
    let provider = GcpProvider::with_endpoint(&server.uri(), PROJECT, "tok");
    (server, provider)
}

fn p(rest: &str) -> String {
    format!("/projects/{PROJECT}/{rest}")
}

fn spec() -> InstanceSpec {
    InstanceSpec {
        vm_name: "dev".into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: Vec::new(),
        user_data: "#cloud-config\n".into(),
        ssh_public_key: "ssh-ed25519 AAAA".into(),
        ssh_user: "ubuntu".into(),
    }
}

fn request(ask: ResourceAsk, regions: &[&str], zone: Option<&str>) -> ResourceRequest {
    ResourceRequest {
        ask,
        regions: regions.iter().map(|r| r.to_string()).collect(),
        zone: zone.map(str::to_string),
        spot: false,
    }
}

fn zonal_offer(machine_type: &str, zone: &str) -> Offer {
    Offer::new(machine_type, catalog::region_of_zone(zone).unwrap()).with_zone(Some(zone.into()))
}

async fn mock_region(server: &MockServer, region: &str, zones: &[&str]) {
    let urls: Vec<String> = zones
        .iter()
        .map(|z| format!("https://compute.googleapis.com/compute/v1/projects/{PROJECT}/zones/{z}"))
        .collect();
    Mock::given(method("GET"))
        .and(path(p(&format!("regions/{region}"))))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": region, "zones": urls})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn plan_offers_only_zones_that_offer_the_type() {
    let (server, provider) = setup().await;
    mock_region(&server, "us-central1", &["us-central1-a", "us-central1-b"]).await;
    // Every G2 type exists only in us-central1-b; -a returns a warning scope.
    Mock::given(method("GET"))
        .and(path(p("aggregated/machineTypes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": {
            "zones/us-central1-a": {"warning": {"code": "NO_RESULTS_ON_PAGE"}},
            "zones/us-central1-b": {"machineTypes": [{"name": "g2-standard-4"}]},
        }})))
        .mount(&server)
        .await;
    let ask = ResourceAsk::Gpus(GpuRequest {
        name: "L4".into(),
        count: 1,
    });
    let offers = provider
        .plan(&request(ask, &["us-central1"], None))
        .await
        .unwrap();
    assert!(!offers.is_empty());
    assert!(offers
        .iter()
        .all(|o| o.zone.as_deref() == Some("us-central1-b")));
    assert_eq!(offers[0].instance_type, "g2-standard-4");
    assert!(catalog::choice_of(&offers[0]).gpu);
}

#[tokio::test]
async fn plan_t4_intersects_machine_and_accelerator_zones() {
    let (server, provider) = setup().await;
    mock_region(&server, "us-west1", &["us-west1-a", "us-west1-b"]).await;
    Mock::given(method("GET"))
        .and(path(p("aggregated/machineTypes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": {
            "zones/us-west1-a": {"machineTypes": [{"name": "n1"}]},
            "zones/us-west1-b": {"machineTypes": [{"name": "n1"}]},
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(p("aggregated/acceleratorTypes")))
        .and(query_param("filter", "name = \"nvidia-tesla-t4\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": {
            "zones/us-west1-b": {"acceleratorTypes": [{"name": "nvidia-tesla-t4"}]},
        }})))
        .mount(&server)
        .await;
    let ask = ResourceAsk::Gpus(GpuRequest {
        name: "T4".into(),
        count: 1,
    });
    let offers = provider
        .plan(&request(ask, &["us-west1"], None))
        .await
        .unwrap();
    assert!(offers
        .iter()
        .all(|o| o.zone.as_deref() == Some("us-west1-b")));
    assert!(catalog::choice_of(&offers[0]).accelerator.is_some());
}

#[tokio::test]
async fn plan_rejects_unknown_region_empty_regions_and_orphan_zone() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("regions/mars1")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": {
            "code": 404, "message": "The resource 'regions/mars1' was not found",
            "errors": [{"reason": "notFound"}]}})))
        .mount(&server)
        .await;
    let ask = || ResourceAsk::InstanceType("e2-medium".into());
    let err = provider
        .plan(&request(ask(), &["mars1"], None))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    assert!(err.msg.contains("mars1"));
    let err = provider.plan(&request(ask(), &[], None)).await.unwrap_err();
    assert!(err.msg.contains("--region"));
    let err = provider
        .plan(&request(ask(), &["us-central1"], Some("europe-west4-a")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
}

#[tokio::test]
async fn prepare_creates_missing_firewall_and_resolves_image() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("global/firewalls/gpuprov-dev")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": 404, "message": "nf"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("global/firewalls")))
        .and(body_partial_json(
            json!({"name": "gpuprov-dev", "targetTags": ["gpuprov-dev"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "op-fw", "status": "RUNNING"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("global/operations/op-fw/wait")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "op-fw", "status": "DONE"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2204-nvidia-580",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"selfLink": "https://x/images/dlvm-123"})))
        .mount(&server)
        .await;
    let prepared = provider
        .prepare(&zonal_offer("g2-standard-4", "us-central1-a"), &spec())
        .await
        .unwrap();
    assert_eq!(prepared["image"], "https://x/images/dlvm-123");
    assert_eq!(prepared["network_tag"], "gpuprov-dev");
}

#[tokio::test]
async fn prepare_keeps_matching_firewall_and_reports_missing_image() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("global/firewalls/gpuprov-dev")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "gpuprov-dev", "allowed": [{"IPProtocol": "tcp", "ports": ["22"]}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("global/firewalls")))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/projects/ubuntu-os-cloud/global/images/family/ubuntu-2204-lts",
        ))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": 404, "message": "nf"}})),
        )
        .mount(&server)
        .await;
    let err = provider
        .prepare(&zonal_offer("e2-medium", "us-central1-a"), &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    assert!(err.msg.contains("ubuntu-2204-lts"));
}

fn prepared() -> Value {
    json!({"image": "https://x/images/img", "network_tag": "gpuprov-dev"})
}

#[tokio::test]
async fn create_inserts_and_waits_for_the_operation() {
    let (server, provider) = setup().await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-a/instances")))
        .and(body_partial_json(
            json!({"name": "gpuprov-dev", "labels": {"gpuprov-vm-name": "dev"}}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "op-1", "status": "RUNNING"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-a/operations/op-1/wait")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "op-1", "status": "DONE"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let id = provider
        .create(
            &zonal_offer("g2-standard-4", "us-central1-a"),
            &prepared(),
            &spec(),
        )
        .await
        .unwrap();
    assert_eq!(id, "gpuprov-dev");
}

#[tokio::test]
async fn create_stockout_in_operation_is_capacity() {
    let (server, provider) = setup().await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-a/instances")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "op-2", "status": "PENDING"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-a/operations/op-2/wait")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "op-2", "status": "DONE",
            "error": {"errors": [{"code": "ZONE_RESOURCE_POOL_EXHAUSTED",
                                  "message": "The zone does not have enough resources"}]}})))
        .mount(&server)
        .await;
    let err = provider
        .create(
            &zonal_offer("g2-standard-4", "us-central1-a"),
            &prepared(),
            &spec(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity);
    assert!(err.msg.contains("ZONE_RESOURCE_POOL_EXHAUSTED"));
}

#[tokio::test]
async fn create_quota_and_conflict() {
    let (server, provider) = setup().await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-a/instances")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403, "message": "Quota 'GPUS_ALL_REGIONS' exceeded.",
            "errors": [{"reason": "quotaExceeded"}]}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(p("zones/us-central1-b/instances")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": {
            "code": 409, "message": "already exists", "errors": [{"reason": "alreadyExists"}]}})))
        .mount(&server)
        .await;
    let err = provider
        .create(
            &zonal_offer("g2-standard-4", "us-central1-a"),
            &prepared(),
            &spec(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Quota);
    // An insert that already landed is idempotent success.
    let id = provider
        .create(
            &zonal_offer("g2-standard-4", "us-central1-b"),
            &prepared(),
            &spec(),
        )
        .await
        .unwrap();
    assert_eq!(id, "gpuprov-dev");
}

fn instance_json(zone: &str, status: &str, nat: Option<&str>) -> Value {
    let mut nic = json!({"networkIP": "10.0.0.5", "accessConfigs": [{}]});
    if let Some(nat) = nat {
        nic["accessConfigs"][0]["natIP"] = json!(nat);
    }
    json!({
        "name": "gpuprov-dev",
        "status": status,
        "zone": format!("https://compute.googleapis.com/compute/v1/projects/{PROJECT}/zones/{zone}"),
        "networkInterfaces": [nic],
    })
}

#[tokio::test]
async fn observe_zonal_list_filters_by_label() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("zones/us-central1-a/instances")))
        .and(query_param("filter", "labels.gpuprov-vm-name = \"dev\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [instance_json("us-central1-a", "RUNNING", Some("34.0.0.1"))]})))
        .mount(&server)
        .await;
    let at = Location {
        region: "us-central1".into(),
        zone: Some("us-central1-a".into()),
    };
    let observed = provider.observe(&at, "dev").await.unwrap();
    assert_eq!(observed.len(), 1);
    assert!(observed[0].is_ready());
    assert_eq!(observed[0].endpoint.as_ref().unwrap().host, "34.0.0.1");
}

#[tokio::test]
async fn observe_region_level_uses_aggregated_list_scoped_to_region() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("aggregated/instances")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": {
            "zones/us-central1-c": {"instances": [instance_json("us-central1-c", "STAGING", None)]},
            "zones/us-east1-b": {"instances": [instance_json("us-east1-b", "RUNNING", Some("1.1.1.1"))]},
            "zones/us-central1-a": {"warning": {"code": "NO_RESULTS_ON_PAGE"}},
        }})))
        .mount(&server)
        .await;
    let at = Location {
        region: "us-central1".into(),
        zone: None,
    };
    let observed = provider.observe(&at, "dev").await.unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].status, InstanceStatus::Pending);
    assert_eq!(observed[0].zone.as_deref(), Some("us-central1-c"));
}

#[tokio::test]
async fn delete_is_idempotent_and_cleanup_tolerates_missing_rule() {
    let (server, provider) = setup().await;
    Mock::given(method("DELETE"))
        .and(path(p("zones/us-central1-a/instances/gpuprov-dev")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": 404, "message": "nf"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(p("global/firewalls/gpuprov-dev")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": 404, "message": "nf"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let at = Location {
        region: "us-central1".into(),
        zone: Some("us-central1-a".into()),
    };
    provider
        .delete(&at, "dev", &["gpuprov-dev".to_string()])
        .await
        .unwrap();
    provider.cleanup(&at, "dev").await.unwrap();
}

#[tokio::test]
async fn delete_without_zone_looks_instances_up_first() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("aggregated/instances")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": {
            "zones/us-central1-f": {"instances": [instance_json("us-central1-f", "RUNNING", None)]},
        }})))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(p("zones/us-central1-f/instances/gpuprov-dev")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "op-d"})))
        .expect(1)
        .mount(&server)
        .await;
    let at = Location {
        region: "us-central1".into(),
        zone: None,
    };
    provider
        .delete(&at, "dev", &["gpuprov-dev".to_string(), "gone".to_string()])
        .await
        .unwrap();
}

#[tokio::test]
async fn auth_failures_are_credentials_errors() {
    let (server, provider) = setup().await;
    Mock::given(method("GET"))
        .and(path(p("zones/us-central1-a/instances")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {
            "code": 401, "message": "Request had invalid authentication credentials."}})))
        .mount(&server)
        .await;
    let at = Location {
        region: "us-central1".into(),
        zone: Some("us-central1-a".into()),
    };
    let err = provider.observe(&at, "dev").await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
}
