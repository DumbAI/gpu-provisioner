//! The Kubernetes layer: turn a provisioned VM into a single-node k3s cluster
//! and hand the caller a working *local* kubeconfig.
//!
//! This module owns all k8s-specific knowledge so the provisioning core below
//! it stays orchestrator-agnostic:
//!
//! - [`K3S_INSTALL_SCRIPT`] is the boot-time recipe. The planner embeds it into
//!   cloud-init (via [`crate::userdata::Workload::K3s`]); it installs k3s as a
//!   systemd service (so k3s's own control plane is the continuously-reconciling
//!   on-VM agent) and, when the instance has an NVIDIA GPU, wires up the NVIDIA
//!   container runtime + device plugin so pods can request `nvidia.com/gpu`.
//! - [`setup`] runs client-side after the VM is SSH-ready: it waits for the k3s
//!   API to answer, reads `/etc/rancher/k3s/k3s.yaml`, rewrites its server URL
//!   from `127.0.0.1` to the VM's public IP (k3s is installed with
//!   `--tls-san <public-ip>` so the API cert already covers it), and writes the
//!   result to `<GPUPROV_HOME>/kube/<name>.yaml`.
//!
//! There is intentionally no custom reconciler here — k3s is the reconciler.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::ssh::options::SshOptions;
use crate::ssh::runner;
use crate::util::{self, Paths};

/// Bash run once at boot (via cloud-init `write_files` + `runcmd`) to install a
/// single-node k3s cluster. Idempotent; k3s's own installer is safe to re-run.
///
/// GPU is detected at boot (not at plan time) via `nvidia-smi`, so the same
/// script serves CPU and GPU instances. On the AWS Deep Learning AMI the NVIDIA
/// driver is already present, so only `nvidia-container-toolkit` is installed.
///
/// The NVIDIA device-plugin manifest is pinned to a specific release
/// (`v0.19.3`); bump the URL below to upgrade it.
pub const K3S_INSTALL_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euxo pipefail

log() { echo "[gpuprov-k3s] $*"; }

# --- Public IP for the API-server TLS SAN (IMDSv2, with retries) -----------
PUBIP=""
for _ in 1 2 3 4 5; do
  TOKEN=$(curl -sS -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 300" 2>/dev/null || true)
  if [ -n "$TOKEN" ]; then
    PUBIP=$(curl -sS -H "X-aws-ec2-metadata-token: $TOKEN" \
      "http://169.254.169.254/latest/meta-data/public-ipv4" 2>/dev/null || true)
  fi
  [ -n "$PUBIP" ] && break
  sleep 3
done
log "public IP: ${PUBIP:-<none>}"

INSTALL_ARGS="--write-kubeconfig-mode 644"
[ -n "$PUBIP" ] && INSTALL_ARGS="$INSTALL_ARGS --tls-san $PUBIP"

# --- GPU: install nvidia-container-toolkit BEFORE k3s so k3s auto-detects it -
GPU=0
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  GPU=1
  if command -v nvidia-ctk >/dev/null 2>&1; then
    log "nvidia-container-toolkit already present (Deep Learning AMI)"
  else
    log "NVIDIA GPU detected; installing nvidia-container-toolkit"
    export DEBIAN_FRONTEND=noninteractive
    # Save the key armored (.asc) and let apt verify against it via signed-by.
    # (Avoids `gpg --dearmor`, which fails on /dev/tty in cloud-init's no-TTY
    #  context and would abort the whole script under `set -e`.)
    curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
      -o /usr/share/keyrings/nvidia-container-toolkit-keyring.asc
    curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
      | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.asc] https://#g' \
      > /etc/apt/sources.list.d/nvidia-container-toolkit.list
    apt-get update
    apt-get install -y nvidia-container-toolkit
  fi
  # Make nvidia the default containerd runtime so the device plugin and GPU
  # pods work without an explicit runtimeClassName.
  INSTALL_ARGS="$INSTALL_ARGS --default-runtime nvidia"
fi

# --- Install k3s (systemd-managed; restarts on reboot) ---------------------
log "installing k3s: server $INSTALL_ARGS"
curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="server $INSTALL_ARGS" sh -

