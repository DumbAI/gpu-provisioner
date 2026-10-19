//! Prime Intellect adapter: a GPU marketplace that resells capacity from
//! upstream providers (Hyperstack, Lambda, DataCrunch, ...) behind one REST
//! API, mapped onto the gpuprov provider contract:
//!
//! - **plan** — live `GET /availability/gpus` (one offer per provider × data
//!   center, cheapest first) — live, because marketplace stock churns;
//!   region = Prime Intellect region id (`united_states`), zone = data center.
//! - **prepare** — find-or-register our SSH key, then re-check the offer is still
//!   in stock so a stale plan fails before anything is created.
//! - **create** — `POST /pods/` with the offer's own `socket` (SXM offers
//!   need it) and the key attached per pod via `sshKeyId` (no change
//!   to the account's primary key).
//! - **observe / delete** — pods are found by name `gpuprov-<vm>`; SSH user
//!   and port come from the pod's `sshConnection` (often not `ubuntu@:22`).
//!
//! Pods are reached over SSH only: no firewall API, no custom images, no spot
//! — declared as unsupported features so such asks fail before planning.

pub mod api;
pub mod catalog;
pub mod error;
pub mod ssh;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use rand_core::RngCore;
use serde::Deserialize;

use crate::config::PrimeIntellectConfig;
use crate::provider::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceRequest, SshEndpoint,
};

use api::{Api, AvailabilityQuery, CreatePod, Pod, PodRequest, ProviderType, TeamConfig};
use catalog::OfferDetail;
use error::{ApiError, Op};

pub const PROVIDER_ID: &str = "primeintellect";

pub const DEFAULT_BASE_URL: &str = "https://api.primeintellect.ai";
/// The prime CLI's credentials file (written by `prime login`).
const CREDENTIALS_PATH: &str = ".prime/config.json";
const API_KEY_ENV: &str = "PRIME_API_KEY";

/// 16-minute boot/terminate budget: marketplace pods can take far longer than
/// EC2 to become ACTIVE or disappear.
const BOOT_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);

const UNSUPPORTED: &[(Feature, &str)] = &[
    (Feature::OpenPorts, "no firewall API; pods expose SSH only"),
    (
        Feature::ImageOverride,
        "images are chosen from the offer's environment list",
    ),
    (Feature::Spot, "only on-demand pods are provisioned"),
];

/// `~/.prime/config.json` as written by `prime login`.
#[derive(Debug, Default, Deserialize)]
struct PrimeCliConfig {
    api_key: Option<String>,
    team_id: Option<String>,
    base_url: Option<String>,
}

pub struct PrimeIntellectProvider {
    api: Api,
    team_id: Option<String>,
}

impl PrimeIntellectProvider {
    /// Resolve credentials: config `api_key` → `$PRIME_API_KEY` →
    /// `~/.prime/config.json`. `team_id` and `base_url`: config → file.
    pub fn new(config: &PrimeIntellectConfig) -> Result<Self, ProviderError> {
        let file = dirs::home_dir()
            .map(|home| read_cli_config(&home.join(CREDENTIALS_PATH)))
            .unwrap_or_default();
        let env_key = std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty());
        let api_key = config
            .api_key
            .clone()
            .or(env_key)
            .or(file.api_key)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                ProviderError::credentials(format!(
                    "no Prime Intellect API key: set primeintellect.api_key in the config file, \
                     ${API_KEY_ENV}, or run `prime login` (~/{CREDENTIALS_PATH})"
                ))
            })?;
        let base_url = config
            .base_url
            .clone()
            .or(file.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let team_id = config
            .team_id
            .clone()
            .or(file.team_id)
            .filter(|t| !t.is_empty());
        Self::with_api(&base_url, &api_key, team_id)
    }

    /// Construct against an explicit endpoint and key (tests, staging).
    pub fn with_api(
        base_url: &str,
        api_key: &str,
        team_id: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api = Api::new(base_url, api_key).map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(PrimeIntellectProvider { api, team_id })
    }

    /// Our SSH key's id on the account, registering it if absent (matched on
    /// key type + material, ignoring the comment).
    async fn ensure_ssh_key(&self, public_key: &str) -> Result<String, ProviderError> {
        let ours = key_material(public_key);
        let keys = self
            .api
            .list_ssh_keys()
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        if let Some(key) = keys.iter().find(|k| key_material(&k.public_key) == ours) {
            return Ok(key.id.clone());
        }
        let name = format!("gpuprov-{:08x}", rand_core::OsRng.next_u32());
        tracing::info!("registering SSH key '{name}' with Prime Intellect");
        self.api
            .add_ssh_key(&name, public_key.trim())
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))
    }

    async fn pods_of(&self, vm_name: &str, op: Op) -> Result<Vec<Pod>, ProviderError> {
        let name = pod_name(vm_name);
        let pods = self
            .api
            .list_pods()
            .await
            .map_err(|e| e.into_provider_error(op))?;
        Ok(pods.into_iter().filter(|p| p.name == name).collect())
    }
}

