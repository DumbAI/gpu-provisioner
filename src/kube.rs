//! The Kubernetes layer's mechanics: the k3s installer and single-shot SSH
//! primitives (boot-install probe, install, API readiness, kubeconfig
//! fetch/rewrite, cluster health) that the durable cluster machine
//! (`cluster::k3s`) sequences, retries and times out. No loops or sleeps here.
//!
//! The installer has two delivery routes and is safe to run through both:
//!
//! - **boot** — embedded in cloud-init user data on clouds that boot with
//!   cloud-init (AWS, GCP), so k3s installs while the VM boots, overlapping
//!   the SSH wait instead of following it;
//! - **SSH** — pushed by the cluster machine when the boot route was not
//!   available (no cloud-init, reused VM), failed, or timed out.
//!
//! A host-wide `flock` serializes the two, a status marker
//! (`/var/lib/gpuprov/k3s-install.status`) reports how the last run ended,
//! and the script owns `/etc/rancher/k3s/config.yaml` (re-rendered from
//! `/etc/gpuprov/k3s-sans`, restarting k3s only when it changes), so re-runs
//! are idempotent and can add a TLS SAN after the fact.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::ssh::options::SshOptions;
use crate::ssh::runner;
use crate::util::{self, Paths};

/// Ports a k3s node serves to clients: the Kubernetes API server plus the
/// k3s-bundled Traefik ingress / ServiceLB HTTP(S) endpoints.
pub const K3S_PORTS: [u16; 3] = [6443, 80, 443];
/// API server port on the node.
pub const API_PORT: u16 = 6443;
/// Local end of `gpuprov tunnel` for clouds without inbound ports.
pub const TUNNEL_LOCAL_PORT: u16 = 16443;

/// Run as root. Environment:
/// - `GPUPROV_INSTALL_ORIGIN` — `boot` or `ssh` (recorded in the marker);
/// - `GPUPROV_TLS_SAN` — host clients use for the API; when empty (boot
///   route) the public IP is looked up from the AWS / GCP metadata services.
///
/// GPU is detected via `nvidia-smi`, so one script serves CPU and GPU nodes.
/// Images that ship the NVIDIA driver and toolkit (AWS Deep Learning AMI,
/// GCP Deep Learning VM, CUDA marketplace images) need nothing extra. The
/// device-plugin manifest is pinned (`v0.19.3`).
pub const K3S_INSTALL_SCRIPT: &str = r#"#!/usr/bin/env bash
set -euo pipefail

ORIGIN="${GPUPROV_INSTALL_ORIGIN:-ssh}"
STATUS_DIR=/var/lib/gpuprov
STATUS="$STATUS_DIR/k3s-install.status"
SANS=/etc/gpuprov/k3s-sans
GPU_FLAG=/etc/gpuprov/k3s-gpu
CONF=/etc/rancher/k3s/config.yaml
mkdir -p "$STATUS_DIR" /etc/gpuprov /etc/rancher/k3s

log() { echo "[gpuprov-k3s:$ORIGIN] $*"; }

# One installer at a time: the boot run and an SSH fallback must never race.
exec 9>/run/gpuprov-k3s.lock
flock 9

GPU=0
echo "state=running origin=$ORIGIN gpu=$GPU" > "$STATUS"
trap 'rc=$?; if [ $rc -ne 0 ]; then echo "state=failed origin=$ORIGIN gpu=$GPU exit=$rc" > "$STATUS"; fi' EXIT
set -x

# --- TLS SAN: the caller's host, else the public IP from cloud metadata ----
SAN="${GPUPROV_TLS_SAN:-}"
if [ -z "$SAN" ]; then
  TOKEN=$(curl -sS -m 2 -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 300" 2>/dev/null || true)
  if [ -n "$TOKEN" ]; then
    SAN=$(curl -sS -m 2 -H "X-aws-ec2-metadata-token: $TOKEN" \
      "http://169.254.169.254/latest/meta-data/public-ipv4" 2>/dev/null || true)
  fi
fi
if [ -z "$SAN" ]; then
  SAN=$(curl -sS -m 2 -H "Metadata-Flavor: Google" \
    "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip" \
    2>/dev/null || true)
fi
case "$SAN" in *[!A-Za-z0-9.:-]*) SAN="";; esac   # ignore error pages
if [ -n "$SAN" ] && ! grep -qxF "$SAN" "$SANS" 2>/dev/null; then echo "$SAN" >> "$SANS"; fi
touch "$SANS"

