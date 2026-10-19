//! Typed Lambda Cloud API v1 client (`Authorization: Bearer <api_key>`). It
//! does not retry: the shared core retries `Transient` errors.
//!
//! Endpoints used: `GET /api/v1/instance-types`, `GET|POST /api/v1/ssh-keys`,
//! `GET|POST /api/v1/firewall-rulesets`, `PATCH|DELETE
//! /api/v1/firewall-rulesets/{id}`, `POST /api/v1/instance-operations/launch`,
//! `POST /api/v1/instance-operations/terminate`, `GET /api/v1/instances`
//! (cursor-paginated by `page_token`).

use std::collections::HashMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::error::{error_parts, ApiError};

/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest page the instance list allows.
const PAGE_SIZE: &str = "100";
/// Safety stop for cursor pagination.
const MAX_PAGES: usize = 100;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Specs {
    #[serde(default)]
    pub gpus: u32,
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_gib: u32,
}

/// An instance type, e.g. `gpu_1x_a10` / "1x A10 (24 GB PCIe)".
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceType {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// e.g. `A10 (24 GB PCIe)`.
    #[serde(default)]
    pub gpu_description: String,
    #[serde(default)]
    pub price_cents_per_hour: u64,
    #[serde(default)]
    pub specs: Specs,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Region {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// One entry of `GET /api/v1/instance-types`: the type plus the regions that
/// can launch it right now.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceTypeEntry {
    pub instance_type: InstanceType,
    #[serde(default)]
    pub regions_with_capacity_available: Vec<Region>,
}

#[derive(Debug, Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub key: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRef {
    pub id: String,
}

/// An inbound firewall rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallRule {
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_range: Option<[u16; 2]>,
    pub source_network: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirewallRuleset {
    pub id: String,
    pub name: String,
    pub region: Region,
    #[serde(default)]
    pub rules: Vec<FirewallRule>,
}

/// An instance (`GET /api/v1/instances`), trimmed to what the adapter reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Instance {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub status: String,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub private_ip: Option<String>,
    pub region: Region,
    #[serde(default)]
    pub tags: Vec<Tag>,
    /// When the instance first became healthy; `null` if it never has
    /// (`components/schemas/Instance.first_healthy`).
    #[serde(default)]
    pub first_healthy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstancePage {
    data: Vec<Instance>,
    #[serde(default)]
    page_token: Option<String>,
}

/// `image` of a launch request: by id or by family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ImageRef {
    Id { id: String },
    Family { family: String },
}

/// `POST /api/v1/instance-operations/launch` body.
#[derive(Debug, Clone, Serialize)]
pub struct Launch {
    pub region_name: String,
    pub instance_type_name: String,
    pub ssh_key_names: Vec<String>,
    pub name: String,
    pub hostname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageRef>,
    pub user_data: String,
    pub tags: Vec<Tag>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub firewall_rulesets: Vec<IdRef>,
}

#[derive(Debug, Deserialize)]
struct LaunchResult {
    instance_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NewSshKey<'a> {
    name: &'a str,
    public_key: &'a str,
}

#[derive(Debug, Serialize)]
struct NewRuleset<'a> {
    name: &'a str,
    region: &'a str,
    rules: &'a [FirewallRule],
}

#[derive(Debug, Serialize)]
struct RulesetRules<'a> {
    rules: &'a [FirewallRule],
}

