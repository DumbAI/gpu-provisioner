//! Single-node k3s cluster as a durable state machine that **composes** the
//! VM launch machine:
//!
//! ```text
//! Vm(LaunchState…) ─Ready─▶ CheckingBootInstall ─installed at boot─────────────┐
//!                               │ not installed / failed / timed out           ▼
//!                               └──────────────▶ Installing (SSH) ─▶ AwaitingApi ─▶ FetchingKubeconfig ─▶ Validating ─▶ Ready
//!        │ launch failed               retries / deadlines ─────────────────────────────────────────────────────────▶ Failed
//! ```
//!
//! - **Composition**: while in `Vm(child)`, steps are delegated to the
//!   [`LaunchMachine`] and its result lifted; the child's journal lives
//!   inside this machine's single journal row.
//! - **Boot first, SSH as fallback**: on clouds with cloud-init the installer
//!   rides in the VM's user data and runs while the VM boots (overlapping the
//!   SSH wait). `CheckingBootInstall` reads what that run left behind and only
//!   pushes the installer over SSH when it is missing (no cloud-init, reused
//!   VM), failed, or ran too long. The installer is lock-serialized and
//!   idempotent, so the two routes can never conflict.
//! - **Portability**: API access is `Direct` when the cloud can open ports,
//!   else `SshTunnel` (`gpuprov tunnel`). In `Direct` mode the API
//!   certificate must cover the public host; validation checks it and, if the
//!   boot run could not learn the public IP, re-runs the installer once with
//!   the SAN (which re-renders the config and restarts k3s).
//! - **Validation from both sides**: on-node health (node Ready, CoreDNS,
//!   GPU allocatable when a GPU is present) plus, in `Direct` mode, a TCP check
//!   of the API endpoint from here — the path the user will actually use.
//! - **Failure policy**: k3s failures keep the VM (`Failed { vm }`) for
//!   debugging; `gpuprov down` removes it, re-running `provision --k8s`
//!   resumes/reinstalls (the installer is idempotent).
//!
//! See `docs/architecture/state-machines.md` §4.5.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::kube::{self, AccessMode, BootDecision, KubeAccess, Node};
use crate::machine::{Machine, RetryPolicy, Step, StepCx};
use crate::provider::Feature;
use crate::userdata;
use crate::util::Paths;
use crate::vm::launch::{LaunchMachine, LaunchState, ReadyVm};

/// How long a boot-route install may run before the SSH route takes over
/// (the SSH run then waits on the installer lock, so nothing runs twice).
pub const BOOT_INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// API readiness budget after the installer returns.
pub const API_TIMEOUT: Duration = Duration::from_secs(600);
/// Validation budget (device plugin registration can take minutes).
pub const VALIDATE_TIMEOUT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum K3sState {
    /// The VM launch, as a nested child machine.
    Vm(LaunchState),
    /// Did the boot-route install (cloud-init) run, and how did it end?
    CheckingBootInstall {
        vm: ReadyVm,
    },
    /// The SSH route, with why it was needed.
    Installing {
        vm: ReadyVm,
        reason: String,
    },
    AwaitingApi {
        vm: ReadyVm,
    },
    FetchingKubeconfig {
        vm: ReadyVm,
    },
    Validating {
        vm: ReadyVm,
        access: KubeAccess,
        /// The installer was already re-run to add the TLS SAN.
        san_repaired: bool,
    },
    Ready {
        vm: ReadyVm,
        access: KubeAccess,
    },
    /// `vm` is `None` when the VM itself never came up.
    Failed {
        vm: Option<ReadyVm>,
        reason: String,
    },
}

impl K3sState {
    /// Start a cluster launch (the child begins at reconciliation).
    pub fn start() -> Self {
        K3sState::Vm(LaunchState::Reconciling)
    }

    /// Re-enter for an existing VM (e.g. `gpuprov kubeconfig`): probe what
    /// is installed and fall back to the (idempotent) SSH install if needed,
    /// which also repairs a half-installed node.
    pub fn for_existing(vm: ReadyVm) -> Self {
        K3sState::CheckingBootInstall { vm }
    }
}

pub struct K3sMachine<'a> {
    pub launch: LaunchMachine<'a>,
    pub paths: &'a Paths,
    pub mode: AccessMode,
}