# --- Wait for the API server, then (GPU) apply the device plugin -----------
for _ in $(seq 1 120); do
  k3s kubectl get nodes >/dev/null 2>&1 && break
  sleep 5
done

if [ "$GPU" = "1" ]; then
  log "applying NVIDIA device plugin"
  k3s kubectl apply -f https://raw.githubusercontent.com/NVIDIA/k8s-device-plugin/v0.19.3/deployments/static/nvidia-device-plugin.yml
fi

log "k3s install complete"
"#;

/// Where things ended up after [`setup`].
#[derive(Debug, Clone)]
pub struct KubeSetup {
    /// Local kubeconfig file (`export KUBECONFIG=<this>`).
    pub kubeconfig_path: PathBuf,
    /// The API endpoint the kubeconfig points at (`https://<ip>:6443`).
    pub api_endpoint: String,
    /// The renamed cluster/user/context name (`gpuprov-<name>`).
    pub context: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error(
        "VM '{0}' does not have k3s installed (was it provisioned without --k8s?); \
         run `gpuprov down {0}` and re-provision with --k8s"
    )]
    NotK3sNode(String),
    #[error(
        "k3s did not become ready on {ip} within {secs}s (last probe: {last}); inspect the \
         boot log: `gpuprov ssh {name} -- sudo cat /var/log/cloud-init-output.log`"
    )]
    NotReady {
        name: String,
        ip: String,
        secs: u64,
        last: String,
    },
    #[error("SSH to the VM failed: {0}")]
    Ssh(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("could not parse the k3s kubeconfig: {0}")]
    Parse(serde_yaml::Error),
    #[error("unexpected kubeconfig structure: {0}")]
    Invalid(String),
}

/// Default readiness budget — generous, since first boot installs k3s (and, on
/// GPU nodes, the toolkit + device plugin) before the API answers.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(600);

/// Bring the local kubeconfig into being: (optionally) confirm k3s is installed,
/// wait for the API, fetch `/etc/rancher/k3s/k3s.yaml`, rewrite it for the
/// public IP, and write `<base>/kube/<name>.yaml` (0600).
///
/// `reused` means the instance already existed, so cloud-init will *not* run
/// again — if k3s is absent we fail fast rather than wait for an install that
/// will never happen.
pub async fn setup(
    name: &str,
    public_ip: &str,
    ssh_user: &str,
    key_path: &Path,
    paths: &Paths,
    timeout: Duration,
    reused: bool,
) -> Result<KubeSetup, KubeError> {
    let opts = SshOptions::new(key_path.to_path_buf())
        .batch_mode(true)
        .control_dir(Some(paths.sockets_dir()));

    if reused && !k3s_installed(ssh_user, public_ip, &opts).await? {
        return Err(KubeError::NotK3sNode(name.to_string()));
    }

    wait_for_k3s(name, public_ip, ssh_user, &opts, timeout).await?;

    let raw = fetch_kubeconfig(ssh_user, public_ip, &opts).await?;
    let rewritten = rewrite_kubeconfig(&raw, public_ip, name)?;

    let path = paths.kube_dir().join(format!("{name}.yaml"));
    std::fs::write(&path, rewritten.as_bytes())?;
    util::set_permissions(&path, 0o600)?;

    Ok(KubeSetup {
        kubeconfig_path: path,
        api_endpoint: format!("https://{public_ip}:6443"),
        context: format!("gpuprov-{name}"),
    })
}

