//! Typed DigitalOcean API v2 client (`Authorization: Bearer <token>`). It
//! does not retry: the shared core retries `Transient` errors.
//!
//! Endpoints used (spec: <https://docs.digitalocean.com/reference/api/digitalocean/>):
//! `GET /v2/sizes`, `GET|POST /v2/account/keys`, `GET|POST /v2/tags`,
//! `DELETE /v2/tags/{tag}`, `GET|POST /v2/firewalls`,
//! `PUT|DELETE /v2/firewalls/{id}`, `GET|POST /v2/droplets`,
//! `DELETE /v2/droplets/{id}`. Lists are paginated by `page` / `per_page`
//! and end when `links.pages.next` is absent.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::error::{error_parts, ApiError};

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest page the list endpoints allow.
const PER_PAGE: &str = "200";
/// Safety stop for pagination.
const MAX_PAGES: usize = 100;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `size.gpu_info.vram`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Vram {
    #[serde(default)]
    pub amount: u32,
    #[serde(default)]
    pub unit: String,
}

/// `size.gpu_info`: present on GPU sizes only.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GpuInfo {
    #[serde(default)]
    pub count: u32,
    /// e.g. `nvidia_h100`, `amd_mi300x`.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub vram: Vram,
}

/// A Droplet plan (`GET /v2/sizes`), e.g. `gpu-h100x1-80gb`.
#[derive(Debug, Clone, Deserialize)]
pub struct Size {
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub vcpus: u32,
    /// MiB.
    #[serde(default)]
    pub memory: u64,
    /// GiB.
    #[serde(default)]
    pub disk: u64,
    #[serde(default)]
    pub price_hourly: f64,
    /// Region slugs where this size can be created.
    #[serde(default)]
    pub regions: Vec<String>,
    /// Whether the size can be created at all right now.
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub gpu_info: Option<GpuInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    pub id: u64,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub name: String,
}

