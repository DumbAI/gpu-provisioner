//! Typed Verda REST API v1 client. It does not retry (the shared core
//! retries `Transient` errors) except for one thing only the client can fix:
//! a 401 on a cached token, which is answered by fetching a fresh token and
//! repeating that call once.
//!
//! Endpoints used: `GET /v1/instance-types`, `GET /v1/instance-availability`
//! (`/{instance_type}` for a single check), `GET /v1/locations`,
//! `GET|POST /v1/ssh-keys`, `GET|POST /v1/scripts`, `DELETE /v1/scripts/{id}`,
//! `GET|POST|PUT /v1/instances`, `GET /v1/volumes`, `DELETE /v1/volumes/{id}`.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use super::auth::TokenCache;
use super::error::{error_parts, ApiError};

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A price the API sends as a decimal string (`"3.17"`) or a number.
fn de_price<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

/// A nullable field documented as an untyped object but holding a string
/// (instance `ip`, volume `instance_id`).
fn de_opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    Ok(value
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty()))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GpuInfo {
    #[serde(default)]
    pub number_of_gpus: u32,
}

/// An instance type (`GET /v1/instance-types`), e.g. `1H100.80S.22V`.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceType {
    pub instance_type: String,
    /// GPU model, e.g. `H100`.
    #[serde(default)]
    pub model: String,
    /// GPU model name, e.g. `H100 SXM5 80GB`.
    #[serde(default)]
    pub name: String,
    /// e.g. `NVIDIA H100 SXM5 80GB` (documented as an object; usually a string).
    #[serde(default, deserialize_with = "de_opt_string")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub gpu: GpuInfo,
    #[serde(default, deserialize_with = "de_price")]
    pub price_per_hour: Option<f64>,
    #[serde(default, deserialize_with = "de_price")]
    pub spot_price: Option<f64>,
    /// OS image types this type can boot (`image_type` values).
    #[serde(default)]
    pub supported_os: Vec<String>,
}

/// Instance types available at one location.
#[derive(Debug, Clone, Deserialize)]
pub struct LocationAvailability {
    pub location_code: String,
    #[serde(default)]
    pub availabilities: Vec<String>,
}

/// A location (`GET /v1/locations`), e.g. `FIN-01` in `FI`.
#[derive(Debug, Clone, Deserialize)]
pub struct VerdaLocation {
    pub code: String,
    #[serde(default)]
    pub country_code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartupScript {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// An instance (`GET /v1/instances`), trimmed to what the adapter reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub id: String,
    pub status: String,
    /// Public address; `None` until assigned (or when not internet-facing).
    #[serde(default, deserialize_with = "de_opt_string")]
    pub ip: Option<String>,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub os_volume_id: Option<String>,
    #[serde(default)]
    pub volume_ids: Vec<String>,
    /// Spot instances may be evicted (`discontinued`) when capacity is needed.
    #[serde(default)]
    pub is_spot: bool,
}

/// A volume (`GET /v1/volumes`), trimmed to what cleanup reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<Tag>,
}

/// `os_volume` of a deploy request.
#[derive(Debug, Clone, Serialize)]
pub struct OsVolume {
    pub name: String,
    pub size: i32,
    pub on_spot_discontinue: String,
    pub tags: Vec<Tag>,
}

/// `POST /v1/instances` body.
#[derive(Debug, Clone, Serialize)]
pub struct DeployInstance {
    pub instance_type: String,
    pub image: String,
    pub ssh_key_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_script_id: Option<String>,
    pub location_code: String,
    pub hostname: String,
    pub description: String,
    pub is_spot: bool,
    pub os_volume: OsVolume,
    pub tags: Vec<Tag>,
}

/// `PUT /v1/instances` body for the delete action.
#[derive(Debug, Clone, Serialize)]
struct DeleteInstances<'a> {
    action: &'static str,
    id: &'a [String],
    volume_ids: &'a [String],
    delete_permanently: bool,
}

/// Per-instance result of a (multi-)instance action.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status_code: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct NewSshKey<'a> {
    name: &'a str,
    key: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct NewScript<'a> {
    name: &'a str,
    script: &'a str,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    tokens: TokenCache,
}