/// Delete `<base>/kube/<name>.yaml` if present (idempotent). Called by `down`.
pub fn remove_kubeconfig(paths: &Paths, name: &str) -> std::io::Result<()> {
    let path = paths.kube_dir().join(format!("{name}.yaml"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// A single remote shell command (ssh runs it via the login shell).
fn sh(command: &str) -> Vec<String> {
    vec![command.to_string()]
}

async fn k3s_installed(user: &str, ip: &str, opts: &SshOptions) -> Result<bool, KubeError> {
    // Absolute path: a non-interactive SSH shell does not have /usr/local/bin
    // (where the k3s installer puts the binary) on PATH.
    let out = runner::run_command_capture(user, ip, opts, &sh("test -x /usr/local/bin/k3s"))
        .await
        .map_err(|e| KubeError::Ssh(e.to_string()))?;
    Ok(out.status.success())
}

async fn wait_for_k3s(
    name: &str,
    ip: &str,
    user: &str,
    opts: &SshOptions,
    timeout: Duration,
) -> Result<(), KubeError> {
    let start = Instant::now();
    // Absolute path to k3s: a non-interactive SSH shell (and sudo's secure_path
    // on some AMIs) does not include /usr/local/bin, so a bare `k3s` is not found.
    let probe = sh("sudo /usr/local/bin/k3s kubectl get nodes");
    let mut attempts: u32 = 0;
    loop {
        // Capture why the probe isn't ready so a timeout is diagnosable, not silent.
        let last = match runner::run_command_capture(user, ip, opts, &probe).await {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let msg = stderr.trim();
                if msg.is_empty() {
                    format!("exit {}", out.status.code().unwrap_or(-1))
                } else {
                    msg.to_string()
                }
            }
            // A transient ssh hiccup mid-boot is expected; keep polling.
            Err(e) => {
                tracing::debug!("k3s readiness probe ssh error on {ip}: {e}");
                format!("ssh error: {e}")
            }
        };
        if start.elapsed() >= timeout {
            return Err(KubeError::NotReady {
                name: name.to_string(),
                ip: ip.to_string(),
                secs: timeout.as_secs(),
                last,
            });
        }
        attempts += 1;
        if attempts.is_multiple_of(6) {
            tracing::info!(
                "still waiting for k3s API on {ip} ({}s elapsed): {last}",
                start.elapsed().as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn fetch_kubeconfig(user: &str, ip: &str, opts: &SshOptions) -> Result<String, KubeError> {
    let out =
        runner::run_command_capture(user, ip, opts, &sh("sudo cat /etc/rancher/k3s/k3s.yaml"))
            .await
            .map_err(|e| KubeError::Ssh(e.to_string()))?;
    if !out.status.success() {
        return Err(KubeError::Ssh(format!(
            "reading /etc/rancher/k3s/k3s.yaml failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Rewrite a k3s kubeconfig for remote use: repoint the server at `public_ip`
/// and rename the cluster/user/context to `gpuprov-<name>` (so multiple
/// clusters can coexist in a merged kubeconfig). Pure — unit-tested.
fn rewrite_kubeconfig(raw: &str, public_ip: &str, name: &str) -> Result<String, KubeError> {
    let ctx = format!("gpuprov-{name}");
    let ctx_val = serde_yaml::Value::from(ctx.as_str());
    let mut doc: serde_yaml::Value = serde_yaml::from_str(raw).map_err(KubeError::Parse)?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| KubeError::Invalid("kubeconfig root is not a mapping".into()))?;

    // clusters[].name -> ctx ; clusters[].cluster.server host -> public_ip
    let clusters = map
        .get_mut("clusters")
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| KubeError::Invalid("kubeconfig has no clusters".into()))?;
    for cluster in clusters.iter_mut() {
        let Some(cm) = cluster.as_mapping_mut() else {
            continue;
        };
        cm.insert("name".into(), ctx_val.clone());
        if let Some(inner) = cm
            .get_mut("cluster")
            .and_then(serde_yaml::Value::as_mapping_mut)
        {
            let server = inner
                .get("server")
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_string);
            if let Some(server) = server {
                inner.insert("server".into(), with_host(&server, public_ip).into());
            }
        }
    }

    // users[].name -> ctx
    if let Some(users) = map
        .get_mut("users")
        .and_then(serde_yaml::Value::as_sequence_mut)
    {
        for user in users.iter_mut() {
            if let Some(um) = user.as_mapping_mut() {
                um.insert("name".into(), ctx_val.clone());
            }
        }
    }

    // contexts[].name -> ctx and repoint its cluster/user
    if let Some(contexts) = map
        .get_mut("contexts")
        .and_then(serde_yaml::Value::as_sequence_mut)
    {
        for c in contexts.iter_mut() {
            let Some(cmap) = c.as_mapping_mut() else {
                continue;
            };
            cmap.insert("name".into(), ctx_val.clone());
            if let Some(inner) = cmap
                .get_mut("context")
                .and_then(serde_yaml::Value::as_mapping_mut)
            {
                inner.insert("cluster".into(), ctx_val.clone());
                inner.insert("user".into(), ctx_val.clone());
            }
        }
    }

    map.insert("current-context".into(), ctx_val);

    serde_yaml::to_string(&doc).map_err(KubeError::Parse)
}

/// Replace the host in a `scheme://host:port` URL, preserving scheme and port.
fn with_host(server: &str, host: &str) -> String {
    let (scheme, rest) = server.split_once("://").unwrap_or(("https", server));
    let port = rest.rsplit_once(':').map(|(_, p)| p).unwrap_or("6443");
    format!("{scheme}://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: QUFB
    server: https://127.0.0.1:6443
  name: default
contexts:
- context:
    cluster: default
    user: default
  name: default
current-context: default
kind: Config
preferences: {}
users:
- name: default
  user:
    client-certificate-data: QkJC
    client-key-data: Q0ND
";

    #[test]
    fn with_host_preserves_scheme_and_port() {
        assert_eq!(
            with_host("https://127.0.0.1:6443", "1.2.3.4"),
            "https://1.2.3.4:6443"
        );
        assert_eq!(
            with_host("https://localhost:6443", "9.9.9.9"),
            "https://9.9.9.9:6443"
        );
    }

    #[test]
    fn rewrite_points_server_at_public_ip_and_renames() {
        let out = rewrite_kubeconfig(SAMPLE, "203.0.113.7", "dev").unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();

        let cluster = &doc["clusters"][0];
        assert_eq!(cluster["name"], serde_yaml::Value::from("gpuprov-dev"));
        assert_eq!(
            cluster["cluster"]["server"],
            serde_yaml::Value::from("https://203.0.113.7:6443")
        );
        assert_eq!(
            doc["users"][0]["name"],
            serde_yaml::Value::from("gpuprov-dev")
        );
        assert_eq!(
            doc["contexts"][0]["name"],
            serde_yaml::Value::from("gpuprov-dev")
        );
        assert_eq!(
            doc["contexts"][0]["context"]["cluster"],
            serde_yaml::Value::from("gpuprov-dev")
        );
        assert_eq!(
            doc["contexts"][0]["context"]["user"],
            serde_yaml::Value::from("gpuprov-dev")
        );
        assert_eq!(
            doc["current-context"],
            serde_yaml::Value::from("gpuprov-dev")
        );
        // Credentials survive the round-trip untouched.
        assert_eq!(
            cluster["cluster"]["certificate-authority-data"],
            serde_yaml::Value::from("QUFB")
        );
    }

    #[test]
    fn rewrite_rejects_non_mapping() {
        let err = rewrite_kubeconfig("- a\n- b\n", "1.2.3.4", "dev").unwrap_err();
        assert!(matches!(err, KubeError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn rewrite_rejects_unparseable_yaml() {
        let err = rewrite_kubeconfig("clusters: [1, 2", "1.2.3.4", "dev").unwrap_err();
        assert!(matches!(err, KubeError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn install_script_is_well_formed() {
        // The GPU URL must be assembled with the pinned version and no stray gaps.
        assert!(K3S_INSTALL_SCRIPT.starts_with("#!/usr/bin/env bash"));
        assert!(K3S_INSTALL_SCRIPT.contains("INSTALL_K3S_EXEC=\"server $INSTALL_ARGS\""));
        assert!(K3S_INSTALL_SCRIPT.contains("--tls-san $PUBIP"));
        assert!(K3S_INSTALL_SCRIPT
            .contains("k8s-device-plugin/v0.19.3/deployments/static/nvidia-device-plugin.yml"));
        // The armored keyring is downloaded directly; piping the key through
        // gpg fails on /dev/tty under cloud-init, so it must not be used.
        assert!(!K3S_INSTALL_SCRIPT.contains("| gpg"));
        assert!(K3S_INSTALL_SCRIPT.contains("nvidia-container-toolkit-keyring.asc"));
        assert!(K3S_INSTALL_SCRIPT
            .trim_end()
            .ends_with(r#"log "k3s install complete""#));
    }
}
