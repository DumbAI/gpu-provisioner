//! VM health check: is a VM still there, running and usable — or has the
//! vendor stopped, preempted or reclaimed it?
//!
//! The check is a ladder; each rung builds on the previous one:
//!
//! | rung | source | failure means |
//! |---|---|---|
//! | `record` | gpuprov's local record | torn down by `gpuprov down` |
//! | `vendor` | [`CloudProvider::diagnose`] — the vendor's own view plus any vendor-native findings | **gone** (terminated/reclaimed), **stopped/failed** (preempted, evicted, crashed), replaced, or flagged by the vendor |
//! | `ping` | ICMP echo to the address | informational when the vendor answered (clouds often block ICMP); decisive in the fallback |
//! | `network` | TCP connect to the SSH port | firewall, network or host down |
//! | `ssh` | our key logs in | sshd or account broken |
//! | `gpu` (deep) | `nvidia-smi -L` | driver lost, GPU fell off the bus |
//! | `k3s` (deep) | the single-node cluster's health | k3s down, GPU not allocatable |
//!
//! **Vendor first, network as fallback.** Vendor knowledge comes only from
//! `diagnose`: every cloud gets the default (derived from `observe`), and an
//! adapter adds native signals — status checks, scheduled maintenance,
//! preemption notices — by overriding that one method. When the vendor
//! cannot be asked (API or credential error, or a bare IP gpuprov has no
//! record of), the check falls back to the general IP probes: ping, TCP and
//! SSH. The report's [`Source`] says which of the two judged it.
//!
//! Checking is read-only: it never changes the VM or local state, so it is
//! safe from cron or a monitor. Probes go through [`HealthProbes`], so the
//! logic is testable without a host.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use crate::db::{Db, VmRecord, VmState};
use crate::kube;
use crate::provider::{CloudProvider, InstanceStatus, Location, Severity, SshEndpoint};
use crate::spec::VmSpec;
use crate::ssh::options::SshOptions;
use crate::ssh::{runner, wait};

/// Overall health. Ordered best → worst, so the worst finding wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// Running and usable.
    #[default]
    Healthy,
    /// Running, but not (fully) usable or at risk: unreachable, SSH or a
    /// deep check failing, replaced, moved, or flagged by the vendor.
    Degraded,
    /// The vendor reports it stopped, failed or being deleted — typically a
    /// preemption, eviction or crash.
    Unhealthy,
    /// The vendor no longer has it: terminated or reclaimed.
    Gone,
    /// Neither the vendor nor the network could tell.
    Unknown,
}

impl Verdict {
    /// Process exit code for `gpuprov health`.
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Healthy => 0,
            Verdict::Degraded => 1,
            Verdict::Unhealthy | Verdict::Gone => 2,
            Verdict::Unknown => 3,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Healthy => "HEALTHY",
            Verdict::Degraded => "DEGRADED",
            Verdict::Unhealthy => "UNHEALTHY",
            Verdict::Gone => "GONE",
            Verdict::Unknown => "UNKNOWN",
        })
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Vendor => "vendor",
            Source::Network => "network",
        })
    }
}

/// What the verdict rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The vendor's API answered (`CloudProvider::diagnose`).
    Vendor,
    /// The vendor could not be asked; judged by ping/TCP/SSH alone, which
    /// cannot tell "down" from "reclaimed".
    Network,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

/// One rung of the ladder.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

/// The result of checking one VM (or bare address).
#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    /// VM name, or the address for an unrecorded target.
    pub name: String,
    pub provider: Option<String>,
    pub location: Option<String>,
    pub instance_id: Option<String>,
    /// The endpoint the probes used (the vendor's current view when known).
    pub endpoint: Option<String>,
    pub verdict: Verdict,
    pub source: Source,
    pub checks: Vec<Check>,
}

/// What to check.
#[derive(Debug, Clone, Copy, Default)]
pub struct HealthOptions {
    /// Also check GPUs (`nvidia-smi`) and, for k3s VMs, the cluster.
    pub deep: bool,
}

// ---------------------------------------------------------------------------
// Resolving a handle
// ---------------------------------------------------------------------------

