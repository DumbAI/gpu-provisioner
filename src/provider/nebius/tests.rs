//! Nebius adapter tests: pure helpers, plus each HTTP primitive against a
//! `wiremock` server standing in for the REST gateway.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{BootScript, ErrorKind, GpuRequest, ResourceAsk, Severity};

const TOKEN: &str = "nebius-token";
const TENANT: &str = "tenant-e00stub";
const NORTH: &str = "project-e00north";
const CENTRAL: &str = "project-e00central";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn instance(value: serde_json::Value) -> Instance {
    serde_json::from_value(value).unwrap()
}

fn base_instance(state: &str) -> serde_json::Value {
    json!({
        "metadata": {"id": "computeinstance-e00dev", "parentId": NORTH, "name": "gpuprov-dev",
                     "labels": {"gpuprov-vm-name": "dev"}},
        "spec": {"resources": {"platform": "gpu-h100-sxm", "preset": "1gpu-16vcpu-200gb"}},
        "status": {"state": state, "reconciling": false, "networkInterfaces": [{
            "name": "eth0", "ipAddress": {"address": "192.168.0.5/32"},
            "publicIpAddress": {"address": "203.0.113.5/32"}
        }]}
    })
}

fn spec(spot: bool) -> InstanceSpec {
    InstanceSpec {
        vm_name: "dev".into(),
        image_id: None,
        disk_gb: 200,
        spot,
        open_ports: vec![],
        user_data: "#cloud-config\nusers: []\n".into(),
        ssh_public_key: "ssh-ed25519 AAAA key".into(),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

fn detail() -> OfferDetail {
    OfferDetail {
        project_id: NORTH.into(),
        region: "eu-north1".into(),
        platform: "gpu-h100-sxm".into(),
        preset: "1gpu-16vcpu-200gb".into(),
        image_family: catalog::IMAGE_GPU.into(),
    }
}

fn offer() -> Offer {
    let mut offer = Offer::new("gpu-h100-sxm/1gpu-16vcpu-200gb", "eu-north1");
    offer.detail = serde_json::to_value(detail()).unwrap();
    offer
}

#[test]
fn instance_states_map_to_the_core_status() {
    let running = to_observed(&instance(base_instance("RUNNING"))).unwrap();
    assert!(running.is_ready());
    assert_eq!(running.instance_id, "computeinstance-e00dev");
    assert_eq!(
        running.endpoint,
        Some(SshEndpoint {
            host: "203.0.113.5".into(),
            port: 22,
            user: None,
        })
    );
    assert_eq!(running.private_ip.as_deref(), Some("192.168.0.5"));

    for (state, expected) in [
        ("CREATING", InstanceStatus::Pending),
        ("STARTING", InstanceStatus::Pending),
        ("UPDATING", InstanceStatus::Pending),
        ("STOPPING", InstanceStatus::Stopping),
        ("STOPPED", InstanceStatus::Stopped),
        ("DELETING", InstanceStatus::Terminating),
    ] {
        let o = to_observed(&instance(base_instance(state))).unwrap();
        assert_eq!(o.status, expected, "{state}");
        assert!(o.endpoint.is_none(), "{state} has no endpoint");
    }
    assert!(matches!(
        to_observed(&instance(base_instance("ERROR")))
            .unwrap()
            .status,
        InstanceStatus::Failed(_)
    ));
    // A brand-new VM is STOPPED while its create operation still runs.
    let mut creating = base_instance("STOPPED");
    creating["status"]["reconciling"] = json!(true);
    assert_eq!(
        to_observed(&instance(creating)).unwrap().status,
        InstanceStatus::Pending
    );
}

#[test]
fn preemption_stops_and_maintenance_become_findings() {
    let mut preempted = base_instance("STOPPED");
    preempted["spec"]["preemptible"] = json!({"onPreemption": "STOP"});
    let findings = findings_for(&instance(preempted.clone()));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Critical);
    assert!(findings[0].message.starts_with("preempted"), "{findings:?}");

    let mut spot_running = preempted;
    spot_running["status"]["state"] = json!("RUNNING");
    spot_running["status"]["maintenanceEventId"] = json!("computemaintenance-e00m");
    let findings = findings_for(&instance(spot_running));
    let severities: Vec<Severity> = findings.iter().map(|f| f.severity).collect();
    assert_eq!(severities, [Severity::Info, Severity::Warn]);
    assert!(findings[1].message.contains("computemaintenance-e00m"));

    let stopped = findings_for(&instance(base_instance("STOPPED")));
    assert_eq!(stopped[0].severity, Severity::Critical);
    assert!(stopped[0].message.starts_with("stopped"));
    assert!(findings_for(&instance(base_instance("RUNNING"))).is_empty());
}

#[test]
fn diagnosis_picks_the_recorded_instance() {
    let mut other = base_instance("RUNNING");
    other["metadata"]["id"] = json!("computeinstance-e00other");
    let instances = [instance(base_instance("STOPPED")), instance(other)];
    let diagnosis = diagnosis_from(&instances, Some("computeinstance-e00dev"));
    assert_eq!(
        diagnosis.instance.unwrap().instance_id,
        "computeinstance-e00dev"
    );
    assert_eq!(diagnosis.findings.len(), 1);
    assert!(diagnosis_from(&[], Some("x")).instance.is_none());
}

#[test]
fn projects_map_by_region_deterministically() {
    let projects: Vec<api::Container> = serde_json::from_value(json!([
        {"metadata": {"id": "project-e00b", "parentId": TENANT, "name": "default-project-eu-north1"},
         "spec": {"region": "eu-north1"}, "status": {"containerState": "ACTIVE", "region": "eu-north1"}},
        {"metadata": {"id": "project-e00a", "parentId": TENANT, "name": "second"},
         "spec": {}, "status": {"containerState": "ACTIVE", "region": "eu-north1"}},
        {"metadata": {"id": "project-e00c", "parentId": TENANT},
         "spec": {"region": "us-central1"}, "status": {"containerState": "SCHEDULED_FOR_DELETION"}},
        {"metadata": {"id": "project-e00d", "parentId": TENANT},
         "spec": {"region": "me-west1"}}
    ]))
    .unwrap();
    let by_region = projects_by_region(&projects);
    assert_eq!(
        by_region,
        BTreeMap::from([
            ("eu-north1".to_string(), "project-e00a".to_string()),
            ("me-west1".to_string(), "project-e00d".to_string()),
        ])
    );
}

#[test]
fn region_selection_filters_and_reports_unknown_regions() {
    let projects = BTreeMap::from([
        ("eu-north1".to_string(), NORTH.to_string()),
        ("us-central1".to_string(), CENTRAL.to_string()),
    ]);
    let mut request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H200".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    };
    assert_eq!(select_regions(&projects, &request).unwrap().len(), 2);
    request.regions = vec!["US-CENTRAL1".into(), "uk-south1".into()];
    assert_eq!(
        select_regions(&projects, &request)
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["us-central1"]
    );
    request.regions = vec!["uk-south1".into()];
    let err = select_regions(&projects, &request).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    assert!(err.msg.contains("eu-north1, us-central1"), "{}", err.msg);
    request.regions.clear();
    request.zone = Some("eu-north1".into());
    assert_eq!(select_regions(&projects, &request).unwrap().len(), 1);
}

#[test]
fn create_request_carries_identity_disk_network_and_spot() {
    let prepared = Prepared {
        subnet_id: "vpcsubnet-e00s".into(),
        image_family: Some(catalog::IMAGE_GPU.into()),
        image_id: None,
    };
    let mut with_boot = spec(true);
    with_boot.boot_script = Some(BootScript {
        script: "#!/usr/bin/env bash\necho hi\n".into(),
        env: vec![],
    });
    let body = serde_json::to_value(create_request(&detail(), &prepared, &with_boot)).unwrap();
    assert_eq!(body["metadata"]["parentId"], NORTH);
    assert_eq!(body["metadata"]["name"], "gpuprov-dev");
    assert_eq!(body["metadata"]["labels"][LABEL_VM], "dev");
    assert_eq!(
        body["spec"]["resources"],
        json!({"platform": "gpu-h100-sxm", "preset": "1gpu-16vcpu-200gb"})
    );
    let disk = &body["spec"]["bootDisk"]["managedDisk"];
    assert_eq!(disk["name"], "gpuprov-dev-boot");
    assert_eq!(disk["labels"][LABEL_VM], "dev");
    assert_eq!(disk["spec"]["sizeGibibytes"], "200");
    assert_eq!(
        disk["spec"]["sourceImageFamily"]["imageFamily"],
        catalog::IMAGE_GPU
    );
    let nic = &body["spec"]["networkInterfaces"][0];
    assert_eq!(nic["subnetId"], "vpcsubnet-e00s");
    assert_eq!(nic["publicIpAddress"], json!({}));
    assert!(body["spec"]["cloudInitUserData"]
        .as_str()
        .unwrap()
        .contains("echo hi"));
    assert_eq!(body["spec"]["preemptible"]["onPreemption"], "STOP");
    assert_eq!(body["spec"]["recoveryPolicy"], "FAIL");

    let on_demand =
        serde_json::to_value(create_request(&detail(), &prepared, &spec(false))).unwrap();
    for key in ["preemptible", "recoveryPolicy", "followsSpotPrice"] {
        assert!(on_demand["spec"].get(key).is_none(), "{key}");
    }
}

#[test]
fn image_overrides_are_ids_or_families() {
    assert_eq!(
        image_override("computeimage-e00abc"),
        (None, Some("computeimage-e00abc".into()))
    );
    assert_eq!(
        image_override("ubuntu24.04-cuda12"),
        (Some("ubuntu24.04-cuda12".into()), None)
    );
}

// ---------------------------------------------------------------------------
// HTTP primitives
// ---------------------------------------------------------------------------

async fn server() -> (MockServer, NebiusProvider) {
    let server = MockServer::start().await;
    let provider = NebiusProvider::with_api(&server.uri(), TOKEN, None, BTreeMap::new()).unwrap();
    (server, provider)
}

fn pinned(server: &MockServer) -> NebiusProvider {
    NebiusProvider::with_api(
        &server.uri(),
        TOKEN,
        Some(TENANT),
        BTreeMap::from([("eu-north1".to_string(), NORTH.to_string())]),
    )
    .unwrap()
}

fn auth() -> wiremock::matchers::HeaderExactMatcher {
    header("authorization", format!("Bearer {TOKEN}").as_str())
}

async fn mount_discovery(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/iam/v1/tenants"))
        .and(auth())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"metadata": {"id": TENANT, "name": "acme"}, "spec": {}, "status": {"containerState": "ACTIVE"}}]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/iam/v1/projects"))
        .and(query_param("parentId", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {"metadata": {"id": NORTH, "parentId": TENANT}, "spec": {"region": "eu-north1"},
                 "status": {"containerState": "ACTIVE", "region": "eu-north1"}},
                {"metadata": {"id": CENTRAL, "parentId": TENANT}, "spec": {"region": "us-central1"},
                 "status": {"containerState": "ACTIVE", "region": "us-central1"}}
            ]
        })))
        .mount(server)
        .await;
}

