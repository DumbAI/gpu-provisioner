//! CLI config file: AWS credentials and provisioning defaults.
//!
//! Location: `--config <path>` or `~/.gpu-provisioner/config.toml` (optional).
//!
//! ```toml
//! [aws]
//! profile = "default"          # or explicit static credentials:
//! # access_key_id = "..."
//! # secret_access_key = "..."
//! # session_token = "..."      # optional
//!
//! [primeintellect]
//! api_key = "..."               # else $PRIME_API_KEY, else ~/.prime/config.json
//! # team_id = "..."             # bill pods to a team
//! # regions = ["united_states"] # default region filter (empty = anywhere)
//!
//! [runpod]
//! api_key = "..."               # else $RUNPOD_API_KEY, else ~/.runpod/config.toml
//! # regions = ["north_america"] # continents or ISO country codes (empty = anywhere)
//!
//! [verda]
//! client_id = "..."             # else $VERDA_CLIENT_ID
//! client_secret = "..."         # else $VERDA_CLIENT_SECRET
//! # regions = ["FIN-01"]        # location codes or ISO country codes (empty = anywhere)
//!
//! [massedcompute]
//! api_key = "..."               # else $MASSED_COMPUTE_API_KEY
//!
//! [hyperstack]
//! api_key = "..."               # else $HYPERSTACK_API_KEY
//! # regions = ["CANADA-1"]      # default: North American regions
//!
//! [digitalocean]
//! api_key = "..."               # else $DIGITALOCEAN_ACCESS_TOKEN
//! # regions = ["tor1"]          # default: North American regions
//!
//! [lambda]
//! api_key = "..."               # else $LAMBDA_API_KEY
//!
//! [gcp]
//! project = "my-project"        # else $GOOGLE_CLOUD_PROJECT / gcloud config
//! # regions = ["us-central1"]   # default failover order
//!
//! [docker]                      # local test cloud (sshd containers)
//! # image = "gpuprov/sshd:latest"
//!
//! [defaults]
//! regions = ["us-west-2", "us-east-1"]   # AWS failover order when the spec has none
//! disk_gb = 100
//! ssh_user = "ubuntu"
//! kubernetes = true             # every VM becomes a k3s node; false to opt out
//! ```

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub aws: AwsConfig,
    #[serde(default)]
    pub primeintellect: PrimeIntellectConfig,
    #[serde(default)]
    pub gcp: GcpConfig,
    #[serde(default)]
    pub runpod: RunpodConfig,
    #[serde(default)]
    pub verda: VerdaConfig,
    #[serde(default)]
    pub massedcompute: ApiKeyConfig,
    #[serde(default)]
    pub hyperstack: ApiKeyConfig,
    #[serde(default)]
    pub lambda: ApiKeyConfig,
    #[serde(default)]
    pub digitalocean: ApiKeyConfig,
    #[serde(default)]
    pub docker: DockerConfig,
    #[serde(default)]
    pub defaults: Defaults,
}

impl AppConfig {
    /// Region preference used when the spec lists none. Region names are
    /// cloud-specific, so each provider section may carry its own;
    /// `defaults.regions` serves the clouds without one (AWS).
    pub fn default_regions(&self, provider: &str) -> &[String] {
        match provider {
            "primeintellect" => &self.primeintellect.regions,
            "gcp" => &self.gcp.regions,
            "runpod" => &self.runpod.regions,
            "verda" => &self.verda.regions,
            "massedcompute" => &self.massedcompute.regions,
            "hyperstack" => &self.hyperstack.regions,
            "lambda" => &self.lambda.regions,
            "digitalocean" => &self.digitalocean.regions,
            "docker" => &self.docker.regions,
            _ => &self.defaults.regions,
        }
    }
}

/// AWS credential selection. Resolution order:
/// explicit static keys → named profile → default AWS chain (env, `~/.aws`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    pub profile: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    /// Send every EC2/SSM call to this endpoint instead of AWS (an
    /// AWS-compatible emulator such as moto or LocalStack).
    pub endpoint_url: Option<String>,
}

/// Prime Intellect API access. Key resolution order: `api_key` here →
/// `$PRIME_API_KEY` → the prime CLI's `~/.prime/config.json` (written by
/// `prime login`) — resolved by the adapter at construction.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimeIntellectConfig {
    pub api_key: Option<String>,
    pub team_id: Option<String>,
    /// API base URL override (testing / staging).
    pub base_url: Option<String>,
    /// Default region filter (Prime Intellect region ids such as
    /// `united_states`, `canada`, `eu_west`); empty = any region.
    #[serde(default)]
    pub regions: Vec<String>,
}

/// Google Cloud (Compute Engine REST). Token resolution: `access_token`
/// here → `$GOOGLE_OAUTH_ACCESS_TOKEN` → `gcloud auth print-access-token`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcpConfig {
    /// Project id; else `$GOOGLE_CLOUD_PROJECT`, else `gcloud config get project`.
    pub project: Option<String>,
    /// Static OAuth access token (short-lived; mostly for CI/testing).
    pub access_token: Option<String>,
    /// API base URL override (testing).
    pub base_url: Option<String>,
    /// Default failover regions (e.g. `us-central1`); zones are derived.
    #[serde(default)]
    pub regions: Vec<String>,
}