#[derive(Debug, Serialize)]
struct Terminate<'a> {
    instance_ids: &'a [String],
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Api {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Api {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::from_reqwest)?;
        Ok(Api {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every instance type with the regions that have capacity for it now.
    pub async fn instance_types(&self) -> Result<Vec<InstanceTypeEntry>, ApiError> {
        let resp: Data<HashMap<String, InstanceTypeEntry>> = self
            .send(self.http.get(self.url("/api/v1/instance-types")))
            .await?;
        let mut entries: Vec<InstanceTypeEntry> = resp.data.into_values().collect();
        entries.sort_by(|a, b| a.instance_type.name.cmp(&b.instance_type.name));
        Ok(entries)
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        let resp: Data<Vec<SshKey>> = self
            .send(self.http.get(self.url("/api/v1/ssh-keys")))
            .await?;
        Ok(resp.data)
    }

    /// Register a public key; returns the stored key.
    pub async fn add_ssh_key(&self, name: &str, public_key: &str) -> Result<SshKey, ApiError> {
        let resp: Data<SshKey> = self
            .send(
                self.http
                    .post(self.url("/api/v1/ssh-keys"))
                    .json(&NewSshKey { name, public_key }),
            )
            .await?;
        Ok(resp.data)
    }

    pub async fn firewall_rulesets(&self) -> Result<Vec<FirewallRuleset>, ApiError> {
        let resp: Data<Vec<FirewallRuleset>> = self
            .send(self.http.get(self.url("/api/v1/firewall-rulesets")))
            .await?;
        Ok(resp.data)
    }

    pub async fn create_firewall_ruleset(
        &self,
        name: &str,
        region: &str,
        rules: &[FirewallRule],
    ) -> Result<FirewallRuleset, ApiError> {
        let resp: Data<FirewallRuleset> = self
            .send(
                self.http
                    .post(self.url("/api/v1/firewall-rulesets"))
                    .json(&NewRuleset {
                        name,
                        region,
                        rules,
                    }),
            )
            .await?;
        Ok(resp.data)
    }

    pub async fn update_firewall_rules(
        &self,
        id: &str,
        rules: &[FirewallRule],
    ) -> Result<(), ApiError> {
        self.send::<serde_json::Value>(
            self.http
                .patch(self.url(&format!("/api/v1/firewall-rulesets/{}", path_segment(id))))
                .json(&RulesetRules { rules }),
        )
        .await
        .map(|_| ())
    }

    /// Delete a firewall ruleset; an unknown one (404) is success.
    pub async fn delete_firewall_ruleset(&self, id: &str) -> Result<(), ApiError> {
        match self
            .send::<serde_json::Value>(
                self.http
                    .delete(self.url(&format!("/api/v1/firewall-rulesets/{}", path_segment(id)))),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Launch one instance; returns its id.
    pub async fn launch(&self, body: &Launch) -> Result<String, ApiError> {
        let resp: Data<LaunchResult> = self
            .send(
                self.http
                    .post(self.url("/api/v1/instance-operations/launch"))
                    .json(body),
            )
            .await?;
        resp.data
            .instance_ids
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::Decode("launch returned no instance id".into()))
    }

    /// Terminate instances; unknown ids (404) are success.
    pub async fn terminate(&self, ids: &[String]) -> Result<(), ApiError> {
        match self
            .send::<serde_json::Value>(
                self.http
                    .post(self.url("/api/v1/instance-operations/terminate"))
                    .json(&Terminate { instance_ids: ids }),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every instance on the account, following `page_token`.
    pub async fn instances(&self) -> Result<Vec<Instance>, ApiError> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query = vec![("page_size", PAGE_SIZE.to_string())];
            if let Some(t) = &token {
                query.push(("page_token", t.clone()));
            }
            let page: InstancePage = self
                .send(self.http.get(self.url("/api/v1/instances")).query(&query))
                .await?;
            all.extend(page.data);
            match page.page_token.filter(|t| !t.is_empty()) {
                Some(next) => token = Some(next),
                None => return Ok(all),
            }
        }
        Err(ApiError::Decode(format!(
            "instance list did not end after {MAX_PAGES} pages"
        )))
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ApiError> {
        let resp = req
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(ApiError::from_reqwest)?;
        let status = resp.status();
        let body = resp.text().await.map_err(ApiError::from_reqwest)?;
        if !status.is_success() {
            let (code, message) = error_parts(&body);
            return Err(ApiError::Status {
                status: status.as_u16(),
                code,
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
    fn image_refs_serialize_by_id_or_family() {
        let id = serde_json::to_value(ImageRef::Id { id: "abc".into() }).unwrap();
        assert_eq!(id, serde_json::json!({"id": "abc"}));
        let family = serde_json::to_value(ImageRef::Family {
            family: "lambda-stack-24-04".into(),
        })
        .unwrap();
        assert_eq!(family, serde_json::json!({"family": "lambda-stack-24-04"}));
    }

    #[test]
    fn firewall_rules_omit_port_range_when_absent() {
        let rule = FirewallRule {
            protocol: "icmp".into(),
            port_range: None,
            source_network: "0.0.0.0/0".into(),
            description: "ping".into(),
        };
        let value = serde_json::to_value(rule).unwrap();
        assert!(value.get("port_range").is_none());
    }
}