fn platforms(names: &[&str]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            json!({
                "metadata": {"id": format!("computeplatform-{name}"), "name": name},
                "spec": {"humanReadableName": format!("NVIDIA® {} NVLink", name.split('-').nth(1).unwrap().to_uppercase()),
                         "presets": [{"name": "1gpu-16vcpu-200gb",
                                      "resources": {"vcpuCount": 16, "memoryGibibytes": 200, "gpuCount": 1}}]},
                "status": {"allowedForPreemptibles": true}
            })
        })
        .collect();
    json!({"items": items})
}

#[tokio::test]
async fn plan_discovers_projects_prices_and_consults_the_advisor() {
    let (server, provider) = server().await;
    mount_discovery(&server).await;
    for (project, names) in [
        (NORTH, vec!["gpu-h100-sxm", "gpu-h200-sxm"]),
        (CENTRAL, vec!["gpu-h200-sxm"]),
    ] {
        Mock::given(method("GET"))
            .and(path("/compute/v1/platforms"))
            .and(query_param("parentId", project))
            .respond_with(ResponseTemplate::new(200).set_body_json(platforms(&names)))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/billing/v1/calculator/estimate-batch"))
        .and(body_partial_json(json!({
            "filterAggregationUnit": {"filterAggregationUnitValues": ["FILTER_AGGREGATION_UNIT_HOUR"]}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "currency": "usd",
            "resourceCosts": [
                {"metadata": {"parentId": NORTH, "name": "gpu-h200-sxm/1gpu-16vcpu-200gb@eu-north1"},
                 "aggregationUnit": {"unit": "hour", "unitType": "UNIT_TYPE_TIME_BASED"},
                 "general": {"total": {"cost": "3.50"}}},
                {"metadata": {"parentId": CENTRAL, "name": "gpu-h200-sxm/1gpu-16vcpu-200gb@us-central1"},
                 "aggregationUnit": {"unit": "hour", "unitType": "UNIT_TYPE_TIME_BASED"},
                 "general": {"total": {"cost": "3.20"}}}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/capacity/v1/resource-advice"))
        .and(query_param("parentId", TENANT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"spec": {"region": "us-central1", "computeInstance": {"platform": "gpu-h200-sxm", "preset": {"name": "1gpu-16vcpu-200gb"}}},
             "status": {"onDemand": {"available": "0", "availabilityLevel": "AVAILABILITY_LEVEL_LIMIT_REACHED", "dataState": "DATA_STATE_FRESH"}}}
        ]})))
        .mount(&server)
        .await;

    let request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H200".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    };
    let offers = provider.plan(&request).await.unwrap();
    // us-central1 is cheaper but the advisor says it is full.
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].to_string(),
        "gpu-h200-sxm/1gpu-16vcpu-200gb@eu-north1 ($3.50/h)"
    );
    let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
    assert_eq!(detail.project_id, NORTH);
}