fn read_cli_config(path: &Path) -> PrimeCliConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Pod name carrying the VM identity.
pub fn pod_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

/// `"<type> <base64>"` of an OpenSSH public key, without the comment.
fn key_material(public_key: &str) -> Vec<&str> {
    public_key.split_whitespace().take(2).collect()
}

fn offer_detail(offer: &Offer) -> Result<OfferDetail, ProviderError> {
    serde_json::from_value(offer.detail.clone()).map_err(|e| {
        ProviderError::invalid(format!(
            "offer {offer} carries no Prime Intellect launch details ({e}); re-plan"
        ))
    })
}

/// Normalize a pod's status and SSH endpoint; `None` for
/// terminated pods, which no longer count as live.
fn to_observed(pod: &Pod) -> Option<Observed> {
    let status = match pod.status.as_str() {
        "TERMINATED" => return None,
        "ACTIVE" => InstanceStatus::Running,
        "STOPPED" => InstanceStatus::Stopped,
        "DELETING" => InstanceStatus::Terminating,
        "ERROR" => InstanceStatus::Failed(
            pod.installation_failure
                .clone()
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| "ERROR".to_string()),
        ),
        // PROVISIONING, PENDING, UNKNOWN, and anything new.
        _ => InstanceStatus::Pending,
    };
    let ip = pod.ip.as_ref().and_then(ssh::first_ip);
    let endpoint = if status == InstanceStatus::Running {
        pod.ssh_connection
            .as_ref()
            .and_then(ssh::parse_ssh_connection)
            .and_then(|conn| {
                let host = conn.host.or(ip.clone())?;
                Some(SshEndpoint {
                    host,
                    port: conn.port,
                    user: conn.user,
                })
            })
    } else {
        None
    };
    Some(Observed {
        instance_id: pod.id.clone(),
        status,
        zone: None,
        private_ip: None,
        endpoint,
    })
}

