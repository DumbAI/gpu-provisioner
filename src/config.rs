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
//! [defaults]
//! regions = ["us-west-2", "us-east-1"]   # failover order when the spec has none
//! disk_gb = 100
//! ssh_user = "ubuntu"
//! ```

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub aws: AwsConfig,
    #[serde(default)]
    pub defaults: Defaults,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Ordered failover candidate regions used when the spec lists none.
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default = "default_disk_gb")]
    pub disk_gb: i32,
    #[serde(default = "default_ssh_user")]
    pub ssh_user: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            regions: Vec::new(),
            disk_gb: default_disk_gb(),
            ssh_user: default_ssh_user(),
        }
    }
}

/// Default SSH login user for provisioned VMs (Ubuntu AMIs).
pub const DEFAULT_SSH_USER: &str = "ubuntu";

fn default_disk_gb() -> i32 {
    100
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
    fn empty_config_uses_defaults() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.aws.profile.is_none());
        assert!(cfg.defaults.regions.is_empty());
        assert_eq!(cfg.defaults.disk_gb, 100);
        assert_eq!(cfg.defaults.ssh_user, "ubuntu");
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