#[tokio::test]
async fn plan_survives_missing_prices_and_advisor() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("GET"))
        .and(path("/compute/v1/platforms"))
        .respond_with(ResponseTemplate::new(200).set_body_json(platforms(&["gpu-h100-sxm"])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/billing/v1/calculator/estimate-batch"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"code": 14, "message": "unavailable"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/capacity/v1/resource-advice"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"code": 7, "message": "denied"})),
        )
        .mount(&server)
        .await;
    let request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    };
    let offers = provider.plan(&request).await.unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].hourly_price, None);
}

#[tokio::test]
async fn spot_plans_price_from_the_spot_list() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("GET"))
        .and(path("/compute/v1/platforms"))
        .respond_with(ResponseTemplate::new(200).set_body_json(platforms(&["gpu-h100-sxm"])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/billing/v1alpha1/spot-price"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"skuId": "sku-1", "region": "eu-north1", "computeInstance": {"platform": "gpu-h100-sxm"},
             "price": {"value": "0.79", "currency": "usd", "unitTitle": "GPU hour"}}
        ], "snapshotTime": "2026-10-19T00:00:00Z"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/capacity/v1/resource-advice"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&server)
        .await;
    let request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: true,
    };
    let offers = provider.plan(&request).await.unwrap();
    assert_eq!(offers[0].hourly_price, Some(0.79));
}

