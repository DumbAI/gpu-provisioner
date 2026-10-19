//! Docker "test cloud": each VM is a long-running sshd container on the local
//! Docker daemon. It exists to exercise the full launch/teardown state
//! machines end-to-end — real SSH probing, journaling, reuse, adoption,
//! teardown — without a cloud account.
//!
//! Primitive mapping (`CloudProvider` contract):
//!
//! | primitive | Docker |
//! |-----------|--------|
//! | plan      | one offer in the single region `local` |
//! | prepare   | check the daemon; build the embedded sshd image if missing |
//! | create    | `docker run -d` labeled `gpuprov-vm-name=<vm>`, SSH published on `127.0.0.1:<random>` |
//! | observe   | `docker ps -a` by label + `docker inspect` → status, mapped port |
//! | delete    | `docker rm -f` (unknown ids are success) |
//! | cleanup   | nothing per-VM |
//!
//! The container entrypoint honors the same login contract cloud-init gives a
//! real VM: `$SSH_USER` with passwordless sudo and `$SSH_PUBLIC_KEY`
//! authorized (see `sshd.Dockerfile`).

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::config::DockerConfig;
use crate::provider::{
    Capabilities, CloudProvider, Feature, InstanceSpec, InstanceStatus, Location, Observed, Offer,
    ProviderError, ResourceAsk, ResourceRequest, SshEndpoint,
};

/// Canonical provider id recorded on VM rows.
pub const PROVIDER_ID: &str = "docker";

/// The only region this "cloud" has.
pub const REGION: &str = "local";

/// Identity label on every container (the AWS `gpuprov-vm-name` tag analog).
pub const LABEL_VM_NAME: &str = "gpuprov-vm-name";
pub const LABEL_MANAGED: &str = "gpuprov-managed";

/// The sshd image recipe, built from stdin on first use.
const DOCKERFILE: &str = include_str!("sshd.Dockerfile");

/// Short CLI calls (run/ps/inspect/rm).
const CLI_TIMEOUT: Duration = Duration::from_secs(60);
/// `docker build` of the sshd image (apt-get on a cold cache).
const BUILD_TIMEOUT: Duration = Duration::from_secs(600);

const UNSUPPORTED: &[(Feature, &str)] = &[
    (Feature::Spot, "local containers have no spot market"),
    (
        Feature::OpenPorts,
        "only SSH is published from the container",
    ),
    (
        Feature::ImageOverride,
        "the container image must implement the sshd login contract",
    ),
    (
        Feature::Kubernetes,
        "containers run sshd, not systemd, so k3s cannot be installed",
    ),
];

pub struct DockerProvider {
    /// User-supplied image; `None` → the embedded sshd image (built on demand).
    image: Option<String>,
}

impl DockerProvider {
    pub fn new(config: &DockerConfig) -> Self {
        DockerProvider {
            image: config.image.clone(),
        }
    }

    /// Tag of the embedded image: crate version + a hash of the Dockerfile,
    /// so editing the recipe never reuses a stale local image.
    pub fn default_image() -> String {
        format!(
            "gpuprov-sshd:{}-{:08x}",
            env!("CARGO_PKG_VERSION"),
            fnv1a(DOCKERFILE.as_bytes()) as u32
        )
    }

    fn image(&self) -> String {
        self.image.clone().unwrap_or_else(Self::default_image)
    }

    /// Build the embedded image unless it already exists locally.
    async fn ensure_image(&self, image: &str) -> Result<(), ProviderError> {
        if docker(&["image", "inspect", image], None, CLI_TIMEOUT)
            .await?
            .status
            .success()
        {
            return Ok(());
        }
        if self.image.is_some() {
            return Err(ProviderError::invalid(format!(
                "docker image '{image}' not found locally; pull or build it first"
            )));
        }
        tracing::info!("building docker test-cloud image {image} (first use)");
        let out = docker(
            &["build", "-t", image, "-"],
            Some(DOCKERFILE),
            BUILD_TIMEOUT,
        )
        .await?;
        if !out.status.success() {
            return Err(ProviderError::unavailable(format!(
                "docker build of {image} failed: {}",
                tail(&out.stderr)
            )));
        }
        Ok(())
    }
}

fn container_name(vm_name: &str) -> String {
    format!("gpuprov-{vm_name}")
}

