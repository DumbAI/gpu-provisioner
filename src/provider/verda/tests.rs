//! Verda adapter tests: pure helpers, plus each HTTP primitive (and the
//! OAuth token handling) against a `wiremock` server standing in for the
//! REST API v1.

use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::{BootScript, ErrorKind, GpuRequest, ResourceAsk};

const OUR_KEY: &str = "ssh-ed25519 AAAAour gpuprov";
const TOKEN: &str = "tok-1";

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn fnv_names_are_stable_and_content_addressed() {
    assert_eq!(fnv1a_hex(""), "811c9dc5");
    assert_eq!(fnv1a_hex("a"), "e40c292c");
    let a = script_name("dev", "#!/bin/bash\necho a\n");
    let b = script_name("dev", "#!/bin/bash\necho b\n");
    assert!(a.starts_with("gpuprov-dev-") && a.len() == "gpuprov-dev-".len() + 8);
    assert_ne!(a, b);
    assert_eq!(a, script_name("dev", "#!/bin/bash\necho a\n"));
}

#[test]
fn vm_script_names_do_not_bleed_across_vms() {
    let mine = script_name("dev", "x");
    assert!(is_vm_script(&mine, "dev"));
    assert!(!is_vm_script(&script_name("dev-2", "x"), "dev"));
    assert!(!is_vm_script("gpuprov-dev-notahash", "dev"));
    assert!(!is_vm_script("user-script", "dev"));
}

fn instance(value: serde_json::Value) -> Instance {
    serde_json::from_value(value).unwrap()
}

#[test]
fn verda_statuses_become_findings() {
    let critical = crate::provider::Severity::Critical;
    for (status, text) in [
        ("discontinued", "discontinued by Verda"),
        ("notfound", "not found"),
        ("error", "status 'error'"),
        ("installation_failed", "status 'installation_failed'"),
        ("no_capacity", "no capacity"),
    ] {
        let found = findings_for(&instance(json!({"id": "i1", "status": status})));
        assert_eq!(found.len(), 1, "{status}");
        assert_eq!(found[0].severity, critical, "{status}");
        assert!(found[0].message.contains(text), "{status}: {found:?}");
    }
    let spot = findings_for(&instance(
        json!({"id": "i1", "status": "running", "is_spot": true}),
    ));
    assert_eq!(spot[0].severity, crate::provider::Severity::Info);
    assert!(findings_for(&instance(json!({"id": "i1", "status": "running"}))).is_empty());
}

#[test]
fn a_discontinued_recorded_instance_is_gone_with_the_reason() {
    let instances = [instance(
        json!({"id": "i1", "status": "discontinued", "is_spot": true}),
    )];
    let diagnosis = diagnosis_from(&instances, Some("i1"));
    assert!(diagnosis.instance.is_none());
    assert!(diagnosis.findings[0].message.contains("discontinued"));

    // Without a recorded id the evicted instance still explains the absence.
    let diagnosis = diagnosis_from(&instances, None);
    assert!(diagnosis.instance.is_none());
    assert_eq!(diagnosis.findings.len(), 1);

    // A running instance is picked, with its spot note.
    let running = [instance(
        json!({"id": "i2", "status": "running", "ip": "1.2.3.4", "is_spot": true}),
    )];
    let diagnosis = diagnosis_from(&running, Some("i2"));
    assert_eq!(diagnosis.instance.unwrap().instance_id, "i2");
    assert!(diagnosis.findings[0].message.contains("spot"));
}