#[tokio::test]
async fn several_tenants_need_a_choice() {
    let (server, provider) = server().await;
    Mock::given(method("GET"))
        .and(path("/iam/v1/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"metadata": {"id": "tenant-e00a"}}, {"metadata": {"id": "tenant-e00b"}}
        ]})))
        .mount(&server)
        .await;
    let request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    };
    let err = provider.plan(&request).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    assert!(err.msg.contains("tenant-e00a, tenant-e00b"), "{}", err.msg);
}

#[tokio::test]
async fn bad_token_is_a_credentials_error() {
    let (server, provider) = server().await;
    Mock::given(method("GET"))
        .and(path("/iam/v1/tenants"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"code": 16, "message": "invalid token"})),
        )
        .mount(&server)
        .await;
    let request = ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: Vec::new(),
        zone: None,
        spot: false,
    };
    assert_eq!(
        provider.plan(&request).await.unwrap_err().kind,
        ErrorKind::Credentials
    );
}

#[tokio::test]
async fn prepare_picks_the_default_ready_subnet_and_image() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("GET"))
        .and(path("/vpc/v1/subnets"))
        .and(query_param("parentId", NORTH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"metadata": {"id": "vpcsubnet-e00a", "name": "custom"}, "status": {"state": "READY"}},
            {"metadata": {"id": "vpcsubnet-e00z", "name": "default-subnet-x"}, "status": {"state": "READY"}},
            {"metadata": {"id": "vpcsubnet-e000", "name": "default-new"}, "status": {"state": "CREATING"}}
        ]})))
        .mount(&server)
        .await;
    let prepared: Prepared =
        serde_json::from_value(provider.prepare(&offer(), &spec(false)).await.unwrap()).unwrap();
    assert_eq!(prepared.subnet_id, "vpcsubnet-e00z");
    assert_eq!(prepared.image_family.as_deref(), Some(catalog::IMAGE_GPU));

    let mut custom = spec(false);
    custom.image_id = Some("computeimage-e00mine".into());
    let prepared: Prepared =
        serde_json::from_value(provider.prepare(&offer(), &custom).await.unwrap()).unwrap();
    assert_eq!(prepared.image_id.as_deref(), Some("computeimage-e00mine"));
    assert_eq!(prepared.image_family, None);
}