#[async_trait]
impl CloudProvider for PrimeIntellectProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: UNSUPPORTED,
            // Pods take no user data; workloads are pushed over SSH.
            boot_script: false,
            boot_timeout: BOOT_TIMEOUT,
            delete_timeout: BOOT_TIMEOUT,
            poll_interval: POLL_INTERVAL,
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        let items = self
            .api
            .availability(&catalog::query_for(request))
            .await
            .map_err(|e| e.into_provider_error(Op::Plan))?;
        Ok(catalog::offers_from(items, request))
    }

    async fn prepare(
        &self,
        offer: &Offer,
        spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let detail = offer_detail(offer)?;
        let ssh_key_id = self.ensure_ssh_key(&spec.ssh_public_key).await?;

        // Re-validate: the plan may be minutes old and marketplace stock moves.
        let fresh = self
            .api
            .availability(&AvailabilityQuery {
                gpu_count: Some(detail.gpu_count),
                cloud_id: Some(detail.cloud_id.clone()),
                data_center_id: detail.data_center.clone(),
                ..Default::default()
            })
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        if !fresh
            .iter()
            .any(|item| catalog::still_offered(item, &detail))
        {
            return Err(ProviderError::unavailable(format!(
                "offer {offer} is no longer in stock"
            )));
        }
        Ok(serde_json::json!({ "ssh_key_id": ssh_key_id }))
    }

    async fn create(
        &self,
        offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let detail = offer_detail(offer)?;
        let ssh_key_id = prepared["ssh_key_id"]
            .as_str()
            .ok_or_else(|| ProviderError::failed("prepared state lacks ssh_key_id"))?
            .to_string();
        let body = CreatePod {
            pod: PodRequest {
                name: pod_name(&spec.vm_name),
                cloud_id: detail.cloud_id,
                gpu_type: detail.gpu_type,
                socket: detail.socket,
                gpu_count: detail.gpu_count,
                disk_size: spec.disk_gb,
                image: detail.image,
                data_center_id: detail.data_center,
                country: detail.country,
                security: detail.security,
                ssh_key_id,
            },
            provider: ProviderType {
                kind: detail.provider,
            },
            team: self.team_id.clone().map(|team_id| TeamConfig { team_id }),
        };
        let pod_id = self
            .api
            .create_pod(&body)
            .await
            .map_err(|e| e.into_provider_error(Op::Launch))?;
        tracing::info!("created Prime Intellect pod {pod_id} ({offer})");
        Ok(pod_id)
    }

    /// Pods are account-global, so the location is not a filter here.
    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let pods = self.pods_of(vm_name, Op::Manage).await?;
        Ok(pods.iter().filter_map(to_observed).collect())
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        for id in instance_ids {
            self.api
                .delete_pod(id)
                .await
                .map_err(|e: ApiError| e.into_provider_error(Op::Manage))?;
        }
        Ok(())
    }

    /// Nothing per-VM to clean up: the registered SSH key is account-level and
    /// shared by every gpuprov VM.
    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::provider::{ErrorKind, GpuRequest, ResourceAsk};

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGpu gpuprov@host";

    fn provider(server: &MockServer) -> PrimeIntellectProvider {
        PrimeIntellectProvider::with_api(&server.uri(), "pk-test", Some("team-1".into())).unwrap()
    }

    fn spec() -> InstanceSpec {
        InstanceSpec {
            vm_name: "dev".into(),
            image_id: None,
            disk_gb: 120,
            spot: false,
            open_ports: Vec::new(),
            user_data: String::new(),
            ssh_public_key: KEY.into(),
            ssh_user: "ubuntu".into(),
            boot_script: None,
        }
    }

    fn offer_json(cloud_id: &str, dc: &str, price: f64, stock: &str) -> serde_json::Value {
        json!({
            "cloudId": cloud_id, "gpuType": "H100_80GB", "socket": "SXM5",
            "provider": "hyperstack", "gpuCount": 1, "gpuMemory": 80,
            "region": "canada", "dataCenter": dc, "country": "CA",
            "stockStatus": stock, "security": "secure_cloud",
            "prices": {"onDemand": price, "isVariable": false, "currency": "USD"},
            "images": ["ubuntu_22_cuda_12"], "isSpot": null
        })
    }

    fn h100(count: u32) -> ResourceRequest {
        ResourceRequest {
            ask: ResourceAsk::Gpus(GpuRequest {
                name: "H100".into(),
                count,
            }),
            regions: vec!["canada".into()],
            zone: None,
            spot: false,
        }
    }

    fn pod_json(id: &str, name: &str, status: &str) -> serde_json::Value {
        json!({
            "id": id, "userId": "u", "name": name, "providerType": "hyperstack",
            "status": status, "gpuName": "H100_80GB", "gpuCount": 1, "socket": "SXM5",
            "priceHr": 1.9, "sshConnection": null, "ip": null
        })
    }

    #[tokio::test]
    async fn plan_paginates_availability_and_sorts_by_price() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/availability/gpus"))
            .and(query_param("page", "1"))
            .and(query_param("regions", "canada"))
            .and(query_param("gpu_count", "1"))
            .and(header("authorization", "Bearer pk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [offer_json("pricey", "CANADA-1", 3.0, "Available")],
                "totalCount": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/availability/gpus"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [offer_json("cheap", "CANADA-2", 1.5, "Low")],
                "totalCount": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        let offers = provider(&server).plan(&h100(1)).await.unwrap();
        let types: Vec<&str> = offers.iter().map(|o| o.instance_type.as_str()).collect();
        assert_eq!(types, ["hyperstack/cheap", "hyperstack/pricey"]);
        assert_eq!(offers[0].zone.as_deref(), Some("CANADA-2"));
    }

    #[tokio::test]
    async fn plan_rejection_is_invalid_request_and_401_is_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/availability/gpus"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "errors": [{"param": "regions", "details": "not a valid region"}]
            })))
            .mount(&server)
            .await;
        let err = provider(&server).plan(&h100(1)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(
            err.msg.contains("regions: not a valid region"),
            "{}",
            err.msg
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"detail": "bad key"})))
            .mount(&server)
            .await;
        let err = provider(&server).plan(&h100(1)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Credentials);
    }

    async fn mount_fresh_availability(server: &MockServer, stock: &str) {
        Mock::given(method("GET"))
            .and(path("/api/v1/availability/gpus"))
            .and(query_param("cloud_id", "cheap"))
            .and(query_param("data_center_id", "CANADA-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [offer_json("cheap", "CANADA-2", 1.5, stock)],
                "totalCount": 1
            })))
            .mount(server)
            .await;
    }

    /// The offer `plan` would have produced for the `cheap` mock item.
    fn planned_offer() -> Offer {
        let items: Vec<api::GpuOffer> =
            serde_json::from_value(json!([offer_json("cheap", "CANADA-2", 1.5, "Available")]))
                .unwrap();
        catalog::offers_from(items, &h100(1)).remove(0)
    }

    #[tokio::test]
    async fn prepare_reuses_a_registered_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/ssh_keys/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "k-other", "name": "x", "publicKey": "ssh-rsa AAAAB3 other"},
                    // Same key material, different comment.
                    {"id": "k-ours", "name": "y", "publicKey": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGpu laptop"}
                ],
                "total_count": 2
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/ssh_keys/"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        mount_fresh_availability(&server, "Available").await;
        let offer = planned_offer();
        let prepared = provider(&server).prepare(&offer, &spec()).await.unwrap();
        assert_eq!(prepared["ssh_key_id"], "k-ours");
    }

    #[tokio::test]
    async fn prepare_registers_a_missing_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/ssh_keys/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [], "total_count": 0})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/ssh_keys/"))
            .and(body_partial_json(json!({"publicKey": KEY})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "k-new", "userId": "u", "name": "gpuprov-1", "publicKey": KEY,
                "isPrimary": false, "isUserKey": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        mount_fresh_availability(&server, "Available").await;
        let offer = planned_offer();
        let prepared = provider(&server).prepare(&offer, &spec()).await.unwrap();
        assert_eq!(prepared["ssh_key_id"], "k-new");
        let posted: Vec<Request> = server.received_requests().await.unwrap();
        let body: serde_json::Value = posted
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .unwrap()
            .body_json()
            .unwrap();
        assert!(body["name"].as_str().unwrap().starts_with("gpuprov-"));
    }

    #[tokio::test]
    async fn prepare_fails_unavailable_when_offer_sold_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/ssh_keys/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "k", "name": "n", "publicKey": KEY}]
            })))
            .mount(&server)
            .await;
        mount_fresh_availability(&server, "Unavailable").await;
        let offer = planned_offer();
        let err = provider(&server)
            .prepare(&offer, &spec())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unavailable);
    }

    #[tokio::test]
    async fn create_sends_offer_socket_key_and_identity_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/pods/"))
            .and(body_partial_json(json!({
                "pod": {
                    "name": "gpuprov-dev", "cloudId": "cheap", "gpuType": "H100_80GB",
                    "socket": "SXM5", "gpuCount": 1, "diskSize": 120,
                    "image": "ubuntu_22_cuda_12", "dataCenterId": "CANADA-2",
                    "country": "CA", "security": "secure_cloud", "sshKeyId": "k-1"
                },
                "provider": {"type": "hyperstack"},
                "team": {"teamId": "team-1"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(pod_json(
                "pod-1",
                "gpuprov-dev",
                "PROVISIONING",
            )))
            .expect(1)
            .mount(&server)
            .await;
        let offer = planned_offer();
        let id = provider(&server)
            .create(&offer, &json!({"ssh_key_id": "k-1"}), &spec())
            .await
            .unwrap();
        assert_eq!(id, "pod-1");
    }

    #[tokio::test]
    async fn create_capacity_error_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/pods/"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"message": "No capacity available for H100_80GB"})),
            )
            .mount(&server)
            .await;
        let offer = planned_offer();
        let err = provider(&server)
            .create(&offer, &json!({"ssh_key_id": "k-1"}), &spec())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Capacity);
    }

    #[tokio::test]
    async fn observe_paginates_and_maps_status_and_endpoint() {
        let server = MockServer::start().await;
        // Page 1: a full page of other pods plus nothing of ours.
        let others: Vec<serde_json::Value> = (0..api::PAGE_SIZE)
            .map(|i| pod_json(&format!("o{i}"), "someone-else", "ACTIVE"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/pods/"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": others, "total_count": api::PAGE_SIZE + 3
            })))
            .mount(&server)
            .await;
        let mut active = pod_json("p-1", "gpuprov-dev", "ACTIVE");
        active["sshConnection"] = json!("root@203.0.113.9 -p 40022");
        active["ip"] = json!(["203.0.113.9"]);
        let mut failed = pod_json("p-2", "gpuprov-dev", "ERROR");
        failed["installationFailure"] = json!("driver install failed");
        Mock::given(method("GET"))
            .and(path("/api/v1/pods/"))
            .and(query_param("offset", api::PAGE_SIZE.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [active, failed, pod_json("p-3", "gpuprov-dev", "TERMINATED")],
                "total_count": api::PAGE_SIZE + 3
            })))
            .mount(&server)
            .await;
        let at = Location {
            region: "canada".into(),
            zone: None,
        };
        let observed = provider(&server).observe(&at, "dev").await.unwrap();
        assert_eq!(observed.len(), 2, "TERMINATED pods are not live");
        let running = &observed[0];
        assert!(running.is_ready());
        assert_eq!(
            running.endpoint,
            Some(SshEndpoint {
                host: "203.0.113.9".into(),
                port: 40022,
                user: Some("root".into()),
            })
        );
        assert_eq!(
            observed[1].status,
            InstanceStatus::Failed("driver install failed".into())
        );
    }

    #[tokio::test]
    async fn active_pod_without_ssh_connection_is_not_ready() {
        let server = MockServer::start().await;
        let mut pod = pod_json("p-1", "gpuprov-dev", "ACTIVE");
        pod["ip"] = json!("203.0.113.9");
        Mock::given(method("GET"))
            .and(path("/api/v1/pods/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [pod], "total_count": 1})),
            )
            .mount(&server)
            .await;
        let at = Location {
            region: "any".into(),
            zone: None,
        };
        let observed = provider(&server).observe(&at, "dev").await.unwrap();
        assert_eq!(observed[0].status, InstanceStatus::Running);
        assert!(!observed[0].is_ready());
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/pods/p-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "TERMINATED"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/pods/gone"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"detail": "not found"})))
            .expect(1)
            .mount(&server)
            .await;
        let at = Location {
            region: "any".into(),
            zone: None,
        };
        provider(&server)
            .delete(&at, "dev", &["p-1".into(), "gone".into()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_server_error_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let at = Location {
            region: "any".into(),
            zone: None,
        };
        let err = provider(&server)
            .delete(&at, "dev", &["p-1".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Transient);
    }

    #[test]
    fn capabilities_reject_ports_images_and_spot() {
        let server_less =
            PrimeIntellectProvider::with_api("http://127.0.0.1:1", "k", None).unwrap();
        let caps = server_less.capabilities();
        for feature in [Feature::OpenPorts, Feature::ImageOverride, Feature::Spot] {
            assert!(!caps.supports(feature), "{feature} should be unsupported");
        }
        assert_eq!(caps.boot_timeout, BOOT_TIMEOUT);
    }

    #[test]
    fn missing_detail_is_invalid_request() {
        let offer = Offer::new("hyperstack/x", "canada");
        assert_eq!(
            offer_detail(&offer).unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn reads_prime_cli_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"api_key": "pk-file", "team_id": "t", "base_url": "https://staging"}"#,
        )
        .unwrap();
        let cfg = read_cli_config(&path);
        assert_eq!(cfg.api_key.as_deref(), Some("pk-file"));
        assert_eq!(cfg.team_id.as_deref(), Some("t"));
        assert_eq!(cfg.base_url.as_deref(), Some("https://staging"));
        assert!(read_cli_config(&dir.path().join("missing.json"))
            .api_key
            .is_none());
    }
}