#[test]
fn instance_status_and_endpoint_mapping() {
    let running = instance(json!({"id": "i1", "status": "running", "ip": "1.2.3.4"}));
    let o = to_observed(&running).unwrap();
    assert!(o.is_ready());
    assert_eq!(
        o.endpoint,
        Some(SshEndpoint {
            host: "1.2.3.4".into(),
            port: 22,
            user: Some("root".into()),
        })
    );
    // Running but no public address yet: not reachable.
    let no_ip = to_observed(&instance(
        json!({"id": "i1", "status": "running", "ip": null}),
    ));
    assert!(!no_ip.unwrap().is_ready());

    for (status, expected) in [
        ("new", InstanceStatus::Pending),
        ("ordered", InstanceStatus::Pending),
        ("validating", InstanceStatus::Pending),
        ("provisioning", InstanceStatus::Pending),
        ("unknown", InstanceStatus::Pending),
        ("offline", InstanceStatus::Stopped),
        ("deleting", InstanceStatus::Terminating),
        ("error", InstanceStatus::Failed("error".into())),
        (
            "installation_failed",
            InstanceStatus::Failed("installation_failed".into()),
        ),
        ("no_capacity", InstanceStatus::Failed("no capacity".into())),
    ] {
        let o = to_observed(&instance(
            json!({"id": "i1", "status": status, "ip": "1.2.3.4"}),
        ))
        .unwrap();
        assert_eq!(o.status, expected, "{status}");
        assert!(o.endpoint.is_none(), "{status}");
    }
    for gone in ["discontinued", "notfound"] {
        assert!(to_observed(&instance(json!({"id": "i1", "status": gone}))).is_none());
    }
}

#[test]
fn capabilities_run_boot_scripts_but_open_no_ports() {
    let provider = VerdaProvider::with_api("http://127.0.0.1:1", "id", "secret").unwrap();
    let caps = provider.capabilities();
    assert!(caps.boot_script);
    assert!(!caps.supports(Feature::OpenPorts));
    for feature in [Feature::Spot, Feature::ImageOverride, Feature::Kubernetes] {
        assert!(caps.supports(feature), "{feature}");
    }
}

#[test]
fn missing_credentials_fail_at_construction() {
    // Explicit empty config values are ignored; env is consulted next, so
    // only assert the error shape when the env is not set.
    if std::env::var(CLIENT_ID_ENV).is_ok() || std::env::var(CLIENT_SECRET_ENV).is_ok() {
        return;
    }
    let err = match VerdaProvider::new(&VerdaConfig::default()) {
        Ok(_) => panic!("credentials should be required"),
        Err(e) => e,
    };
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert!(err.msg.contains(CLIENT_ID_ENV), "{}", err.msg);
}

// ---------------------------------------------------------------------------
// HTTP primitives against a mock API
// ---------------------------------------------------------------------------

async fn server() -> MockServer {
    MockServer::start().await
}

fn token_response(token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": token, "token_type": "Bearer", "expires_in": 3600,
        "refresh_token": "r", "scope": "cloud-api-v1"
    }))
}

async fn mount_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/oauth2/token"))
        .and(body_json(json!({
            "grant_type": "client_credentials", "client_id": "cid", "client_secret": "secret"
        })))
        .respond_with(token_response(TOKEN))
        .mount(server)
        .await;
}

fn provider(server: &MockServer) -> VerdaProvider {
    VerdaProvider::with_api(&server.uri(), "cid", "secret").unwrap()
}

fn here() -> Location {
    Location {
        region: "FIN-01".into(),
        zone: None,
    }
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

fn h100_type() -> serde_json::Value {
    json!({
        "instance_type": "1H100.80S.22V", "model": "H100", "name": "H100 SXM5 80GB",
        "display_name": "NVIDIA H100 SXM5 80GB", "gpu": {"number_of_gpus": 1},
        "price_per_hour": "3.17", "spot_price": "1.10",
        "supported_os": ["ubuntu-22.04-cuda-12.3", "ubuntu-24.04-cuda-12.8-open-docker", "ubuntu-24.04"]
    })
}

async fn mount_catalog(server: &MockServer, spot: bool) {
    Mock::given(method("GET"))
        .and(path("/v1/instance-types"))
        .and(query_param("currency", "usd"))
        .and(header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([h100_type()])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/instance-availability"))
        .and(query_param("is_spot", spot.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"location_code": "FIN-01", "availabilities": ["1H100.80S.22V"]},
            {"location_code": "ICE-01", "availabilities": ["1H100.80S.22V"]},
            {"location_code": "FIN-02", "availabilities": []}
        ])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/locations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"code": "FIN-01", "name": "Finland 1", "country_code": "FI"},
            {"code": "FIN-02", "name": "Finland 2", "country_code": "FI"},
            {"code": "ICE-01", "name": "Iceland 1", "country_code": "IS"}
        ])))
        .mount(server)
        .await;
}

