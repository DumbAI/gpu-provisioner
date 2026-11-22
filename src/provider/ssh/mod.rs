//! `ssh`: machines you already have (a workstation, a lab server, a DGX
//! Spark) used as a cloud. Each configured host is a region of its own;
//! "creating a VM" adopts the host, "deleting" it gives the host back. The
//! host's own credentials are used only for those two steps; in between
//! the host is reached like any VM, with gpuprov's key, so the launch and
//! k3s machines run unchanged.
//!
//! | primitive | SSH host |
//! |-----------|----------|
//! | plan      | one offer per configured host that matches the ask (`--gpus` against its `gpus`, `--instance-type` against its id); `--region` names hosts |
//! | prepare   | nothing |
//! | create    | `claim.sh` with the host's credentials: passwordless sudo and systemd present, no k3s but gpuprov's; claim the host for this VM (`<state>/ssh-claim`, created exclusively); authorize gpuprov's public key |
//! | observe   | `observe.sh`: the claim names this VM → running at the host's address, port and user |
//! | delete    | `release.sh`: uninstall gpuprov's k3s (unless `keep_k3s`), remove the key the claim added, drop the claim. The machine is never powered off |
//! | cleanup   | nothing |
//!
//! A host serves one VM at a time: a claim by another VM is `Capacity`, so
//! failover moves on to the next matching host.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{SshConfig, SshHostConfig};
use crate::provider::{
    Capabilities, CloudProvider, Feature, GpuRequest, InstanceSpec, InstanceStatus, Location,
    Observed, Offer, ProviderError, ResourceAsk, ResourceRequest, SshEndpoint,
};
use crate::ssh::options::SshOptions;
use crate::ssh::runner;

/// Canonical provider id recorded on VM rows.
pub const PROVIDER_ID: &str = "ssh";

/// Where the claim lives on a host (with the k3s installer's status marker).
pub const STATE_DIR: &str = "/var/lib/gpuprov";

const CLAIM: &str = include_str!("claim.sh");
const OBSERVE: &str = include_str!("observe.sh");
const RELEASE: &str = include_str!("release.sh");

/// `claim.sh` / `release.sh` exit codes.
const EXIT_NO_SUDO: i32 = 10;
const EXIT_NO_SYSTEMD: i32 = 11;
const EXIT_CLAIMED: i32 = 12;
const EXIT_FOREIGN_K3S: i32 = 13;
/// `ssh` itself failed (connect, auth).
const EXIT_SSH: i32 = 255;

/// Instance type of a host that offers no GPU.
pub const CPU_TYPE: &str = "cpu";

const UNSUPPORTED: &[(Feature, &str)] = &[
    (Feature::Spot, "your own machines have no spot market"),
    (
        Feature::OpenPorts,
        "gpuprov does not manage your machine's firewall (k3s is reached over SSH)",
    ),
    (
        Feature::ImageOverride,
        "the machine keeps the operating system it has",
    ),
];

/// Runs a script on a host as its configured user: `bash -s -- <args>`,
/// the script on stdin. `Err` only when nothing could run locally.
#[async_trait]
pub trait HostShell: Send + Sync {
    async fn run(
        &self,
        host: &SshHostConfig,
        script: &str,
        args: &[String],
    ) -> std::io::Result<std::process::Output>;
}

/// The system OpenSSH client, with the host's own key.
pub struct OpenSsh;

#[async_trait]
impl HostShell for OpenSsh {
    async fn run(
        &self,
        host: &SshHostConfig,
        script: &str,
        args: &[String],
    ) -> std::io::Result<std::process::Output> {
        let opts = SshOptions::new(expand_home(&host.identity_file))
            .port(host.port)
            .batch_mode(true)
            .connect_timeout_secs(15);
        let mut command = vec!["bash".to_string(), "-s".to_string(), "--".to_string()];
        command.extend(args.iter().cloned());
        runner::run_command_with_input(&host.user, &host.host, &opts, &command, script.as_bytes())
            .await
    }
}

pub struct SshProvider {
    config: SshConfig,
    shell: Box<dyn HostShell>,
    state_dir: String,
}

impl SshProvider {
    pub fn new(config: &SshConfig) -> Self {
        Self::with_shell(config, Box::new(OpenSsh), STATE_DIR)
    }

    /// With another way to reach hosts and another state directory (tests).
    pub fn with_shell(config: &SshConfig, shell: Box<dyn HostShell>, state_dir: &str) -> Self {
        SshProvider {
            config: config.clone(),
            shell,
            state_dir: state_dir.to_string(),
        }
    }

    fn host(&self, id: &str) -> Result<&SshHostConfig, ProviderError> {
        self.config.hosts.get(id).ok_or_else(|| {
            ProviderError::invalid(format!(
                "no ssh host '{id}' in [ssh.hosts] (configured: {})",
                self.known()
            ))
        })
    }

    fn known(&self) -> String {
        if self.config.hosts.is_empty() {
            "none".to_string()
        } else {
            self.config
                .hosts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    async fn run(
        &self,
        id: &str,
        script: &str,
        args: Vec<String>,
    ) -> Result<std::process::Output, ProviderError> {
        let host = self.host(id)?;
        self.shell
            .run(host, script, &args)
            .await
            .map_err(|e| ProviderError::transient(format!("ssh host '{id}': cannot run ssh: {e}")))
    }
}

#[async_trait]
impl CloudProvider for SshProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: UNSUPPORTED,
            boot_script: false,
            boot_timeout: Duration::from_secs(120),
            // Releasing uninstalls k3s, synchronously, inside `delete`.
            delete_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(2),
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        plan_hosts(&self.config, request)
    }