/// What a user-supplied handle refers to.
#[derive(Debug, Clone)]
pub enum Target {
    /// A VM gpuprov has a record of (matched by name, instance id or IP).
    Vm(Box<VmRecord>),
    /// An address gpuprov knows nothing about: network probes only.
    Address(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(
        "no gpuprov VM matches '{0}' (by name, instance id, or IP), and it is not an IP \
         address — see `gpuprov list`"
    )]
    NotFound(String),
    #[error("'{target}' matches several VMs ({}); use the VM name", .names.join(", "))]
    Ambiguous { target: String, names: Vec<String> },
    #[error("state db error: {0}")]
    Db(#[from] rusqlite::Error),
}

/// Resolve a handle: a VM name, an instance id, or an IP. Names win; an id
/// or IP shared by several records (a reused address) is ambiguous unless
/// exactly one is live. An IP gpuprov has no record of becomes a bare
/// [`Target::Address`] (network probes only).
pub fn resolve(db: &Db, target: &str) -> Result<Target, ResolveError> {
    let records = db.list()?;
    if let Some(record) = records.iter().find(|r| r.name == target) {
        return Ok(Target::Vm(Box::new(record.clone())));
    }
    let matches: Vec<&VmRecord> = records
        .iter()
        .filter(|r| {
            r.instance_id.as_deref() == Some(target) || r.public_ip.as_deref() == Some(target)
        })
        .collect();
    let live: Vec<&&VmRecord> = matches
        .iter()
        .filter(|r| r.state != VmState::Terminated)
        .collect();
    match (matches.as_slice(), live.as_slice()) {
        ([], _) if target.parse::<std::net::IpAddr>().is_ok() => {
            Ok(Target::Address(target.to_string()))
        }
        ([], _) => Err(ResolveError::NotFound(target.to_string())),
        ([only], _) => Ok(Target::Vm(Box::new((*only).clone()))),
        (_, [only]) => Ok(Target::Vm(Box::new((**only).clone()))),
        (many, _) => Err(ResolveError::Ambiguous {
            target: target.to_string(),
            names: many.iter().map(|r| r.name.clone()).collect(),
        }),
    }
}

/// The records `gpuprov health` checks when given no target: every VM that
/// may still exist at a vendor.
pub fn live_records(db: &Db) -> Result<Vec<VmRecord>, rusqlite::Error> {
    Ok(db
        .list()?
        .into_iter()
        .filter(|r| r.state != VmState::Terminated && r.region.is_some())
        .collect())
}

// ---------------------------------------------------------------------------
// Probes (the parts that touch the VM)
// ---------------------------------------------------------------------------

/// Probes against the VM's address; [`SystemProbes`] is the real thing.
#[async_trait]
pub trait HealthProbes: Send + Sync {
    /// Does the host answer an ICMP echo?
    async fn ping(&self, host: &str) -> bool;
    /// Does `host:port` accept a TCP connection?
    async fn tcp(&self, host: &str, port: u16) -> bool;
    /// Does an SSH login succeed (banner + key auth)?
    async fn ssh(&self, endpoint: &SshEndpoint, user: &str, key: &Path) -> Result<(), String>;
    /// Run `command` over SSH: `(succeeded, combined output)`.
    async fn exec(
        &self,
        endpoint: &SshEndpoint,
        user: &str,
        key: &Path,
        command: &str,
    ) -> Result<(bool, String), String>;
}

/// The real probes: the system `ping`, a TCP connect, the OpenSSH client.
pub struct SystemProbes;

#[async_trait]
impl HealthProbes for SystemProbes {
    async fn ping(&self, host: &str) -> bool {
        // One echo, short wait. The system binary needs no raw-socket
        // privileges. `-W` is seconds on Linux and milliseconds on macOS;
        // 2000 works for both (the outer timeout bounds Linux's reading).
        let wait = if cfg!(target_os = "macos") {
            "2000"
        } else {
            "2"
        };
        let run = tokio::process::Command::new("ping")
            .args(["-c", "1", "-W", wait, host])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        matches!(
            tokio::time::timeout(Duration::from_secs(5), run).await,
            Ok(Ok(status)) if status.success()
        )
    }

    async fn tcp(&self, host: &str, port: u16) -> bool {
        kube::reachable(host, port).await
    }

    async fn ssh(&self, endpoint: &SshEndpoint, user: &str, key: &Path) -> Result<(), String> {
        let opts = SshOptions::new(key.to_path_buf()).port(endpoint.port);
        wait::probe(&endpoint.host, user, &opts).await
    }