fn request(spot: bool, regions: &[&str]) -> ResourceRequest {
    ResourceRequest {
        ask: ResourceAsk::Gpus(GpuRequest {
            name: "H100".into(),
            count: 1,
        }),
        regions: regions.iter().map(|r| r.to_string()).collect(),
        zone: None,
        spot,
    }
}

#[tokio::test]
async fn plan_builds_offers_and_reuses_one_token() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth2/token"))
        .respond_with(token_response(TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    mount_catalog(&server, false).await;
    let provider = provider(&server);

    let offers = provider.plan(&request(false, &[])).await.unwrap();
    let rendered: Vec<String> = offers.iter().map(|o| o.to_string()).collect();
    assert_eq!(
        rendered,
        [
            "1H100.80S.22V@FIN-01 ($3.17/h)",
            "1H100.80S.22V@ICE-01 ($3.17/h)"
        ]
    );
    // Country filter via /v1/locations; the token is fetched once overall.
    let finnish = provider.plan(&request(false, &["FI"])).await.unwrap();
    assert_eq!(finnish.len(), 1);
    assert_eq!(finnish[0].region, "FIN-01");
}

#[tokio::test]
async fn plan_spot_queries_spot_availability_and_prices() {
    let server = server().await;
    mount_token(&server).await;
    mount_catalog(&server, true).await;
    let offers = provider(&server).plan(&request(true, &[])).await.unwrap();
    assert_eq!(offers[0].hourly_price, Some(1.10));
    let detail: OfferDetail = serde_json::from_value(offers[0].detail.clone()).unwrap();
    assert!(detail.is_spot);
}

#[tokio::test]
async fn expired_token_is_refreshed_once_then_reported() {
    let server = server().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth2/token"))
        .respond_with(token_response(TOKEN))
        .expect(2)
        .mount(&server)
        .await;
    // First call with the cached token is rejected, the retry succeeds.
    Mock::given(method("GET"))
        .and(path("/v1/locations"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "code": "unauthorized_request", "message": "token expired"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/locations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let provider = provider(&server);
    assert!(provider.api.locations().await.unwrap().is_empty());
}

#[tokio::test]
async fn persistent_401_and_rejected_credentials_are_credentials_errors() {
    let server = server().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1/instance-types"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "code": "unauthorized_request", "message": "nope"
        })))
        .mount(&server)
        .await;
    let err = provider(&server)
        .plan(&request(false, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);

    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth2/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "code": "unauthorized_request", "message": "invalid client"
        })))
        .mount(&bad)
        .await;
    let err = provider(&bad).plan(&request(false, &[])).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Credentials);
    assert!(err.msg.contains("invalid client"), "{}", err.msg);
}

fn offer() -> Offer {
    let detail = OfferDetail {
        instance_type: "1H100.80S.22V".into(),
        location_code: "FIN-01".into(),
        is_spot: false,
        supported_os: vec![
            "ubuntu-22.04-cuda-12.3".into(),
            "ubuntu-24.04-cuda-12.8-open-docker".into(),
        ],
        gpu: true,
    };
    let mut offer = Offer::new("1H100.80S.22V", "FIN-01");
    offer.detail = serde_json::to_value(detail).unwrap();
    offer
}

async fn mount_available(server: &MockServer, available: bool) {
    Mock::given(method("GET"))
        .and(path("/v1/instance-availability/1H100.80S.22V"))
        .and(query_param("is_spot", "false"))
        .and(query_param("location_code", "FIN-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(available)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn prepare_reuses_registered_key_and_script() {
    let server = server().await;
    mount_token(&server).await;
    mount_available(&server, true).await;
    let mut spec = spec();
    spec.boot_script = Some(BootScript {
        script: "#!/usr/bin/env bash\necho k3s\n".into(),
        env: vec![("GPUPROV_INSTALL_ORIGIN".into(), "boot".into())],
    });
    let name = script_name("vm1", &spec.boot_script.as_ref().unwrap().to_shell());
    Mock::given(method("GET"))
        .and(path("/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "k-other", "name": "laptop", "key": "ssh-rsa BBBB me", "fingerprint": null,
             "created_by_user_id": null},
            {"id": "k-ours", "name": "renamed", "key": "ssh-ed25519 AAAAour another-comment",
             "fingerprint": null, "created_by_user_id": null}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/scripts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "s-1", "name": name, "script": "...", "created_at": "2026-01-01"}
        ])))
        .mount(&server)
        .await;
    // Nothing may be created when both already exist.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .named("no creates")
        .mount(&server)
        .await;

    let prepared = provider(&server).prepare(&offer(), &spec).await.unwrap();
    assert_eq!(
        prepared,
        json!({
            "ssh_key_id": "k-ours",
            "startup_script_id": "s-1",
            "image": "ubuntu-24.04-cuda-12.8-open-docker"
        })
    );
}

