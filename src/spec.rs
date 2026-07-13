//! `VmSpec`: the user's request — from a YAML file, CLI flags, or both
//! (flags override the file). The mini analog of SkyPilot's task YAML +
//! CLI overrides.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSpec {
    pub name: Option<String>,
    /// Exact cloud instance type (e.g. `g5.xlarge`). Mutually exclusive with `gpus`.
    pub instance_type: Option<String>,
    /// GPU shorthand (e.g. `A10G:1`), resolved by the cloud provider's
    /// catalog into one or more instance types.
    pub gpus: Option<String>,
    /// Ordered failover candidate regions; falls back to config `defaults.regions`.
    #[serde(default)]
    pub regions: Vec<String>,
    /// Optional zone pin (e.g. `us-west-2a`); restricts to that zone's region.
    pub zone: Option<String>,
    /// AMI override; defaults to an SSM public-parameter lookup per region.
    pub ami: Option<String>,
    pub disk_gb: Option<i32>,
    #[serde(default)]
    pub spot: bool,
    /// Install a single-node k3s cluster on the VM (via cloud-init) and open
    /// the Kubernetes API + ingress ports (6443/80/443).
    #[serde(default)]
    pub kubernetes: bool,
    /// Extra security-group ingress ports besides SSH (22).
    #[serde(default)]
    pub ports: Vec<u16>,
    pub ssh_user: Option<String>,
}

/// CLI flag overrides applied on top of an optional spec file.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub name: Option<String>,
    pub instance_type: Option<String>,
    pub gpus: Option<String>,
    pub regions: Vec<String>,
    pub zone: Option<String>,
    pub ami: Option<String>,
    pub disk_gb: Option<i32>,
    pub spot: bool,
    pub kubernetes: bool,
    pub ports: Vec<u16>,
    pub ssh_user: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("cannot read spec file '{path}': {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid spec file '{path}': {source}")]
    Parse {
        path: String,
        source: serde_yaml::Error,
    },
    #[error("a VM name is required (--name or `name:` in the spec file)")]
    MissingName,
    #[error("invalid VM name '{0}': use lowercase letters, digits and '-', starting with a letter (max 40 chars)")]
    InvalidName(String),
    #[error("exactly one of instance_type or gpus must be set")]
    InstanceTypeXorGpus,
}

impl VmSpec {
    pub fn from_yaml_file(path: &Path) -> Result<Self, SpecError> {
        let content = std::fs::read_to_string(path).map_err(|source| SpecError::Io {
            path: path.display().to_string(),
            source,
        })?;
        serde_yaml::from_str(&content).map_err(|source| SpecError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Apply CLI overrides (flags win; list flags replace, `--spot` can only enable).
    pub fn merged_with(mut self, cli: CliOverrides) -> Self {
        if cli.name.is_some() {
            self.name = cli.name;
        }
        if cli.instance_type.is_some() {
            self.instance_type = cli.instance_type;
            self.gpus = None; // an explicit instance type overrides a file's gpus
        }
        if cli.gpus.is_some() {
            self.gpus = cli.gpus;
            self.instance_type = None;
        }
        if !cli.regions.is_empty() {
            self.regions = cli.regions;
        }
        if cli.zone.is_some() {
            self.zone = cli.zone;
        }
        if cli.ami.is_some() {
            self.ami = cli.ami;
        }
        if cli.disk_gb.is_some() {
            self.disk_gb = cli.disk_gb;
        }
        self.spot |= cli.spot;
        self.kubernetes |= cli.kubernetes;
        if !cli.ports.is_empty() {
            self.ports = cli.ports;
        }
        if cli.ssh_user.is_some() {
            self.ssh_user = cli.ssh_user;
        }
        self
    }

    /// Validate and normalize the VM name (the identity used for cloud tags,
    /// the security group name, the SSH host alias, and the DB key).
    pub fn validated_name(&self) -> Result<String, SpecError> {
        let raw = self.name.as_deref().ok_or(SpecError::MissingName)?;
        let name = sanitize_name(raw);
        let valid = !name.is_empty()
            && name.len() <= 40
            && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(SpecError::InvalidName(raw.to_string()));
        }
        Ok(name)
    }
}

/// Lowercase and map `_`/`.` to `-` (mirrors SkyPilot's
/// `make_cluster_name_on_cloud` sanitization; no user-hash suffix needed for
/// a single-user local tool).
fn sanitize_name(raw: &str) -> String {
    raw.to_lowercase().replace(['_', '.'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_win() {
        let spec = VmSpec {
            name: Some("from-file".into()),
            instance_type: Some("t3.micro".into()),
            regions: vec!["us-east-1".into()],
            disk_gb: Some(50),
            ..Default::default()
        };
        let merged = spec.merged_with(CliOverrides {
            name: Some("from-cli".into()),
            gpus: Some("A10G:1".into()),
            regions: vec!["us-west-2".into()],
            spot: true,
            ..Default::default()
        });
        assert_eq!(merged.name.as_deref(), Some("from-cli"));
        // --gpus displaces the file's instance_type.
        assert!(merged.instance_type.is_none());
        assert_eq!(merged.gpus.as_deref(), Some("A10G:1"));
        assert_eq!(merged.regions, vec!["us-west-2"]);
        assert!(merged.spot);
        assert_eq!(merged.disk_gb, Some(50)); // untouched
    }

    #[test]
    fn kubernetes_flag_is_enable_only() {
        // A file with kubernetes=true stays true even if the CLI doesn't set it.
        let spec = VmSpec {
            kubernetes: true,
            ..Default::default()
        };
        assert!(spec.merged_with(CliOverrides::default()).kubernetes);
        // The CLI flag can turn it on...
        let spec = VmSpec::default().merged_with(CliOverrides {
            kubernetes: true,
            ..Default::default()
        });
        assert!(spec.kubernetes);
        // ...but never off (mirrors --spot).
        let spec = VmSpec {
            kubernetes: true,
            ..Default::default()
        }
        .merged_with(CliOverrides {
            kubernetes: false,
            ..Default::default()
        });
        assert!(spec.kubernetes);
    }

    #[test]
    fn name_validation() {
        let mut spec = VmSpec {
            name: Some("My_Test.VM".into()),
            ..Default::default()
        };
        assert_eq!(spec.validated_name().unwrap(), "my-test-vm");
        spec.name = Some("9bad".into());
        assert!(matches!(
            spec.validated_name(),
            Err(SpecError::InvalidName(_))
        ));
        spec.name = None;
        assert!(matches!(spec.validated_name(), Err(SpecError::MissingName)));
    }
}