# --- GPU: install nvidia-container-toolkit BEFORE k3s so k3s detects it -----
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  GPU=1
  if ! command -v nvidia-ctk >/dev/null 2>&1; then
    log "NVIDIA GPU detected; installing nvidia-container-toolkit"
    export DEBIAN_FRONTEND=noninteractive
    # Armored key + signed-by (no `gpg --dearmor`, which needs a TTY).
    curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
      -o /usr/share/keyrings/nvidia-container-toolkit-keyring.asc
    curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
      | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.asc] https://#g' \
      > /etc/apt/sources.list.d/nvidia-container-toolkit.list
    apt-get update
    apt-get install -y nvidia-container-toolkit
  fi
fi
echo "$GPU" > "$GPU_FLAG"

# --- Render the k3s config we own; note whether it changed ------------------
{
  echo 'write-kubeconfig-mode: "0644"'
  # nvidia as the default containerd runtime: GPU pods need no runtimeClassName.
  [ "$GPU" = "1" ] && echo 'default-runtime: nvidia'
  if [ -s "$SANS" ]; then
    echo 'tls-san:'
    while IFS= read -r s; do [ -n "$s" ] && echo "  - \"$s\""; done < "$SANS"
  fi
} > "$CONF.new"
CHANGED=1
if cmp -s "$CONF.new" "$CONF"; then CHANGED=0; rm -f "$CONF.new"; else mv "$CONF.new" "$CONF"; fi

# --- Install k3s, or apply config changes to a running one -----------------
if systemctl is-active --quiet k3s 2>/dev/null; then
  if [ "$CHANGED" = "1" ]; then
    log "k3s running; config changed (TLS SAN / runtime) — restarting"
    systemctl restart k3s
  else
    log "k3s running; nothing to change"
  fi
else
  log "installing k3s"
  curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="server" sh -
fi

# --- GPU: device plugin (needs the API) ------------------------------------
if [ "$GPU" = "1" ]; then
  for _ in $(seq 1 60); do
    /usr/local/bin/k3s kubectl get --raw=/readyz >/dev/null 2>&1 && break
    sleep 5
  done
  log "applying NVIDIA device plugin"
  /usr/local/bin/k3s kubectl apply -f https://raw.githubusercontent.com/NVIDIA/k8s-device-plugin/v0.19.3/deployments/static/nvidia-device-plugin.yml
fi

echo "state=done origin=$ORIGIN gpu=$GPU" > "$STATUS"
log "k3s install complete"
"#;

/// Reports what a boot-route install did, in ONE SSH round trip (parsed by
/// [`BootProbe::parse`]).
const BOOT_PROBE_SCRIPT: &str = r#"echo "marker=$(cat /var/lib/gpuprov/k3s-install.status 2>/dev/null | tr ' ' ',')"
if command -v cloud-init >/dev/null 2>&1; then
  echo "cloud_init=$(cloud-init status 2>/dev/null | sed -n 's/^status: *//p' | head -1)"
else
  echo "cloud_init=absent"
fi
echo "k3s_active=$(systemctl is-active k3s 2>/dev/null)"
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then echo "gpu_present=1"; else echo "gpu_present=0"; fi
"#;