#[tokio::test]
async fn prepare_registers_missing_key_and_script() {
    let server = server().await;
    mount_token(&server).await;
    mount_available(&server, true).await;
    let mut spec = spec();
    spec.image_id = Some("ubuntu-24.04".into());
    spec.boot_script = Some(BootScript {
        script: "#!/usr/bin/env bash\necho k3s\n".into(),
        env: Vec::new(),
    });
    let content = spec.boot_script.as_ref().unwrap().to_shell();
    Mock::given(method("GET"))
        .and(path("/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/ssh-keys"))
        .and(body_json(json!({
            "name": format!("gpuprov-{}", fnv1a_hex("ssh-ed25519 AAAAour")),
            "key": OUR_KEY
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!("k-new")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/scripts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/scripts"))
        .and(body_json(
            json!({"name": script_name("vm1", &content), "script": content}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!("s-new")))
        .expect(1)
        .mount(&server)
        .await;

    let prepared = provider(&server).prepare(&offer(), &spec).await.unwrap();
    assert_eq!(
        prepared,
        json!({"ssh_key_id": "k-new", "startup_script_id": "s-new", "image": "ubuntu-24.04"})
    );
}

#[tokio::test]
async fn prepare_reports_sold_out_offers_as_unavailable() {
    let server = server().await;
    mount_token(&server).await;
    mount_available(&server, false).await;
    Mock::given(method("GET"))
        .and(path("/v1/ssh-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "k-ours", "name": "x", "key": OUR_KEY, "fingerprint": null,
             "created_by_user_id": null}
        ])))
        .mount(&server)
        .await;
    let err = provider(&server)
        .prepare(&offer(), &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    assert!(err.msg.contains("FIN-01"), "{}", err.msg);
}

#[tokio::test]
async fn create_sends_the_tagged_deploy_body() {
    let server = server().await;
    mount_token(&server).await;
    let tags = json!([
        {"key": "gpuprov-vm-name", "value": "vm1"},
        {"key": "gpuprov-managed", "value": "true"}
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/instances"))
        .and(body_json(json!({
            "instance_type": "1H100.80S.22V",
            "image": "ubuntu-24.04-cuda-12.8-open-docker",
            "ssh_key_ids": ["k-ours"],
            "startup_script_id": "s-1",
            "location_code": "FIN-01",
            "hostname": "gpuprov-vm1",
            "description": "gpuprov vm1",
            "is_spot": false,
            "os_volume": {
                "name": "gpuprov-vm1-os", "size": 120,
                "on_spot_discontinue": "delete_permanently", "tags": tags
            },
            "tags": tags
        })))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!("inst-1")))
        .mount(&server)
        .await;
    let prepared = json!({
        "ssh_key_id": "k-ours", "startup_script_id": "s-1",
        "image": "ubuntu-24.04-cuda-12.8-open-docker"
    });
    let id = provider(&server)
        .create(&offer(), &prepared, &spec())
        .await
        .unwrap();
    assert_eq!(id, "inst-1");
}

#[tokio::test]
async fn create_without_capacity_is_a_capacity_error() {
    let server = server().await;
    mount_token(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/instances"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "code": "service_unavailable", "message": "No capacity available"
        })))
        .mount(&server)
        .await;
    let prepared = json!({"ssh_key_id": "k", "image": "ubuntu-24.04"});
    let err = provider(&server)
        .create(&offer(), &prepared, &spec())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity);
}