/// RunPod (REST API v2). Key resolution order: `api_key` here →
/// `$RUNPOD_API_KEY` → the runpod CLI's `~/.runpod/config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunpodConfig {
    pub api_key: Option<String>,
    /// API base URL override (testing).
    pub base_url: Option<String>,
    /// Default region filter: continents (`north_america`, `europe`, …) or
    /// ISO country codes (`US`, `CA`); empty = any data center.
    #[serde(default)]
    pub regions: Vec<String>,
    /// Container image for pods; must start sshd from `$PUBLIC_KEY` (all
    /// official `runpod/*` images do). Default: `runpod/base` with CUDA.
    pub image: Option<String>,
}

/// Verda (REST API v1, OAuth client credentials). Resolution order:
/// `client_id`/`client_secret` here → `$VERDA_CLIENT_ID` /
/// `$VERDA_CLIENT_SECRET`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerdaConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// API base URL override (testing).
    pub base_url: Option<String>,
    /// Default region filter: location codes (`FIN-01`) or ISO country
    /// codes (`FI`, `IS`); empty = any location.
    #[serde(default)]
    pub regions: Vec<String>,
}

/// A cloud authenticated by a single API key (Massed Compute, Hyperstack,
/// Lambda, DigitalOcean). Each adapter documents its key's environment-variable fallback.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyConfig {
    pub api_key: Option<String>,
    /// API base URL override (testing).
    pub base_url: Option<String>,
    /// Default region filter (the cloud's own region names); empty = the
    /// adapter's default (documented per cloud).
    #[serde(default)]
    pub regions: Vec<String>,
}

/// The local Docker "cloud" used for end-to-end tests: each VM is an sshd
/// container on this machine.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// Image to run; built from the embedded Dockerfile when absent locally.
    pub image: Option<String>,
    /// Default region label (only `local` exists).
    #[serde(default)]
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Ordered failover candidate regions used when the spec lists none
    /// (for clouds without their own `regions` setting — see
    /// `AppConfig::default_regions`).
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default = "default_disk_gb")]
    pub disk_gb: i32,
    #[serde(default = "default_ssh_user")]
    pub ssh_user: String,
    /// Make every VM a single-node k3s cluster unless the spec or
    /// `--no-k8s` opts out (default: true).
    #[serde(default = "default_kubernetes")]
    pub kubernetes: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            disk_gb: default_disk_gb(),
            ssh_user: default_ssh_user(),
            kubernetes: default_kubernetes(),
        }
    }
}

/// Default SSH login user for provisioned VMs (Ubuntu AMIs).
pub const DEFAULT_SSH_USER: &str = "ubuntu";

fn default_disk_gb() -> i32 {
    100
}

fn default_kubernetes() -> bool {
    true
}

fn default_ssh_user() -> String {
    DEFAULT_SSH_USER.to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file '{path}': {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid config file '{path}': {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("config: aws.access_key_id and aws.secret_access_key must be set together")]
    PartialStaticCredentials,
}

impl AppConfig {
    /// Load the config. An explicitly given path must exist; the default path
    /// is optional (missing → built-in defaults).
    pub fn load(explicit: Option<&Path>, default_path: &Path) -> Result<Self, ConfigError> {
        let (path, required) = match explicit {
            Some(p) => (p, true),
            None => (default_path, false),
        };
        let config = if path.exists() || required {
            let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
                path: path.display().to_string(),
                source,
            })?;
            toml::from_str(&content).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?
        } else {
            Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.aws.access_key_id.is_some() != self.aws.secret_access_key.is_some() {
            return Err(ConfigError::PartialStaticCredentials);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: AppConfig = toml::from_str(
            r#"
            [aws]
            profile = "work"

            [defaults]
            regions = ["us-west-2", "us-east-1"]
            disk_gb = 200
            ssh_user = "ec2-user"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.aws.profile.as_deref(), Some("work"));
        assert_eq!(cfg.defaults.regions, vec!["us-west-2", "us-east-1"]);
        assert_eq!(cfg.defaults.disk_gb, 200);
        assert_eq!(cfg.defaults.ssh_user, "ec2-user");
    }

    #[test]
    fn default_regions_are_per_provider() {
        let cfg: AppConfig = toml::from_str(
            r#"
            [primeintellect]
            api_key = "pk"
            regions = ["canada"]

            [defaults]
            regions = ["us-west-2"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.primeintellect.api_key.as_deref(), Some("pk"));
        assert_eq!(cfg.default_regions("primeintellect"), ["canada"]);
        assert_eq!(cfg.default_regions("aws"), ["us-west-2"]);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.aws.profile.is_none());
        assert!(cfg.defaults.regions.is_empty());
        assert_eq!(cfg.defaults.disk_gb, 100);
        assert_eq!(cfg.defaults.ssh_user, "ubuntu");
        assert!(cfg.defaults.kubernetes, "k3s is on by default");
    }

    #[test]
    fn rejects_partial_static_credentials() {
        let cfg: AppConfig = toml::from_str(
            r#"
            [aws]
            access_key_id = "AKIA..."
            "#,
        )
        .unwrap();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::PartialStaticCredentials)
        ));
    }
}