#[tokio::test]
async fn prepare_without_a_subnet_is_unavailable() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("GET"))
        .and(path("/vpc/v1/subnets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let err = provider.prepare(&offer(), &spec(false)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
}

fn prepared() -> serde_json::Value {
    json!({"subnet_id": "vpcsubnet-e00s", "image_family": catalog::IMAGE_GPU, "image_id": null})
}

#[tokio::test]
async fn create_returns_the_operations_instance_id() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("POST"))
        .and(path("/compute/v1/instances"))
        .and(auth())
        .and(body_partial_json(json!({
            "metadata": {"parentId": NORTH, "name": "gpuprov-dev", "labels": {"gpuprov-vm-name": "dev"}},
            "spec": {"resources": {"platform": "gpu-h100-sxm", "preset": "1gpu-16vcpu-200gb"}}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "computeoperation-e00op", "resourceId": "computeinstance-e00new",
            "createdAt": "2026-10-19T00:00:00Z", "finishedAt": "2026-10-19T00:00:05Z", "status": {}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let id = provider
        .create(&offer(), &prepared(), &spec(false))
        .await
        .unwrap();
    assert_eq!(id, "computeinstance-e00new");
}

fn quick(provider: NebiusProvider) -> NebiusProvider {
    provider.with_operation_wait(Duration::from_millis(300), Duration::from_millis(20))
}

#[tokio::test]
async fn create_waits_on_the_operation_and_classifies_its_failure() {
    let server = MockServer::start().await;
    let provider = quick(pinned(&server));
    Mock::given(method("POST"))
        .and(path("/compute/v1/instances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "computeoperation-e00op", "resourceId": "computeinstance-e00new"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/compute/v1/instances/operations/computeoperation-e00op",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "computeoperation-e00op", "resourceId": "computeinstance-e00new",
            "finishedAt": "2026-10-19T00:00:05Z",
            "status": {"code": 8, "message": "resource exhausted", "details": [{
                "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                "notEnoughResources": {"violations": [{"message": "not enough gpu-h100-sxm"}]}
            }]}
        })))
        .mount(&server)
        .await;
    let err = provider
        .create(&offer(), &prepared(), &spec(false))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity, "{}", err.msg);
    assert!(err.msg.contains("computeoperation-e00op"));
}