#[async_trait]
impl CloudProvider for DockerProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            unsupported: UNSUPPORTED,
            boot_script: false,
            boot_timeout: Duration::from_secs(120),
            delete_timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(1),
        }
    }

    async fn plan(&self, request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        plan_offers(request)
    }

    async fn prepare(
        &self,
        _offer: &Offer,
        _spec: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        let image = self.image();
        self.ensure_image(&image).await?;
        Ok(serde_json::json!({ "image": image }))
    }

    async fn create(
        &self,
        _offer: &Offer,
        prepared: &serde_json::Value,
        spec: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        let image = prepared["image"]
            .as_str()
            .ok_or_else(|| ProviderError::failed("corrupt prepare state: no image"))?;
        let name = container_name(&spec.vm_name);
        let vm_label = format!("{LABEL_VM_NAME}={}", spec.vm_name);
        let managed_label = format!("{LABEL_MANAGED}=true");
        let user_env = format!("SSH_USER={}", spec.ssh_user);
        let key_env = format!("SSH_PUBLIC_KEY={}", spec.ssh_public_key.trim());
        let out = docker(
            &[
                "run",
                "-d",
                "--name",
                &name,
                "--hostname",
                &name,
                "--label",
                &vm_label,
                "--label",
                &managed_label,
                "-e",
                &user_env,
                "-e",
                &key_env,
                "-p",
                "127.0.0.1::22",
                image,
            ],
            None,
            CLI_TIMEOUT,
        )
        .await?;
        if !out.status.success() {
            let err = tail(&out.stderr);
            return Err(if err.contains("is already in use") {
                ProviderError::failed(format!("container name {name} already in use: {err}"))
            } else {
                ProviderError::failed(format!("docker run failed: {err}"))
            });
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(ProviderError::failed("docker run returned no container id"));
        }
        Ok(id)
    }

    async fn observe(&self, _at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError> {
        let filter = format!("label={LABEL_VM_NAME}={vm_name}");
        let out = docker(
            &["ps", "-a", "-q", "--no-trunc", "--filter", &filter],
            None,
            CLI_TIMEOUT,
        )
        .await?;
        if !out.status.success() {
            return Err(ProviderError::transient(format!(
                "docker ps failed: {}",
                tail(&out.stderr)
            )));
        }
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["inspect"];
        args.extend(ids.iter().map(String::as_str));
        let out = docker(&args, None, CLI_TIMEOUT).await?;
        // A container removed between `ps` and `inspect` makes inspect exit
        // non-zero while still printing the ones it found.
        let stderr = tail(&out.stderr);
        if !out.status.success() && !stderr.contains("No such") {
            return Err(ProviderError::transient(format!(
                "docker inspect failed: {stderr}"
            )));
        }
        parse_inspect(&out.stdout)
    }

    async fn delete(
        &self,
        _at: &Location,
        _vm_name: &str,
        instance_ids: &[String],
    ) -> Result<(), ProviderError> {
        if instance_ids.is_empty() {
            return Ok(());
        }
        let mut args = vec!["rm", "-f"];
        args.extend(instance_ids.iter().map(String::as_str));
        let out = docker(&args, None, CLI_TIMEOUT).await?;
        let stderr = tail(&out.stderr);
        if out.status.success() || is_only_missing(&stderr) {
            Ok(())
        } else {
            Err(ProviderError::transient(format!(
                "docker rm failed: {stderr}"
            )))
        }
    }

    async fn cleanup(&self, _at: &Location, _vm_name: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// One offer in `local`; GPU asks are unplannable here.
fn plan_offers(request: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
    let instance_type = match &request.ask {
        ResourceAsk::InstanceType(it) => it.clone(),
        ResourceAsk::Gpus(gpus) => {
            return Err(ProviderError::invalid(format!(
                "the docker test cloud has no GPUs (asked for {gpus}); use --instance-type \
                 container"
            )))
        }
    };
    if let Some(bad) = request.regions.iter().find(|r| *r != REGION) {
        return Err(ProviderError::invalid(format!(
            "the docker test cloud only has region '{REGION}' (got '{bad}')"
        )));
    }
    if let Some(zone) = &request.zone {
        return Err(ProviderError::invalid(format!(
            "the docker test cloud has no zones (got '{zone}')"
        )));
    }
    Ok(vec![Offer::new(instance_type, REGION)])
}

/// Run the docker CLI, mapping "can't run docker at all" to clear errors.
async fn docker(
    args: &[&str],
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<std::process::Output, ProviderError> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ProviderError::invalid("the docker CLI was not found on PATH (install Docker)")
        } else {
            ProviderError::transient(format!("cannot run docker: {e}"))
        }
    })?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())
            .await
            .map_err(|e| ProviderError::transient(format!("writing to docker stdin: {e}")))?;
        drop(pipe);
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(ProviderError::transient(format!("docker {}: {e}", args[0]))),
        Err(_) => {
            return Err(ProviderError::transient(format!(
                "docker {} timed out after {}s",
                args[0],
                timeout.as_secs()
            )))
        }
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Cannot connect to the Docker daemon") {
        return Err(ProviderError::invalid(
            "the Docker daemon is not running (start Docker Desktop / dockerd)",
        ));
    }
    Ok(out)
}