/// Prints `key=value` lines describing cluster health in ONE SSH round trip.
/// Absolute k3s path: non-interactive SSH shells (and sudo's secure_path on
/// some images) lack /usr/local/bin. `GPUPROV_TLS_SAN`, when set, is checked
/// against the API server's serving certificate.
const VALIDATE_SCRIPT: &str = r#"K=/usr/local/bin/k3s
echo "node_ready=$($K kubectl get nodes -o jsonpath='{.items[0].status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)"
echo "coredns_ready=$($K kubectl -n kube-system get deploy coredns -o jsonpath='{.status.readyReplicas}' 2>/dev/null)"
if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then echo "gpu_present=1"; else echo "gpu_present=0"; fi
echo "gpu_allocatable=$($K kubectl get nodes -o jsonpath='{.items[0].status.allocatable.nvidia\.com/gpu}' 2>/dev/null)"
if [ -n "${GPUPROV_TLS_SAN:-}" ]; then
  CERT=/var/lib/rancher/k3s/server/tls/serving-kube-apiserver.crt
  if command -v openssl >/dev/null 2>&1 && [ -f "$CERT" ]; then
    if openssl x509 -in "$CERT" -noout -ext subjectAltName 2>/dev/null | tr ',' '\n' | sed 's/^ *//' \
      | grep -qxF -e "IP Address:$GPUPROV_TLS_SAN" -e "DNS:$GPUPROV_TLS_SAN"; then
      echo "san_ok=1"
    else
      echo "san_ok=0"
    fi
  fi
fi
"#;

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error("SSH to the VM failed: {0}")]
    Ssh(String),
    #[error("k3s install failed (exit {code}): {stderr}")]
    Install { code: i32, stderr: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("could not parse the k3s kubeconfig: {0}")]
    Parse(serde_yaml::Error),
    #[error("unexpected kubeconfig structure: {0}")]
    Invalid(String),
}

/// How clients reach the API server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessMode {
    /// `https://<public-host>:6443` — the cloud opened the port.
    Direct,
    /// `https://127.0.0.1:16443` through `gpuprov tunnel <name>` — for clouds
    /// without inbound-port control (e.g. Prime Intellect, the Docker cloud).
    SshTunnel,
}

/// Where the local kubeconfig lives and what it points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubeAccess {
    pub kubeconfig_path: PathBuf,
    pub api_endpoint: String,
    /// The renamed cluster/user/context name (`gpuprov-<name>`).
    pub context: String,
    pub mode: AccessMode,
}

/// SSH coordinates of the node.
#[derive(Debug, Clone)]
pub struct Node<'a> {
    pub host: &'a str,
    pub user: &'a str,
    pub opts: SshOptions,
}

/// One validation snapshot (see [`VALIDATE_SCRIPT`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    pub node_ready: bool,
    pub coredns_ready: bool,
    pub gpu_present: bool,
    pub gpu_allocatable: u32,
    /// Whether the API serving cert covers the requested SAN; `None` when not
    /// checked (no SAN asked for, or no `openssl` on the node).
    pub san_ok: Option<bool>,
}

impl Health {
    /// What is still missing, or empty when the cluster is usable.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.node_ready {
            missing.push("node not Ready");
        }
        if !self.coredns_ready {
            missing.push("CoreDNS not ready");
        }
        if self.gpu_present && self.gpu_allocatable == 0 {
            missing.push("GPU present but nvidia.com/gpu not allocatable yet");
        }
        if self.san_ok == Some(false) {
            missing.push("API certificate does not cover the public host yet");
        }
        missing
    }

    /// Parse the `key=value` output of [`VALIDATE_SCRIPT`]. Pure.
    pub fn parse(output: &str) -> Health {
        let mut health = Health::default();
        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "node_ready" => health.node_ready = value == "True",
                "coredns_ready" => health.coredns_ready = value.parse::<u32>().unwrap_or(0) > 0,
                "gpu_present" => health.gpu_present = value == "1",
                "gpu_allocatable" => health.gpu_allocatable = value.parse().unwrap_or(0),
                "san_ok" => health.san_ok = Some(value == "1"),
                _ => {}
            }
        }
        health
    }
}

/// What a boot-route install left behind (see [`BOOT_PROBE_SCRIPT`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootProbe {
    /// `state` from the status marker: `running` / `done` / `failed`; `None`
    /// when no installer ever ran on this host.
    pub marker_state: Option<String>,
    /// Whether the marker's run detected a GPU.
    pub marker_gpu: bool,
    /// `cloud-init status` (`running`, `done`, `error`, …) or `absent`.
    pub cloud_init: String,
    pub k3s_active: bool,
    pub gpu_present: bool,
}