#[tokio::test]
async fn create_hands_a_long_operation_to_boot_polling() {
    let server = MockServer::start().await;
    let provider = quick(pinned(&server));
    let running = json!({"id": "computeoperation-e00op", "resourceId": "computeinstance-e00new"});
    Mock::given(method("POST"))
        .and(path("/compute/v1/instances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(running.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/compute/v1/instances/operations/computeoperation-e00op",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(running))
        .mount(&server)
        .await;
    let id = provider
        .create(&offer(), &prepared(), &spec(false))
        .await
        .unwrap();
    assert_eq!(id, "computeinstance-e00new");
}

#[tokio::test]
async fn synchronous_create_rejections_are_classified() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("POST"))
        .and(path("/compute/v1/instances"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "code": 8, "message": "quota exceeded", "details": [{
                "@type": "type.googleapis.com/nebius.common.v1.ServiceError",
                "quotaFailure": {"violations": [{"quota": "compute.instance.gpu.h100", "limit": "0", "requested": "1"}]}
            }]
        })))
        .mount(&server)
        .await;
    let err = provider
        .create(&offer(), &prepared(), &spec(false))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Quota);
}

#[tokio::test]
async fn observe_pages_and_filters_by_identity() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    let mut stranger = base_instance("RUNNING");
    stranger["metadata"] =
        json!({"id": "computeinstance-e00other", "name": "theirs", "labels": {}});
    let mut by_name = base_instance("STARTING");
    by_name["metadata"] = json!({"id": "computeinstance-e00byname", "name": "gpuprov-dev"});
    Mock::given(method("GET"))
        .and(path("/compute/v1/instances"))
        .and(query_param("parentId", NORTH))
        .and(query_param("pageToken", "next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [by_name], "nextPageToken": ""
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/compute/v1/instances"))
        .and(query_param("parentId", NORTH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [base_instance("RUNNING"), stranger], "nextPageToken": "next"
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    let at = Location {
        region: "eu-north1".into(),
        zone: None,
    };
    let observed = provider.observe(&at, "dev").await.unwrap();
    let ids: Vec<&str> = observed.iter().map(|o| o.instance_id.as_str()).collect();
    assert_eq!(ids, ["computeinstance-e00byname", "computeinstance-e00dev"]);

    let unknown = Location {
        region: "uk-south1".into(),
        zone: None,
    };
    assert_eq!(
        provider.observe(&unknown, "dev").await.unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
}

#[tokio::test]
async fn delete_treats_missing_instances_as_gone() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    Mock::given(method("DELETE"))
        .and(path("/compute/v1/instances/computeinstance-e00dev"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "computeoperation-e00del", "resourceId": "computeinstance-e00dev"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/compute/v1/instances/computeinstance-e00gone"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"code": 5, "message": "not found"})),
        )
        .mount(&server)
        .await;
    let at = Location {
        region: "eu-north1".into(),
        zone: None,
    };
    provider
        .delete(
            &at,
            "dev",
            &[
                "computeinstance-e00dev".into(),
                "computeinstance-e00gone".into(),
            ],
        )
        .await
        .unwrap();
    provider.cleanup(&at, "dev").await.unwrap();
}

#[tokio::test]
async fn diagnose_reports_a_preempted_vm() {
    let server = MockServer::start().await;
    let provider = pinned(&server);
    let mut preempted = base_instance("STOPPED");
    preempted["spec"]["preemptible"] = json!({"onPreemption": "STOP"});
    Mock::given(method("GET"))
        .and(path("/compute/v1/instances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [preempted]})))
        .mount(&server)
        .await;
    let at = Location {
        region: "eu-north1".into(),
        zone: None,
    };
    let diagnosis = provider
        .diagnose(&at, "dev", Some("computeinstance-e00dev"))
        .await
        .unwrap();
    assert_eq!(diagnosis.instance.unwrap().status, InstanceStatus::Stopped);
    assert_eq!(diagnosis.findings[0].severity, Severity::Critical);
}

#[test]
fn capabilities_support_spot_ports_images_and_boot_scripts() {
    let provider =
        NebiusProvider::with_api("http://127.0.0.1:1", TOKEN, None, BTreeMap::new()).unwrap();
    let caps = provider.capabilities();
    assert!(caps.unsupported.is_empty());
    assert!(caps.boot_script);
    assert_eq!(provider.name(), "nebius");
}