/// `docker rm -f` stderr consisting only of "No such container" lines.
fn is_only_missing(stderr: &str) -> bool {
    !stderr.is_empty()
        && stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .all(|l| l.contains("No such container"))
}

/// Last few lines of CLI stderr, for error messages.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" | ")
}

/// 64-bit FNV-1a: a stable (toolchain-independent) content hash for the tag.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

// ---------------------------------------------------------------------------
// `docker inspect` parsing (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspected {
    id: String,
    state: InspectState,
    #[serde(default)]
    network_settings: Option<NetworkSettings>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectState {
    status: String,
    #[serde(default)]
    exit_code: i64,
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NetworkSettings {
    #[serde(default)]
    ports: Option<std::collections::HashMap<String, Option<Vec<PortBinding>>>>,
    #[serde(default, rename = "IPAddress")]
    ip_address: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PortBinding {
    #[serde(default)]
    host_ip: String,
    host_port: String,
}

/// Parse `docker inspect` output into normalized observations.
fn parse_inspect(stdout: &[u8]) -> Result<Vec<Observed>, ProviderError> {
    let text = String::from_utf8_lossy(stdout);
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let inspected: Vec<Inspected> = serde_json::from_str(&text)
        .map_err(|e| ProviderError::transient(format!("unparseable docker inspect output: {e}")))?;
    let mut observed: Vec<Observed> = inspected.into_iter().map(to_observed).collect();
    observed.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    Ok(observed)
}

fn to_observed(c: Inspected) -> Observed {
    let status = map_status(&c.state);
    let endpoint = (status == InstanceStatus::Running)
        .then(|| ssh_host_port(c.network_settings.as_ref()))
        .flatten()
        .map(|(host, port)| SshEndpoint {
            host,
            port,
            user: None,
        });
    let private_ip = c
        .network_settings
        .as_ref()
        .map(|n| n.ip_address.clone())
        .filter(|ip| !ip.is_empty());
    Observed {
        instance_id: c.id,
        status,
        zone: None,
        private_ip,
        endpoint,
    }
}

fn map_status(state: &InspectState) -> InstanceStatus {
    match state.status.as_str() {
        "created" | "restarting" => InstanceStatus::Pending,
        "running" => InstanceStatus::Running,
        "paused" => InstanceStatus::Stopped,
        "removing" => InstanceStatus::Terminating,
        "exited" | "dead" => {
            let mut reason = format!("container {} with code {}", state.status, state.exit_code);
            if !state.error.is_empty() {
                reason.push_str(&format!(": {}", state.error));
            }
            InstanceStatus::Failed(reason)
        }
        other => InstanceStatus::Failed(format!("unknown container status '{other}'")),
    }
}

/// The host side of the published `22/tcp` binding.
fn ssh_host_port(net: Option<&NetworkSettings>) -> Option<(String, u16)> {
    let bindings = net?.ports.as_ref()?.get("22/tcp")?.as_ref()?;
    let binding = bindings.first()?;
    let port = binding.host_port.parse().ok()?;
    let host = match binding.host_ip.as_str() {
        "" | "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        ip => ip.to_string(),
    };
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"[{
        "Id": "bbb",
        "State": {"Status": "running", "ExitCode": 0, "Error": ""},
        "NetworkSettings": {
            "IPAddress": "172.17.0.2",
            "Ports": {"22/tcp": [{"HostIp": "127.0.0.1", "HostPort": "49153"}]}
        }
    }, {
        "Id": "aaa",
        "State": {"Status": "exited", "ExitCode": 137, "Error": ""},
        "NetworkSettings": {"IPAddress": "", "Ports": {}}
    }]"#;

    #[test]
    fn inspect_maps_status_and_published_port() {
        let observed = parse_inspect(RUNNING.as_bytes()).unwrap();
        assert_eq!(observed.len(), 2);
        // Sorted by id.
        let (exited, running) = (&observed[0], &observed[1]);
        assert_eq!(running.instance_id, "bbb");
        assert!(running.is_ready());
        assert_eq!(
            running.endpoint,
            Some(SshEndpoint {
                host: "127.0.0.1".into(),
                port: 49153,
                user: None,
            })
        );
        assert_eq!(running.private_ip.as_deref(), Some("172.17.0.2"));
        assert!(matches!(&exited.status, InstanceStatus::Failed(r) if r.contains("137")));
        assert!(exited.endpoint.is_none());
        assert!(exited.private_ip.is_none());
    }

    #[test]
    fn running_without_binding_is_not_ready_yet() {
        let json = r#"[{"Id": "c", "State": {"Status": "running"},
                        "NetworkSettings": {"Ports": {"22/tcp": null}}}]"#;
        let observed = parse_inspect(json.as_bytes()).unwrap();
        assert_eq!(observed[0].status, InstanceStatus::Running);
        assert!(!observed[0].is_ready());
    }

    #[test]
    fn wildcard_host_ip_maps_to_loopback() {
        let json = r#"[{"Id": "c", "State": {"Status": "running"},
            "NetworkSettings": {"Ports": {"22/tcp": [{"HostIp": "0.0.0.0", "HostPort": "2222"}]}}}]"#;
        let observed = parse_inspect(json.as_bytes()).unwrap();
        let endpoint = observed[0].endpoint.as_ref().unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.port), ("127.0.0.1", 2222));
    }

    #[test]
    fn status_mapping() {
        let state = |s: &str| InspectState {
            status: s.into(),
            exit_code: 1,
            error: "boom".into(),
        };
        assert_eq!(map_status(&state("created")), InstanceStatus::Pending);
        assert_eq!(map_status(&state("restarting")), InstanceStatus::Pending);
        assert_eq!(map_status(&state("paused")), InstanceStatus::Stopped);
        assert_eq!(map_status(&state("removing")), InstanceStatus::Terminating);
        assert!(
            matches!(map_status(&state("dead")), InstanceStatus::Failed(r) if r.contains("boom"))
        );
    }

    #[test]
    fn empty_inspect_output_is_no_instances() {
        assert!(parse_inspect(b"").unwrap().is_empty());
        assert!(parse_inspect(b"[]").unwrap().is_empty());
        assert!(parse_inspect(b"not json").is_err());
    }

    fn request(ask: ResourceAsk, regions: &[&str]) -> ResourceRequest {
        ResourceRequest {
            ask,
            regions: regions.iter().map(|r| r.to_string()).collect(),
            zone: None,
            spot: false,
        }
    }

    #[test]
    fn plan_has_one_local_offer_and_rejects_gpus_and_other_regions() {
        let offers =
            plan_offers(&request(ResourceAsk::InstanceType("container".into()), &[])).unwrap();
        assert_eq!(offers, vec![Offer::new("container", "local")]);
        assert!(plan_offers(&request(
            ResourceAsk::InstanceType("container".into()),
            &["local"]
        ))
        .is_ok());
        let err = plan_offers(&request(
            ResourceAsk::InstanceType("container".into()),
            &["us-west-2"],
        ))
        .unwrap_err();
        assert!(err.kind.is_fatal());
        let gpu = ResourceAsk::Gpus("H100:1".parse().unwrap());
        assert!(plan_offers(&request(gpu, &[])).unwrap_err().kind.is_fatal());
    }

    #[test]
    fn default_image_tag_is_stable_and_versioned() {
        let tag = DockerProvider::default_image();
        assert_eq!(tag, DockerProvider::default_image());
        assert!(tag.starts_with(&format!("gpuprov-sshd:{}-", env!("CARGO_PKG_VERSION"))));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn rm_missing_only_detection() {
        assert!(is_only_missing(
            "Error response from daemon: No such container: abc\n"
        ));
        assert!(!is_only_missing("Error: permission denied"));
        assert!(!is_only_missing(""));
    }

    #[test]
    fn capabilities_reject_ports_and_spot() {
        let caps = DockerProvider::new(&DockerConfig::default()).capabilities();
        assert!(!caps.supports(Feature::OpenPorts));
        assert!(!caps.supports(Feature::Spot));
    }
}