/// What the cluster machine should do after probing the boot route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// Installed at boot and healthy: skip the SSH install.
    Installed,
    /// Boot install (or cloud-init before it) still running.
    Wait(String),
    /// Take the SSH route, with the reason (for the journal).
    Fallback(String),
}

impl BootProbe {
    /// Parse [`BOOT_PROBE_SCRIPT`] output. Pure.
    pub fn parse(output: &str) -> BootProbe {
        let mut probe = BootProbe::default();
        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "marker" => {
                    for field in value.split(',') {
                        match field.split_once('=') {
                            Some(("state", state)) if !state.is_empty() => {
                                probe.marker_state = Some(state.to_string())
                            }
                            Some(("gpu", gpu)) => probe.marker_gpu = gpu == "1",
                            _ => {}
                        }
                    }
                }
                "cloud_init" => {
                    probe.cloud_init = if value.is_empty() {
                        "absent".into()
                    } else {
                        value.to_string()
                    }
                }
                "k3s_active" => probe.k3s_active = value == "active",
                "gpu_present" => probe.gpu_present = value == "1",
                _ => {}
            }
        }
        probe
    }

    /// The boot-route decision table. Pure.
    pub fn decide(&self) -> BootDecision {
        match self.marker_state.as_deref() {
            Some("done") if !self.k3s_active => {
                BootDecision::Fallback("installed at boot but k3s is not running".into())
            }
            Some("done") if self.gpu_present && !self.marker_gpu => BootDecision::Fallback(
                "GPU appeared after the boot install; GPU runtime not configured".into(),
            ),
            Some("done") => BootDecision::Installed,
            Some("running") => BootDecision::Wait("boot install in progress".into()),
            Some(other) => BootDecision::Fallback(format!("boot install ended in state '{other}'")),
            None if self.cloud_init == "running" => {
                BootDecision::Wait("cloud-init still running; boot install not started".into())
            }
            None => BootDecision::Fallback(format!(
                "k3s was not installed at boot (cloud-init: {})",
                self.cloud_init
            )),
        }
    }
}

/// A single remote shell command (ssh runs it via the login shell).
fn sh(command: impl Into<String>) -> Vec<String> {
    vec![command.into()]
}

async fn capture(node: &Node<'_>, command: String) -> Result<std::process::Output, KubeError> {
    runner::run_command_capture(node.user, node.host, &node.opts, &sh(command))
        .await
        .map_err(|e| KubeError::Ssh(e.to_string()))
}

/// Inspect what the boot route did (one SSH round trip).
pub async fn probe_boot(node: &Node<'_>) -> Result<BootProbe, KubeError> {
    let command = format!("bash -c {}", shell_quote(BOOT_PROBE_SCRIPT));
    let out = capture(node, command).await?;
    Ok(BootProbe::parse(&String::from_utf8_lossy(&out.stdout)))
}

/// The SSH route: push and run the installer (idempotent; waits for a boot
/// run holding the lock). `tls_san` is the host clients will use for the API
/// and is added to the API certificate.
pub async fn install(node: &Node<'_>, tls_san: &str) -> Result<(), KubeError> {
    let script = BASE64.encode(K3S_INSTALL_SCRIPT);
    let command = format!(
        "echo {script} | base64 -d | sudo GPUPROV_INSTALL_ORIGIN=ssh GPUPROV_TLS_SAN={san} bash -s",
        san = shell_quote(tls_san)
    );
    let out = capture(node, command).await?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Keep the tail: the actionable error is at the end of `set -x` output.
    let tail: Vec<&str> = stderr.lines().rev().take(15).collect();
    Err(KubeError::Install {
        code: out.status.code().unwrap_or(-1),
        stderr: tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
    })
}

/// Whether the API server answers `/readyz`. `Err` only for SSH failures.
pub async fn api_ready(node: &Node<'_>) -> Result<bool, KubeError> {
    let out = capture(
        node,
        "sudo /usr/local/bin/k3s kubectl get --raw=/readyz".to_string(),
    )
    .await?;
    Ok(out.status.success())
}