impl Api {
    pub fn new(base_url: &str, client_id: &str, client_secret: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            tokens: TokenCache::new(client_id, client_secret),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub async fn instance_types(&self) -> Result<Vec<InstanceType>, ApiError> {
        self.send(|| {
            self.http
                .get(self.url("/v1/instance-types"))
                .query(&[("currency", "usd")])
        })
        .await
    }

    /// Which instance types each location can launch right now.
    pub async fn availability(&self, spot: bool) -> Result<Vec<LocationAvailability>, ApiError> {
        self.send(|| {
            self.http
                .get(self.url("/v1/instance-availability"))
                .query(&[("is_spot", spot.to_string())])
        })
        .await
    }

    /// Whether `instance_type` can launch at `location` right now.
    pub async fn is_available(
        &self,
        instance_type: &str,
        location: &str,
        spot: bool,
    ) -> Result<bool, ApiError> {
        self.send(|| {
            self.http
                .get(self.url(&format!(
                    "/v1/instance-availability/{}",
                    path_segment(instance_type)
                )))
                .query(&[
                    ("is_spot", spot.to_string()),
                    ("location_code", location.to_string()),
                ])
        })
        .await
    }

    pub async fn locations(&self) -> Result<Vec<VerdaLocation>, ApiError> {
        self.send(|| self.http.get(self.url("/v1/locations"))).await
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        self.send(|| self.http.get(self.url("/v1/ssh-keys"))).await
    }

    /// Register a public key; returns its id.
    pub async fn add_ssh_key(&self, name: &str, key: &str) -> Result<String, ApiError> {
        self.send(|| {
            self.http
                .post(self.url("/v1/ssh-keys"))
                .json(&NewSshKey { name, key })
        })
        .await
    }

    pub async fn scripts(&self) -> Result<Vec<StartupScript>, ApiError> {
        self.send(|| self.http.get(self.url("/v1/scripts"))).await
    }

    /// Store a startup script; returns its id.
    pub async fn add_script(&self, name: &str, script: &str) -> Result<String, ApiError> {
        self.send(|| {
            self.http
                .post(self.url("/v1/scripts"))
                .json(&NewScript { name, script })
        })
        .await
    }

    /// Delete a startup script; an unknown one (404) is success.
    pub async fn delete_script(&self, id: &str) -> Result<(), ApiError> {
        ignore_not_found(
            self.send::<serde_json::Value>(|| {
                self.http
                    .delete(self.url(&format!("/v1/scripts/{}", path_segment(id))))
            })
            .await,
        )
    }

    /// Deploy an instance; returns its id.
    pub async fn deploy(&self, body: &DeployInstance) -> Result<String, ApiError> {
        self.send(|| self.http.post(self.url("/v1/instances")).json(body))
            .await
    }

    /// Instances carrying the tag `key=value` (filtered server-side).
    pub async fn instances_tagged(
        &self,
        key: &str,
        value: &str,
    ) -> Result<Vec<Instance>, ApiError> {
        self.send(|| {
            self.http
                .get(self.url("/v1/instances"))
                .query(&[("tag", format!("{key}={value}"))])
        })
        .await
    }

    /// Delete instances together with the given volumes, permanently. 204
    /// ("already in the requested state") answers with no body.
    pub async fn delete_instances(
        &self,
        ids: &[String],
        volume_ids: &[String],
    ) -> Result<Vec<ActionResult>, ApiError> {
        let body = DeleteInstances {
            action: "delete",
            id: ids,
            volume_ids,
            delete_permanently: true,
        };
        let results: Option<Vec<ActionResult>> = self
            .send(|| self.http.put(self.url("/v1/instances")).json(&body))
            .await?;
        Ok(results.unwrap_or_default())
    }

    pub async fn volumes(&self) -> Result<Vec<Volume>, ApiError> {
        self.send(|| self.http.get(self.url("/v1/volumes"))).await
    }

    /// Permanently delete a volume; an unknown one (404) is success.
    pub async fn delete_volume(&self, id: &str) -> Result<(), ApiError> {
        ignore_not_found(
            self.send::<serde_json::Value>(|| {
                self.http
                    .delete(self.url(&format!("/v1/volumes/{}", path_segment(id))))
                    .json(&serde_json::json!({"is_permanent": true}))
            })
            .await,
        )
    }

    /// Send with a bearer token; on 401, refresh the token and repeat once.
    async fn send<T: DeserializeOwned>(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<T, ApiError> {
        let mut refreshed = false;
        loop {
            let token = self.tokens.token(&self.http, &self.base_url).await?;
            let resp = build()
                .bearer_auth(&token)
                .send()
                .await
                .map_err(ApiError::from_reqwest)?;
            let status = resp.status();
            let body = resp.text().await.map_err(ApiError::from_reqwest)?;
            if status.as_u16() == 401 && !refreshed {
                self.tokens.invalidate().await;
                refreshed = true;
                continue;
            }
            if !status.is_success() {
                let (code, message) = error_parts(&body);
                return Err(ApiError::Status {
                    status: status.as_u16(),
                    code,
                    message,
                });
            }
            // 202/204 may answer with an empty body.
            let body = if body.trim().is_empty() {
                "null"
            } else {
                &body
            };
            return serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()));
        }
    }
}

fn ignore_not_found(result: Result<serde_json::Value, ApiError>) -> Result<(), ApiError> {
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.status() == Some(404) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Percent-encode one URL path segment.
pub fn path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_parse_from_strings_or_numbers() {
        let t: InstanceType = serde_json::from_value(serde_json::json!({
            "instance_type": "1H100.80S.22V", "model": "H100", "name": "H100 SXM5 80GB",
            "display_name": "NVIDIA H100 SXM5 80GB", "gpu": {"number_of_gpus": 1},
            "price_per_hour": "3.17", "spot_price": 0.31, "supported_os": ["ubuntu-24.04"]
        }))
        .unwrap();
        assert_eq!(t.price_per_hour, Some(3.17));
        assert_eq!(t.spot_price, Some(0.31));
        assert_eq!(t.display_name.as_deref(), Some("NVIDIA H100 SXM5 80GB"));

        let sparse: InstanceType =
            serde_json::from_value(serde_json::json!({"instance_type": "CPU.4V.16G",
                "display_name": null, "spot_price": "n/a"}))
            .unwrap();
        assert_eq!(sparse.gpu.number_of_gpus, 0);
        assert_eq!(sparse.spot_price, None);
        assert_eq!(sparse.display_name, None);
    }

    #[test]
    fn instance_ip_tolerates_null_and_objects() {
        let up: Instance = serde_json::from_value(serde_json::json!({
            "id": "i1", "status": "running", "ip": "1.2.3.4", "os_volume_id": "v1"
        }))
        .unwrap();
        assert_eq!(up.ip.as_deref(), Some("1.2.3.4"));
        let pending: Instance = serde_json::from_value(serde_json::json!({
            "id": "i1", "status": "provisioning", "ip": null, "os_volume_id": {}
        }))
        .unwrap();
        assert_eq!(pending.ip, None);
        assert_eq!(pending.os_volume_id, None);
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(path_segment("1H100.80S.22V"), "1H100.80S.22V");
        assert_eq!(path_segment("a b/c"), "a%20b%2Fc");
    }
}