/// Traffic endpoints of a firewall rule (only addresses are used).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addresses {
    #[serde(default)]
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundRule {
    pub protocol: String,
    /// A port, a range (`8000-9000`) or `0` (all); omitted for ICMP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<String>,
    pub sources: Addresses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundRule {
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<String>,
    pub destinations: Addresses,
}

/// `POST /v2/firewalls` / `PUT /v2/firewalls/{id}` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirewallSpec {
    pub name: String,
    pub inbound_rules: Vec<InboundRule>,
    pub outbound_rules: Vec<OutboundRule>,
    /// Droplets carrying any of these tags get the firewall.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Firewall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkV4 {
    pub ip_address: String,
    /// `public` or `private`.
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Networks {
    #[serde(default)]
    pub v4: Vec<NetworkV4>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DropletRegion {
    #[serde(default)]
    pub slug: String,
}

/// A Droplet (`GET /v2/droplets`), trimmed to what the adapter reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Droplet {
    pub id: u64,
    pub name: String,
    /// `new`, `active`, `off` or `archive`.
    pub status: String,
    /// Set while an action (resize, snapshot, …) holds the Droplet.
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub networks: Networks,
    #[serde(default)]
    pub region: DropletRegion,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Droplet {
    /// The first address of `kind` (`public` / `private`).
    pub fn ipv4(&self, kind: &str) -> Option<&str> {
        self.networks
            .v4
            .iter()
            .find(|n| n.kind == kind && !n.ip_address.is_empty())
            .map(|n| n.ip_address.as_str())
    }
}

/// `POST /v2/droplets` body (single Droplet).
#[derive(Debug, Clone, Serialize)]
pub struct NewDroplet {
    pub name: String,
    pub region: String,
    pub size: String,
    /// An image slug (`gpu-h100x1-base`) or numeric id.
    pub image: serde_json::Value,
    pub ssh_keys: Vec<u64>,
    pub tags: Vec<String>,
    pub user_data: String,
}

#[derive(Debug, Deserialize)]
struct DropletEnvelope {
    droplet: Droplet,
}

#[derive(Debug, Deserialize)]
struct SshKeyEnvelope {
    ssh_key: SshKey,
}

#[derive(Debug, Deserialize)]
struct FirewallEnvelope {
    firewall: Firewall,
}

#[derive(Debug, Serialize)]
struct NewSshKey<'a> {
    name: &'a str,
    public_key: &'a str,
}

#[derive(Debug, Serialize)]
struct NewTag<'a> {
    name: &'a str,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Api {
    pub fn new(base_url: &str, token: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every Droplet size with the regions it can be created in.
    pub async fn sizes(&self) -> Result<Vec<Size>, ApiError> {
        self.list("/v2/sizes", "sizes", &[]).await
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        self.list("/v2/account/keys", "ssh_keys", &[]).await
    }

    /// Register a public key; returns the stored key.
    pub async fn add_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKey, ApiError> {
        let resp: SshKeyEnvelope = self
            .send(
                self.http
                    .post(self.url("/v2/account/keys"))
                    .json(&NewSshKey { name, public_key }),
            )
            .await?;
        Ok(resp.ssh_key)
    }

    /// Whether a tag exists (`GET /v2/tags/{tag}`; 404 → `false`).
    pub async fn tag_exists(&self, name: &str) -> Result<bool, ApiError> {
        match self
            .send::<serde_json::Value>(
                self.http
                    .get(self.url(&format!("/v2/tags/{}", path_segment(name)))),
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.status() == Some(404) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn create_tag(&self, name: &str) -> Result<(), ApiError> {
        self.send::<serde_json::Value>(self.http.post(self.url("/v2/tags")).json(&NewTag { name }))
            .await
            .map(|_| ())
    }

    /// Delete a tag (untagging its resources); an unknown one is success.
    pub async fn delete_tag(&self, name: &str) -> Result<(), ApiError> {
        not_found_ok(
            self.send::<serde_json::Value>(
                self.http
                    .delete(self.url(&format!("/v2/tags/{}", path_segment(name)))),
            )
            .await,
        )
    }

    pub async fn firewalls(&self) -> Result<Vec<Firewall>, ApiError> {
        self.list("/v2/firewalls", "firewalls", &[]).await
    }

    pub async fn create_firewall(&self, spec: &FirewallSpec) -> Result<Firewall, ApiError> {
        let resp: FirewallEnvelope = self
            .send(self.http.post(self.url("/v2/firewalls")).json(spec))
            .await?;
        Ok(resp.firewall)
    }

    /// Replace a firewall's rules and tags (`PUT` takes the full object).
    pub async fn update_firewall(&self, id: &str, spec: &FirewallSpec) -> Result<(), ApiError> {
        self.send::<serde_json::Value>(
            self.http
                .put(self.url(&format!("/v2/firewalls/{}", path_segment(id))))
                .json(spec),
        )
        .await
        .map(|_| ())
    }

    /// Delete a firewall; an unknown one is success.
    pub async fn delete_firewall(&self, id: &str) -> Result<(), ApiError> {
        not_found_ok(
            self.send::<serde_json::Value>(
                self.http
                    .delete(self.url(&format!("/v2/firewalls/{}", path_segment(id)))),
            )
            .await,
        )
    }

    /// Create one Droplet; returns its id.
    pub async fn create_droplet(&self, body: &NewDroplet) -> Result<u64, ApiError> {
        let resp: DropletEnvelope = self
            .send(self.http.post(self.url("/v2/droplets")).json(body))
            .await?;
        Ok(resp.droplet.id)
    }

    /// Droplets carrying `tag`.
    pub async fn droplets_tagged(&self, tag: &str) -> Result<Vec<Droplet>, ApiError> {
        self.list("/v2/droplets", "droplets", &[("tag_name", tag)])
            .await
    }

    /// Delete a Droplet; an unknown one (already gone) is success.
    pub async fn delete_droplet(&self, id: &str) -> Result<(), ApiError> {
        not_found_ok(
            self.send::<serde_json::Value>(
                self.http
                    .delete(self.url(&format!("/v2/droplets/{}", path_segment(id)))),
            )
            .await,
        )
    }

    /// Every item under `key` across pages, following `links.pages.next`.
    /// Pages are requested by number on our base URL (rather than by the
    /// absolute `next` link) so a configured endpoint is never left.
    async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        key: &str,
        filter: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let page = page.to_string();
            let mut query: Vec<(&str, &str)> = vec![("page", &page), ("per_page", PER_PAGE)];
            query.extend_from_slice(filter);
            let mut body: serde_json::Value = self
                .send(self.http.get(self.url(path)).query(&query))
                .await?;
            let items: Vec<T> = serde_json::from_value(body[key].take())
                .map_err(|e| ApiError::Decode(format!("{path} `{key}`: {e}")))?;
            all.extend(items);
            if body["links"]["pages"]["next"].as_str().is_none() {
                return Ok(all);
            }
        }
        Err(ApiError::Decode(format!(
            "{path} did not end after {MAX_PAGES} pages"
        )))
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(ApiError::from_reqwest)?;
        let status = resp.status();
        let body = resp.text().await.map_err(ApiError::from_reqwest)?;
        if !status.is_success() {
            let (id, message) = error_parts(&body);
            return Err(ApiError::Status {
                status: status.as_u16(),
                id,
                message,
            });
        }
        let body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
    }
}

/// Deletes are idempotent: a 404 means it is already gone.
fn not_found_ok(result: Result<serde_json::Value, ApiError>) -> Result<(), ApiError> {
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
    fn icmp_rules_omit_ports() {
        let rule = InboundRule {
            protocol: "icmp".into(),
            ports: None,
            sources: Addresses {
                addresses: vec!["0.0.0.0/0".into()],
            },
        };
        assert_eq!(
            serde_json::to_value(rule).unwrap(),
            serde_json::json!({"protocol": "icmp", "sources": {"addresses": ["0.0.0.0/0"]}})
        );
    }

    #[test]
    fn droplet_addresses_by_type() {
        let droplet: Droplet = serde_json::from_value(serde_json::json!({
            "id": 3164444, "name": "gpuprov-dev", "status": "active",
            "networks": {"v4": [
                {"ip_address": "10.128.192.124", "netmask": "255.255.0.0", "gateway": "nil", "type": "private"},
                {"ip_address": "192.241.165.154", "netmask": "255.255.255.0", "gateway": "192.241.165.1", "type": "public"}
            ], "v6": []},
            "region": {"slug": "nyc2", "name": "New York 2"}
        }))
        .unwrap();
        assert_eq!(droplet.ipv4("public"), Some("192.241.165.154"));
        assert_eq!(droplet.ipv4("private"), Some("10.128.192.124"));
        assert!(!droplet.locked && droplet.tags.is_empty());
    }
}