    async fn prepare(
        &self,
        _offer: &Offer,
        _spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        Ok(serde_json::Value::Null)
    }

    async fn create(
        &self,
        offer: &Offer,
        _prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let id = &offer.region;
        let out = self
            .run(
                id,
                CLAIM,
                vec![
                    self.state_dir.clone(),
                    spec.vm_name.clone(),
                    spec.ssh_public_key.trim().to_string(),
                ],
            )
            .await?;
        classify(Step::Claim, id, &out)?;
        Ok(id.clone())
    }

    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let id = &at.region;
        let out = self.run(id, OBSERVE, vec![self.state_dir.clone()]).await?;
        classify(Step::Observe, id, &out)?;
        let owner = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if owner != vm_name {
            return Ok(Vec::new());
        }
        let host = self.host(id)?;
        Ok(vec![Observed {
            instance_id: id.clone(),
            status: InstanceStatus::Running,
            zone: None,
            private_ip: None,
            endpoint: Some(SshEndpoint {
                host: host.host.clone(),
                port: host.port,
                user: Some(host.user.clone()),
            }),
        }])
    }

    async fn delete(
        &self,
        at: &Location,
        vm_name: &str,
        _instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        let id = &at.region;
        let keep = if self.host(id)?.keep_k3s { "1" } else { "0" };
        let out = self
            .run(
                id,
                RELEASE,
                vec![
                    self.state_dir.clone(),
                    vm_name.to_string(),
                    keep.to_string(),
                ],
            )
            .await?;
        classify(Step::Release, id, &out)
    }

    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// The configured hosts that can serve `request`, in id order. Pure.
fn plan_hosts(config: &SshConfig, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
    if config.hosts.is_empty() {
        return Err(ProviderError::invalid(
            "no ssh hosts configured: add one under [ssh.hosts.<id>] in gpuprov's config",
        ));
    }
    if let Some(zone) = &request.zone {
        return Err(ProviderError::invalid(format!(
            "ssh hosts have no zones (got '{zone}')"
        )));
    }
    if let Some(bad) = request
        .regions
        .iter()
        .find(|r| !config.hosts.contains_key(*r))
    {
        return Err(ProviderError::invalid(format!(
            "no ssh host '{bad}' (configured: {})",
            config.hosts.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    let named = |id: &String| request.regions.is_empty() || request.regions.contains(id);
    let mut offers = Vec::new();
    for (id, host) in config.hosts.iter().filter(|(id, _)| named(id)) {
        let gpus = host
            .gpus
            .as_deref()
            .map(|g| {
                g.parse::<GpuRequest>().map_err(|e| {
                    ProviderError::invalid(format!("ssh host '{id}': gpus = \"{g}\": {e}"))
                })
            })
            .transpose()?;
        let fits = match &request.ask {
            ResourceAsk::InstanceType(t) => t == id || *t == instance_type(gpus.as_ref()),
            ResourceAsk::Gpus(want) => gpus
                .as_ref()
                .is_some_and(|have| have.name == want.name && have.count >= want.count),
        };
        if fits {
            offers.push(Offer::new(instance_type(gpus.as_ref()), id.clone()));
        }
    }
    if offers.is_empty() {
        let ask = match &request.ask {
            ResourceAsk::InstanceType(t) => format!("instance type '{t}'"),
            ResourceAsk::Gpus(g) => format!("{g}"),
        };
        return Err(ProviderError::invalid(format!(
            "no ssh host offers {ask} ({})",
            config
                .hosts
                .iter()
                .map(|(id, h)| format!("{id}: {}", h.gpus.as_deref().unwrap_or(CPU_TYPE)))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(offers)
}

/// A host's instance type: its GPUs (`GB10:1`), or `cpu`.
fn instance_type(gpus: Option<&GpuRequest>) -> String {
    gpus.map_or_else(
        || CPU_TYPE.to_string(),
        |g| format!("{}:{}", g.name, g.count),
    )
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Claim,
    Observe,
    Release,
}

/// A script's outcome as the core's error taxonomy. Pure.
fn classify(step: Step, id: &str, out: &std::process::Output) -> Result<(), ProviderError> {
    let code = out.status.code().unwrap_or(EXIT_SSH);
    if code == 0 {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let why = format!("ssh host '{id}': {stderr}");
    Err(match code {
        EXIT_SSH if stderr.contains("Permission denied") => ProviderError::credentials(format!(
            "{why} (check user and identity_file in [ssh.hosts.{id}])"
        )),
        // Unreachable before anything happened: another host may do.
        EXIT_SSH if matches!(step, Step::Claim) => ProviderError::unavailable(why),
        EXIT_SSH => ProviderError::transient(why),
        EXIT_CLAIMED => ProviderError::capacity(why),
        EXIT_NO_SUDO | EXIT_NO_SYSTEMD | EXIT_FOREIGN_K3S => ProviderError::invalid(why),
        _ => ProviderError::failed(format!("{why} (exit {code})")),
    })
}

/// `~/…` against `$HOME`.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests;