async fn mount_instances(server: &MockServer, instances: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/v1/instances"))
        .and(query_param("tag", "gpuprov-vm-name=vm1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances))
        .mount(server)
        .await;
}

fn tagged(id: &str, status: &str, ip: Option<&str>) -> serde_json::Value {
    json!({
        "id": id, "status": status, "ip": ip, "location": "FIN-01",
        "tags": [{"id": "t", "key": "gpuprov-vm-name", "value": "vm1"}],
        "os_volume_id": format!("{id}-os"), "volume_ids": [format!("{id}-os"), format!("{id}-data")]
    })
}

#[tokio::test]
async fn observe_filters_by_tag_and_maps_instances() {
    let server = server().await;
    mount_token(&server).await;
    let mut stranger = tagged("i-x", "running", Some("9.9.9.9"));
    stranger["tags"] = json!([{"id": "t", "key": "gpuprov-vm-name", "value": "other"}]);
    mount_instances(
        &server,
        json!([
            tagged("i-2", "provisioning", None),
            tagged("i-1", "running", Some("1.2.3.4")),
            tagged("i-0", "discontinued", None),
            stranger
        ]),
    )
    .await;
    let observed = provider(&server).observe(&here(), "vm1").await.unwrap();
    let ids: Vec<&str> = observed.iter().map(|o| o.instance_id.as_str()).collect();
    assert_eq!(ids, ["i-1", "i-2"]);
    assert!(observed[0].is_ready());
    assert_eq!(observed[1].status, InstanceStatus::Pending);
}

#[tokio::test]
async fn delete_removes_instances_with_their_volumes() {
    let server = server().await;
    mount_token(&server).await;
    mount_instances(
        &server,
        json!([
            tagged("i-1", "running", Some("1.2.3.4")),
            tagged("i-2", "running", None)
        ]),
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/v1/instances"))
        .and(body_json(json!({
            "action": "delete",
            "id": ["i-1"],
            "volume_ids": ["i-1-data", "i-1-os"],
            "delete_permanently": true
        })))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!([
            {"instanceId": "i-1", "action": "delete", "status": "success"}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server)
        .delete(&here(), "vm1", &["i-1".into()])
        .await
        .unwrap();
    // Already gone from the listing: nothing to send.
    provider(&server)
        .delete(&here(), "vm1", &["i-gone".into()])
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_partial_results_tolerate_not_found_only() {
    let server = server().await;
    mount_token(&server).await;
    mount_instances(
        &server,
        json!([
            tagged("i-1", "running", None),
            tagged("i-2", "running", None)
        ]),
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/v1/instances"))
        .respond_with(ResponseTemplate::new(207).set_body_json(json!([
            {"instanceId": "i-1", "action": "delete", "status": "error",
             "error": "Instance not found", "statusCode": 404},
            {"instanceId": "i-2", "action": "delete", "status": "error",
             "error": "Instance is locked", "statusCode": 400}
        ])))
        .mount(&server)
        .await;
    let err = provider(&server)
        .delete(&here(), "vm1", &["i-1".into(), "i-2".into()])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Failed);
    assert!(err.msg.contains("i-2: Instance is locked"), "{}", err.msg);
    assert!(!err.msg.contains("i-1"), "{}", err.msg);
}

#[tokio::test]
async fn cleanup_deletes_detached_tagged_volumes_and_vm_scripts() {
    let server = server().await;
    mount_token(&server).await;
    let tag = json!([{"id": "t", "key": "gpuprov-vm-name", "value": "vm1"}]);
    Mock::given(method("GET"))
        .and(path("/v1/volumes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "v-detached", "status": "detached", "instance_id": null, "tags": tag},
            {"id": "v-attached", "status": "attached", "instance_id": "i-9", "tags": tag},
            {"id": "v-deleted", "status": "deleted", "instance_id": null, "tags": tag},
            {"id": "v-foreign", "status": "detached", "instance_id": null, "tags": []}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/volumes/v-detached"))
        .and(body_json(json!({"is_permanent": true})))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let mine = script_name("vm1", "a");
    let theirs = script_name("vm1-2", "a");
    Mock::given(method("GET"))
        .and(path("/v1/scripts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": "s-mine", "name": mine, "script": "", "created_at": ""},
            {"id": "s-theirs", "name": theirs, "script": "", "created_at": ""}
        ])))
        .mount(&server)
        .await;
    // Already deleted elsewhere: 404 is fine.
    Mock::given(method("DELETE"))
        .and(path("/v1/scripts/s-mine"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "code": "not_found", "message": "gone"
        })))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server).cleanup(&here(), "vm1").await.unwrap();
}