    async fn exec(
        &self,
        endpoint: &SshEndpoint,
        user: &str,
        key: &Path,
        command: &str,
    ) -> Result<(bool, String), String> {
        let opts = SshOptions::new(key.to_path_buf())
            .port(endpoint.port)
            .batch_mode(true)
            .connect_timeout_secs(15);
        let argv = [command.to_string()];
        let run = runner::run_command_capture(user, &endpoint.host, &opts, &argv);
        match tokio::time::timeout(Duration::from_secs(60), run).await {
            Ok(Ok(out)) => Ok((
                out.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            )),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("command timed out after 60s".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

struct Ladder {
    checks: Vec<Check>,
    verdict: Verdict,
    source: Source,
}

impl Ladder {
    fn new() -> Self {
        Ladder {
            checks: Vec::new(),
            verdict: Verdict::Healthy,
            source: Source::Vendor,
        }
    }

    fn push(&mut self, name: &'static str, status: CheckStatus, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            status,
            detail: detail.into(),
        });
    }

    fn worsen(&mut self, verdict: Verdict) {
        self.verdict = self.verdict.max(verdict);
    }

    fn skip(&mut self, names: &[&'static str], why: &str) {
        for name in names {
            self.push(name, CheckStatus::Skip, why);
        }
    }
}

const PROBE_RUNGS: [&str; 5] = ["ping", "network", "ssh", "gpu", "k3s"];

/// Check a recorded VM. `provider` must be the adapter that owns it
/// (`record.provider`); `None` means it could not be built (e.g. missing
/// credentials), which triggers the network fallback.
pub async fn check(
    provider: Option<&dyn CloudProvider>,
    record: &VmRecord,
    probes: &dyn HealthProbes,
    options: HealthOptions,
) -> HealthReport {
    let spec: VmSpec = serde_json::from_str(&record.spec_json).unwrap_or_default();
    let location = record.region.clone().map(|region| Location {
        region,
        zone: record.zone.clone().filter(|z| !z.is_empty()),
    });
    let mut ladder = Ladder::new();
    let mut report = HealthReport {
        name: record.name.clone(),
        provider: record.provider.clone(),
        location: location.as_ref().map(ToString::to_string),
        instance_id: record.instance_id.clone(),
        endpoint: None,
        verdict: Verdict::Healthy,
        source: Source::Vendor,
        checks: Vec::new(),
    };

    // record
    let Some(location) = location else {
        ladder.push(
            "record",
            CheckStatus::Fail,
            "never provisioned (no location)",
        );
        ladder.worsen(Verdict::Gone);
        ladder.skip(&["vendor"], "nothing to check");
        ladder.skip(&PROBE_RUNGS, "nothing to check");
        return finish(report, ladder);
    };
    match record.state {
        VmState::Terminated => {
            ladder.push("record", CheckStatus::Fail, "terminated by `gpuprov down`");
            ladder.worsen(Verdict::Gone);
            ladder.skip(&["vendor"], "VM was torn down");
            ladder.skip(&PROBE_RUNGS, "VM was torn down");
            return finish(report, ladder);
        }
        VmState::Up => ladder.push("record", CheckStatus::Pass, format!("UP at {location}")),
        other => ladder.push(
            "record",
            CheckStatus::Warn,
            format!("{other} at {location} (not UP in the local record)"),
        ),
    }

    // vendor
    let recorded_endpoint = record.public_ip.clone().map(|host| SshEndpoint {
        host,
        port: record.ssh_port.unwrap_or(22),
        user: record.ssh_user.clone(),
    });
    let diagnosis = match provider {
        Some(provider) => provider
            .diagnose(&location, &record.name, record.instance_id.as_deref())
            .await
            .map_err(|e| format!("vendor API unavailable: {e}")),
        None => Err("vendor API unavailable: the provider could not be initialised".into()),
    };
    let endpoint = match diagnosis {
        Err(why) => {
            ladder.source = Source::Network;
            ladder.push(
                "vendor",
                CheckStatus::Warn,
                format!("{why}; falling back to IP reachability"),
            );
            recorded_endpoint
        }
        Ok(diagnosis) => {
            let Some(instance) = diagnosis.instance else {
                ladder.push(
                    "vendor",
                    CheckStatus::Fail,
                    format!(
                        "{} no longer has {} — terminated or reclaimed",
                        record.provider.as_deref().unwrap_or("the vendor"),
                        record.instance_id.as_deref().unwrap_or("the instance")
                    ),
                );
                for finding in &diagnosis.findings {
                    ladder.push("vendor", CheckStatus::Fail, finding.message.clone());
                }
                ladder.worsen(Verdict::Gone);
                ladder.skip(&PROBE_RUNGS, "no instance");
                return finish(report, ladder);
            };
            report.instance_id = Some(instance.instance_id.clone());
            let replaced = record
                .instance_id
                .as_deref()
                .is_some_and(|id| id != instance.instance_id);
            match &instance.status {
                InstanceStatus::Running if replaced => {
                    ladder.push(
                        "vendor",
                        CheckStatus::Warn,
                        format!(
                            "running, but as {} — the recorded {} was replaced",
                            instance.instance_id,
                            record.instance_id.as_deref().unwrap_or_default()
                        ),
                    );
                    ladder.worsen(Verdict::Degraded);
                }
                InstanceStatus::Running => ladder.push(
                    "vendor",
                    CheckStatus::Pass,
                    format!("running ({})", instance.instance_id),
                ),
                InstanceStatus::Pending => {
                    ladder.push(
                        "vendor",
                        CheckStatus::Warn,
                        "starting (pending at the vendor)",
                    );
                    ladder.worsen(Verdict::Degraded);
                }
                status => {
                    let what = match status {
                        InstanceStatus::Stopped => {
                            "stopped (preempted, evicted, or shut down)".to_string()
                        }
                        InstanceStatus::Stopping => {
                            "stopping (being preempted or shut down)".to_string()
                        }
                        InstanceStatus::Terminating => "being deleted".to_string(),
                        InstanceStatus::Failed(reason) => format!("failed: {reason}"),
                        InstanceStatus::Running | InstanceStatus::Pending => unreachable!(),
                    };
                    ladder.push("vendor", CheckStatus::Fail, what);
                    for finding in &diagnosis.findings {
                        ladder.push("vendor", CheckStatus::Fail, finding.message.clone());
                    }
                    ladder.worsen(Verdict::Unhealthy);
                    ladder.skip(&PROBE_RUNGS, "not running");
                    return finish(report, ladder);
                }
            }
            // Vendor-native findings on a running instance.
            for finding in &diagnosis.findings {
                let (status, verdict) = match finding.severity {
                    Severity::Info => (CheckStatus::Pass, Verdict::Healthy),
                    Severity::Warn => (CheckStatus::Warn, Verdict::Degraded),
                    Severity::Critical => (CheckStatus::Fail, Verdict::Unhealthy),
                };
                ladder.push("vendor", status, finding.message.clone());
                ladder.worsen(verdict);
            }
            instance.endpoint.or(recorded_endpoint)
        }
    };

    let Some(endpoint) = endpoint else {
        ladder.push("network", CheckStatus::Fail, "no address known");
        ladder.worsen(if ladder.source == Source::Network {
            Verdict::Unknown
        } else {
            Verdict::Degraded
        });
        ladder.skip(&["ping", "ssh", "gpu", "k3s"], "no address");
        return finish(report, ladder);
    };
    if ladder.source == Source::Vendor {
        if let Some(recorded) = record.public_ip.as_deref() {
            if recorded != endpoint.host {
                ladder.push(
                    "vendor",
                    CheckStatus::Warn,
                    format!(
                        "address changed from {recorded} to {} — re-run `gpuprov provision` to \
                         refresh the SSH config",
                        endpoint.host
                    ),
                );
                ladder.worsen(Verdict::Degraded);
            }
        }
    }
    let user = endpoint
        .user
        .clone()
        .or_else(|| record.ssh_user.clone())
        .unwrap_or_else(|| "ubuntu".into());
    report.endpoint = Some(format!("{user}@{}:{}", endpoint.host, endpoint.port));

    probe_ladder(
        &mut ladder,
        probes,
        &endpoint,
        &user,
        record.ssh_key_path.as_deref().map(PathBuf::from),
        &spec,
        options,
    )
    .await;
    finish(report, ladder)
}

/// Check an address gpuprov has no record of: the generic IP fallback
/// (ping + TCP on `port`). No vendor, no SSH key, so no login check.
pub async fn check_address(address: &str, port: u16, probes: &dyn HealthProbes) -> HealthReport {
    let mut ladder = Ladder::new();
    ladder.source = Source::Network;
    ladder.push(
        "record",
        CheckStatus::Skip,
        "not a gpuprov VM; network probes only",
    );
    ladder.push("vendor", CheckStatus::Skip, "unknown vendor");
    let endpoint = SshEndpoint {
        host: address.to_string(),
        port,
        user: None,
    };
    probe_ladder(
        &mut ladder,
        probes,
        &endpoint,
        "",
        None,
        &VmSpec::default(),
        HealthOptions::default(),
    )
    .await;
    finish(
        HealthReport {
            name: address.to_string(),
            provider: None,
            location: None,
            instance_id: None,
            endpoint: Some(format!("{address}:{port}")),
            verdict: Verdict::Healthy,
            source: Source::Network,
            checks: Vec::new(),
        },
        ladder,
    )
}

/// ping → network → ssh → (deep) gpu, k3s.
async fn probe_ladder(
    ladder: &mut Ladder,
    probes: &dyn HealthProbes,
    endpoint: &SshEndpoint,
    user: &str,
    key: Option<PathBuf>,
    spec: &VmSpec,
    options: HealthOptions,
) {
    let fallback = ladder.source == Source::Network;
    let host = endpoint.host.as_str();

    let pinged = probes.ping(host).await;
    if pinged {
        ladder.push(
            "ping",
            CheckStatus::Pass,
            format!("{host} answers ICMP echo"),
        );
    } else {
        // Clouds commonly block ICMP; only meaningful together with TCP.
        ladder.push(
            "ping",
            CheckStatus::Warn,
            format!("{host} does not answer ICMP echo (often blocked by cloud firewalls)"),
        );
    }

    if probes.tcp(host, endpoint.port).await {
        ladder.push(
            "network",
            CheckStatus::Pass,
            format!("{host}:{} accepts connections", endpoint.port),
        );
    } else {
        ladder.push(
            "network",
            CheckStatus::Fail,
            format!("{host}:{} unreachable", endpoint.port),
        );
        // In the fallback, silence can't tell "down" from "reclaimed".
        ladder.worsen(match (fallback, pinged) {
            (true, false) => Verdict::Unknown,
            _ => Verdict::Degraded,
        });
        ladder.skip(&["ssh", "gpu", "k3s"], "unreachable");
        return;
    }

    let Some(key) = key else {
        ladder.skip(&["ssh", "gpu", "k3s"], "no SSH key for this target");
        return;
    };
    match probes.ssh(endpoint, user, &key).await {
        Ok(()) => ladder.push("ssh", CheckStatus::Pass, format!("login ok as {user}")),
        Err(e) => {
            ladder.push("ssh", CheckStatus::Fail, e);
            ladder.worsen(Verdict::Degraded);
            ladder.skip(&["gpu", "k3s"], "no SSH");
            return;
        }
    }

    if !options.deep {
        ladder.skip(&["gpu", "k3s"], "pass --deep");
        return;
    }
    if spec.gpus.is_some() {
        match probes.exec(endpoint, user, &key, "nvidia-smi -L").await {
            Ok((true, out)) if out.contains("GPU ") => {
                let gpus: Vec<&str> = out.lines().filter(|l| l.starts_with("GPU ")).collect();
                ladder.push(
                    "gpu",
                    CheckStatus::Pass,
                    format!("{} GPU(s): {}", gpus.len(), gpus.join("; ")),
                );
            }
            Ok((_, out)) => {
                ladder.push(
                    "gpu",
                    CheckStatus::Fail,
                    format!("nvidia-smi failed: {}", out.trim()),
                );
                ladder.worsen(Verdict::Degraded);
            }
            Err(e) => {
                ladder.push(
                    "gpu",
                    CheckStatus::Fail,
                    format!("could not run nvidia-smi: {e}"),
                );
                ladder.worsen(Verdict::Degraded);
            }
        }
    } else {
        ladder.push("gpu", CheckStatus::Skip, "no GPU requested");
    }
    if spec.kubernetes == Some(true) {
        match probes
            .exec(endpoint, user, &key, &kube::health_command(None))
            .await
        {
            Ok((_, out)) => {
                let missing = kube::Health::parse(&out).missing();
                if missing.is_empty() {
                    ladder.push("k3s", CheckStatus::Pass, "node Ready, CoreDNS ready");
                } else {
                    ladder.push("k3s", CheckStatus::Fail, missing.join("; "));
                    ladder.worsen(Verdict::Degraded);
                }
            }
            Err(e) => {
                ladder.push(
                    "k3s",
                    CheckStatus::Fail,
                    format!("could not query k3s: {e}"),
                );
                ladder.worsen(Verdict::Degraded);
            }
        }
    } else {
        ladder.push("k3s", CheckStatus::Skip, "not a k3s VM");
    }
}

fn finish(mut report: HealthReport, ladder: Ladder) -> HealthReport {
    report.verdict = ladder.verdict;
    report.source = ladder.source;
    report.checks = ladder.checks;
    report
}

#[cfg(test)]
mod tests;