impl<'a> K3sMachine<'a> {
    /// Wrap a VM launch, adapting it to the provider's capabilities: embed the
    /// installer in the boot user data when the cloud runs cloud-init, and
    /// pick the API access mode (`Direct` asks the VM layer for the k3s ports).
    pub fn new(mut launch: LaunchMachine<'a>, paths: &'a Paths) -> Self {
        let caps = launch.provider.capabilities();
        if caps.cloud_init {
            launch.spec.user_data = userdata::with_boot_script(
                &launch.spec.user_data,
                kube::BOOT_SCRIPT_PATH,
                kube::K3S_INSTALL_SCRIPT,
                &[("GPUPROV_INSTALL_ORIGIN", "boot")],
            );
        }
        let mode = if caps.supports(Feature::OpenPorts) {
            for port in kube::K3S_PORTS {
                if !launch.spec.open_ports.contains(&port) {
                    launch.spec.open_ports.push(port);
                }
            }
            AccessMode::Direct
        } else {
            AccessMode::SshTunnel
        };
        K3sMachine {
            launch,
            paths,
            mode,
        }
    }

    fn node<'v>(&self, vm: &'v ReadyVm) -> Node<'v> {
        Node {
            host: &vm.endpoint.host,
            user: vm.user(),
            opts: vm
                .ssh_options()
                .batch_mode(true)
                .control_dir(Some(self.paths.sockets_dir())),
        }
    }

    fn failed(&self, vm: &ReadyVm, reason: String) -> K3sState {
        K3sState::Failed {
            vm: Some(vm.clone()),
            reason,
        }
    }

    async fn check_boot_install(&self, vm: &ReadyVm, cx: &StepCx) -> Step<K3sState> {
        let probe = match kube::probe_boot(&self.node(vm)).await {
            Ok(probe) => probe,
            Err(e) => {
                return Step::Retry {
                    error: e.to_string(),
                }
            }
        };
        let fallback = |reason: String| {
            Step::Next(K3sState::Installing {
                vm: vm.clone(),
                reason,
            })
        };
        match probe.decide() {
            BootDecision::Installed => Step::Next(K3sState::AwaitingApi { vm: vm.clone() }),
            BootDecision::Fallback(reason) => fallback(reason),
            BootDecision::Wait(_) if cx.in_state >= BOOT_INSTALL_TIMEOUT => fallback(format!(
                "boot install still running after {}s",
                BOOT_INSTALL_TIMEOUT.as_secs()
            )),
            BootDecision::Wait(note) => Step::Poll { after: POLL, note },
        }
    }

    async fn validate(
        &self,
        vm: &ReadyVm,
        access: &KubeAccess,
        san_repaired: bool,
        cx: &StepCx,
    ) -> Step<K3sState> {
        // Only Direct mode reaches the API by the public host; the tunnel
        // uses 127.0.0.1, which k3s always certifies.
        let san = (access.mode == AccessMode::Direct).then_some(vm.endpoint.host.as_str());
        let health = match kube::health(&self.node(vm), san).await {
            Ok(h) => h,
            Err(e) => {
                return Step::Retry {
                    error: e.to_string(),
                }
            }
        };
        let mut missing: Vec<String> = health.missing().into_iter().map(String::from).collect();
        if access.mode == AccessMode::Direct
            && !kube::reachable(&vm.endpoint.host, kube::API_PORT).await
        {
            missing.push(format!(
                "API {}:{} not reachable from this machine (firewall?)",
                vm.endpoint.host,
                kube::API_PORT
            ));
        }
        if missing.is_empty() {
            return Step::Next(K3sState::Ready {
                vm: vm.clone(),
                access: access.clone(),
            });
        }
        if health.san_ok == Some(false) && !san_repaired {
            // The boot run could not learn the public IP: the SSH run adds
            // the SAN, re-renders the config and restarts k3s.
            return match kube::install(&self.node(vm), &vm.endpoint.host).await {
                Ok(()) => Step::Next(K3sState::Validating {
                    vm: vm.clone(),
                    access: access.clone(),
                    san_repaired: true,
                }),
                Err(e) => Step::Retry {
                    error: e.to_string(),
                },
            };
        }
        if cx.in_state >= VALIDATE_TIMEOUT {
            return Step::Next(self.failed(
                vm,
                format!(
                    "cluster not healthy after {}s: {}",
                    VALIDATE_TIMEOUT.as_secs(),
                    missing.join("; ")
                ),
            ));
        }
        Step::Poll {
            after: POLL,
            note: missing.join("; "),
        }
    }
}