/// Read `/etc/rancher/k3s/k3s.yaml`, rewrite it for `server_host`, and write
/// `<base>/kube/<name>.yaml` (0600).
pub async fn fetch_kubeconfig(
    node: &Node<'_>,
    paths: &Paths,
    name: &str,
    mode: AccessMode,
) -> Result<KubeAccess, KubeError> {
    let out = capture(node, "sudo cat /etc/rancher/k3s/k3s.yaml".to_string()).await?;
    if !out.status.success() {
        return Err(KubeError::Ssh(format!(
            "reading /etc/rancher/k3s/k3s.yaml failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let (server_host, port) = match mode {
        AccessMode::Direct => (node.host, API_PORT),
        AccessMode::SshTunnel => ("127.0.0.1", TUNNEL_LOCAL_PORT),
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut rewritten = rewrite_kubeconfig(&raw, server_host, name)?;
    if port != API_PORT {
        rewritten = rewritten.replace(
            &format!("{server_host}:{API_PORT}"),
            &format!("{server_host}:{port}"),
        );
    }
    let path = kubeconfig_path(paths, name);
    std::fs::write(&path, rewritten.as_bytes())?;
    util::set_permissions(&path, 0o600)?;
    Ok(KubeAccess {
        kubeconfig_path: path,
        api_endpoint: format!("https://{server_host}:{port}"),
        context: format!("gpuprov-{name}"),
        mode,
    })
}

/// One health snapshot of the cluster, taken on the node. With `tls_san`,
/// also checks that the API certificate covers that host.
pub async fn health(node: &Node<'_>, tls_san: Option<&str>) -> Result<Health, KubeError> {
    let out = capture(node, health_command(tls_san)).await?;
    Ok(Health::parse(&String::from_utf8_lossy(&out.stdout)))
}

/// The remote shell command whose output [`Health::parse`] reads (one SSH
/// round trip). With `tls_san`, also checks the API certificate covers it.
pub fn health_command(tls_san: Option<&str>) -> String {
    format!(
        "sudo GPUPROV_TLS_SAN={} bash -c {}",
        shell_quote(tls_san.unwrap_or("")),
        shell_quote(VALIDATE_SCRIPT)
    )
}

/// Whether `host:port` accepts TCP from here — catches a closed firewall
/// that on-node checks cannot see.
pub async fn reachable(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect((host, port))
        )
        .await,
        Ok(Ok(_))
    )
}

pub fn kubeconfig_path(paths: &Paths, name: &str) -> PathBuf {
    paths.kube_dir().join(format!("{name}.yaml"))
}

/// Delete `<base>/kube/<name>.yaml` if present (idempotent). Called by `down`.
pub fn remove_kubeconfig(paths: &Paths, name: &str) -> std::io::Result<()> {
    remove_if_exists(&kubeconfig_path(paths, name))
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Single-quote for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Rewrite a k3s kubeconfig for remote use: repoint the server at `server_host`
/// and rename the cluster/user/context to `gpuprov-<name>` (so multiple
/// clusters can coexist in a merged kubeconfig). Pure — unit-tested.
pub fn rewrite_kubeconfig(raw: &str, server_host: &str, name: &str) -> Result<String, KubeError> {
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
                inner.insert("server".into(), with_host(&server, server_host).into());
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
    fn health_parses_and_reports_missing_pieces() {
        let ok =
            Health::parse("node_ready=True\ncoredns_ready=1\ngpu_present=0\ngpu_allocatable=\n");
        assert!(ok.missing().is_empty(), "{ok:?}");

        let gpu_pending =
            Health::parse("node_ready=True\ncoredns_ready=1\ngpu_present=1\ngpu_allocatable=\n");
        assert_eq!(
            gpu_pending.missing(),
            vec!["GPU present but nvidia.com/gpu not allocatable yet"]
        );

        let booting = Health::parse("node_ready=\ncoredns_ready=\n");
        assert_eq!(
            booting.missing(),
            vec!["node not Ready", "CoreDNS not ready"]
        );

        let gpu_ok =
            Health::parse("node_ready=True\ncoredns_ready=2\ngpu_present=1\ngpu_allocatable=8\n");
        assert_eq!(gpu_ok.gpu_allocatable, 8);
        assert!(gpu_ok.missing().is_empty());
    }

    #[test]
    fn shell_quote_survives_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[test]
    fn install_script_is_well_formed() {
        let script = K3S_INSTALL_SCRIPT;
        assert!(script.starts_with("#!/usr/bin/env bash"));
        // Boot and SSH runs are serialized and report through the marker.
        assert!(script.contains("flock 9"));
        assert!(script.contains("state=running origin=$ORIGIN"));
        assert!(script.contains("state=failed origin=$ORIGIN"));
        assert!(script.trim_end().ends_with(r#"log "k3s install complete""#));
        // The config we own carries the SANs; a changed config restarts k3s.
        assert!(script.contains("tls-san:"));
        assert!(script.contains("systemctl restart k3s"));
        assert!(script.contains("systemctl is-active --quiet k3s"));
        // Public-IP fallback for the boot route (AWS IMDSv2, GCP metadata).
        assert!(script.contains("169.254.169.254/latest/api/token"));
        assert!(script.contains("Metadata-Flavor: Google"));
        assert!(script
            .contains("k8s-device-plugin/v0.19.3/deployments/static/nvidia-device-plugin.yml"));
        // Piping the NVIDIA key through gpg fails without a TTY.
        assert!(!script.contains("| gpg"));
    }

    /// `bash -n` every embedded script: a syntax error must fail the test
    /// suite, not a VM's boot.
    #[test]
    fn embedded_scripts_parse_as_bash() {
        for (name, script) in [
            ("install", K3S_INSTALL_SCRIPT),
            ("boot-probe", BOOT_PROBE_SCRIPT),
            ("validate", VALIDATE_SCRIPT),
        ] {
            let out = std::process::Command::new("bash")
                .args(["-n", "-c", script])
                .output()
                .expect("bash available");
            assert!(
                out.status.success(),
                "{name} script: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    fn probe(text: &str) -> BootProbe {
        BootProbe::parse(text)
    }

    #[test]
    fn boot_probe_parses_marker_and_status() {
        let p = probe(
            "marker=state=done,origin=boot,gpu=1\ncloud_init=done\nk3s_active=active\ngpu_present=1\n",
        );
        assert_eq!(p.marker_state.as_deref(), Some("done"));
        assert!(p.marker_gpu && p.k3s_active && p.gpu_present);
        assert_eq!(p.cloud_init, "done");
        let empty = probe("marker=\ncloud_init=\nk3s_active=inactive\ngpu_present=0\n");
        assert_eq!(empty.marker_state, None);
        assert_eq!(empty.cloud_init, "absent");
    }

    #[test]
    fn boot_decision_table() {
        let installed = probe("marker=state=done,gpu=0\nk3s_active=active\ngpu_present=0\n");
        assert_eq!(installed.decide(), BootDecision::Installed);

        let running = probe("marker=state=running\ncloud_init=running\n");
        assert!(matches!(running.decide(), BootDecision::Wait(_)));

        // cloud-init has not reached our runcmd yet.
        let pre = probe("marker=\ncloud_init=running\n");
        assert!(matches!(pre.decide(), BootDecision::Wait(_)));

        for fallback in [
            "marker=state=failed,exit=1\ncloud_init=error\n", // boot run failed
            "marker=\ncloud_init=done\n",                     // never delivered
            "marker=\ncloud_init=absent\n",                   // cloud without cloud-init
            "marker=state=done,gpu=0\nk3s_active=failed\n",   // installed but dead
            "marker=state=done,gpu=0\nk3s_active=active\ngpu_present=1\n", // GPU missed at boot
        ] {
            assert!(
                matches!(probe(fallback).decide(), BootDecision::Fallback(_)),
                "{fallback}"
            );
        }
    }

    #[test]
    fn health_reports_missing_san() {
        let h = Health::parse("node_ready=True\ncoredns_ready=1\ngpu_present=0\nsan_ok=0\n");
        assert_eq!(
            h.missing(),
            vec!["API certificate does not cover the public host yet"]
        );
        let unchecked = Health::parse("node_ready=True\ncoredns_ready=1\n");
        assert_eq!(unchecked.san_ok, None);
        assert!(unchecked.missing().is_empty());
    }
}