#[async_trait]
impl Machine for K3sMachine<'_> {
    type State = K3sState;
    const KIND: &'static str = "k3s";

    fn is_terminal(&self, s: &K3sState) -> bool {
        matches!(s, K3sState::Ready { .. } | K3sState::Failed { .. })
    }

    fn label(&self, s: &K3sState) -> String {
        match s {
            K3sState::Vm(child) => format!("vm: {}", self.launch.label(child)),
            K3sState::CheckingBootInstall { .. } => "checking the boot-time k3s install".into(),
            K3sState::Installing { vm, .. } => {
                format!("installing k3s over SSH on {}", vm.endpoint.host)
            }
            K3sState::AwaitingApi { .. } => "waiting for the k3s API".into(),
            K3sState::FetchingKubeconfig { .. } => "fetching kubeconfig".into(),
            K3sState::Validating { .. } => "validating cluster health".into(),
            K3sState::Ready { access, .. } => format!("cluster ready: {}", access.api_endpoint),
            K3sState::Failed { reason, .. } => format!("cluster FAILED: {reason}"),
        }
    }

    fn detail(&self, s: &K3sState) -> Option<String> {
        match s {
            K3sState::Vm(child) => self.launch.detail(child),
            K3sState::Installing { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }

    async fn step(&self, s: &K3sState, cx: &StepCx) -> Step<K3sState> {
        match s {
            K3sState::Vm(child) => match self.launch.step(child, cx).await {
                Step::Next(LaunchState::Ready(vm)) => {
                    Step::Next(K3sState::CheckingBootInstall { vm })
                }
                Step::Next(next) if self.launch.is_terminal(&next) => {
                    let reason = next
                        .failure_summary()
                        .unwrap_or_else(|| self.launch.label(&next));
                    Step::Next(K3sState::Failed { vm: None, reason })
                }
                Step::Next(next) => Step::Next(K3sState::Vm(next)),
                Step::Poll { after, note } => Step::Poll { after, note },
                Step::Retry { error } => Step::Retry { error },
            },
            K3sState::CheckingBootInstall { vm } => self.check_boot_install(vm, cx).await,
            K3sState::Installing { vm, .. } => {
                match kube::install(&self.node(vm), &vm.endpoint.host).await {
                    Ok(()) => Step::Next(K3sState::AwaitingApi { vm: vm.clone() }),
                    Err(e) => Step::Retry {
                        error: e.to_string(),
                    },
                }
            }
            K3sState::AwaitingApi { vm } => match kube::api_ready(&self.node(vm)).await {
                Ok(true) => Step::Next(K3sState::FetchingKubeconfig { vm: vm.clone() }),
                Ok(false) if cx.in_state >= API_TIMEOUT => Step::Next(self.failed(
                    vm,
                    format!(
                        "k3s API not ready after {}s; inspect `gpuprov ssh {} -- sudo journalctl \
                         -u k3s`",
                        API_TIMEOUT.as_secs(),
                        vm.name
                    ),
                )),
                Ok(false) => Step::Poll {
                    after: POLL,
                    note: "/readyz not ok yet".into(),
                },
                Err(e) => Step::Retry {
                    error: e.to_string(),
                },
            },
            K3sState::FetchingKubeconfig { vm } => {
                match kube::fetch_kubeconfig(&self.node(vm), self.paths, &vm.name, self.mode).await
                {
                    Ok(access) => Step::Next(K3sState::Validating {
                        vm: vm.clone(),
                        access,
                        san_repaired: false,
                    }),
                    Err(e) => Step::Retry {
                        error: e.to_string(),
                    },
                }
            }
            K3sState::Validating {
                vm,
                access,
                san_repaired,
            } => self.validate(vm, access, *san_repaired, cx).await,
            terminal => unreachable!("terminal state {terminal:?} is never stepped"),
        }
    }

    fn retry_policy(&self, s: &K3sState) -> RetryPolicy {
        match s {
            K3sState::Vm(child) => self.launch.retry_policy(child),
            // The installer is long and mostly fails for real (apt/network);
            // three full attempts is plenty.
            K3sState::Installing { .. } => RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_secs(10),
                max_backoff: Duration::from_secs(30),
            },
            _ => RetryPolicy::default(),
        }
    }

    fn on_retries_exhausted(&self, s: &K3sState, last_error: String) -> K3sState {
        match s {
            K3sState::Vm(child) => {
                K3sState::Vm(self.launch.on_retries_exhausted(child, last_error))
            }
            K3sState::CheckingBootInstall { vm }
            | K3sState::Installing { vm, .. }
            | K3sState::AwaitingApi { vm }
            | K3sState::FetchingKubeconfig { vm }
            | K3sState::Validating { vm, .. } => self.failed(vm, last_error),
            terminal => terminal.clone(),
        }
    }

    /// Cancelling during the VM phase compensates like a plain launch;
    /// during cluster setup the VM is kept and the journal stays resumable.
    fn on_cancel(&self, s: &K3sState) -> Option<K3sState> {
        match s {
            K3sState::Vm(child) => self.launch.on_cancel(child).map(K3sState::Vm),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::machine::{Cancel, Executor};
    use crate::provider::fake::{Behavior, FakeProvider};
    use crate::provider::{InstanceSpec, ProviderError, ResourceAsk, ResourceRequest};
    use crate::vm::launch::{Connector, LaunchMachine};

    struct OkConnector;

    #[async_trait]
    impl Connector for OkConnector {
        async fn probe(
            &self,
            _: &crate::provider::SshEndpoint,
            _: &str,
            _: &std::path::Path,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    fn launch<'a>(provider: &'a FakeProvider, db: &'a Db, paths: &'a Paths) -> LaunchMachine<'a> {
        LaunchMachine {
            provider,
            db,
            paths,
            connector: &OkConnector,
            spec: InstanceSpec {
                vm_name: "vm1".into(),
                image_id: None,
                disk_gb: 100,
                spot: false,
                open_ports: vec![8080],
                user_data: String::new(),
                ssh_public_key: "ssh-ed25519 AAAA".into(),
                ssh_user: "ubuntu".into(),
            },
            request: ResourceRequest {
                ask: ResourceAsk::InstanceType("t".into()),
                regions: vec!["r1".into()],
                zone: None,
                spot: false,
            },
            required_features: Vec::new(),
            spec_json: "{}".into(),
            ssh_key_path: paths.keys_dir().join("id"),
            retry_until_up: false,
            ssh_timeout: Duration::from_secs(60),
        }
    }

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            base: dir.path().to_path_buf(),
        };
        paths.ensure_dirs().unwrap();
        (dir, paths)
    }

    #[test]
    fn access_mode_follows_provider_capabilities() {
        let (_dir, paths) = paths();
        let db = Db::open_in_memory().unwrap();

        let open = FakeProvider::new(vec![]);
        let machine = K3sMachine::new(launch(&open, &db, &paths), &paths);
        assert_eq!(machine.mode, AccessMode::Direct);
        // User ports kept, k3s ports added once.
        assert_eq!(machine.launch.spec.open_ports, vec![8080, 6443, 80, 443]);

        let mut closed = FakeProvider::new(vec![]);
        closed.unsupported = &[(Feature::OpenPorts, "no firewall")];
        let machine = K3sMachine::new(launch(&closed, &db, &paths), &paths);
        assert_eq!(machine.mode, AccessMode::SshTunnel);
        assert_eq!(machine.launch.spec.open_ports, vec![8080]);
    }

    #[test]
    fn installer_rides_in_user_data_only_on_cloud_init_clouds() {
        let (_dir, paths) = paths();
        let db = Db::open_in_memory().unwrap();

        let mut booting = FakeProvider::new(vec![]);
        booting.cloud_init = true;
        let machine = K3sMachine::new(launch(&booting, &db, &paths), &paths);
        let user_data: serde_yaml::Value =
            serde_yaml::from_str(&machine.launch.spec.user_data).unwrap();
        assert_eq!(user_data["write_files"][0]["path"], kube::BOOT_SCRIPT_PATH);
        assert!(machine
            .launch
            .spec
            .user_data
            .contains("GPUPROV_INSTALL_ORIGIN=boot"));

        let plain = FakeProvider::new(vec![]);
        let machine = K3sMachine::new(launch(&plain, &db, &paths), &paths);
        assert!(!machine.launch.spec.user_data.contains("write_files"));
    }

    #[test]
    fn existing_vms_start_by_probing_not_reinstalling() {
        let json = serde_json::json!({
            "name": "vm1", "provider": "fake",
            "offer": {"instance_type": "t", "region": "r1", "zone": null},
            "zone": null, "instance_id": "i-1",
            "endpoint": {"host": "203.0.113.1", "port": 22, "user": "ubuntu"},
            "private_ip": null, "ssh_key_path": "/k", "ssh_config_path": null, "reused": true
        });
        let vm: ReadyVm = serde_json::from_value(json).unwrap();
        assert!(matches!(
            K3sState::for_existing(vm),
            K3sState::CheckingBootInstall { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn vm_failure_lifts_into_cluster_failure_without_installing() {
        let (_dir, paths) = paths();
        let db = Db::open_in_memory().unwrap();
        let provider = FakeProvider::new(vec![(
            "r1",
            Behavior::CreateFails(ProviderError::capacity("full")),
        )]);
        let machine = K3sMachine::new(launch(&provider, &db, &paths), &paths);
        let end = Executor::new(&machine, &db, "vm1", Cancel::new())
            .run(K3sState::start())
            .await
            .unwrap();
        match end {
            K3sState::Failed { vm: None, reason } => {
                assert!(reason.contains("every offer"), "{reason}");
                assert!(reason.contains("full"), "{reason}");
            }
            other => panic!("expected Failed without VM, got {other:?}"),
        }
        // One journal row for the whole nested run, kind k3s.
        assert_eq!(db.load_machine("vm1").unwrap().unwrap().kind, "k3s");
    }

    #[test]
    fn child_states_round_trip_through_the_parent_journal() {
        let state = K3sState::Vm(LaunchState::Planning);
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<K3sState>(&json).unwrap(), state);
    }
}
