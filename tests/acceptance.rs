//! Provider acceptance tests against a REAL cloud account — the bar a cloud
//! adapter must clear before it is labeled supported:
//!
//! 1. `full_lifecycle`: allocate one VM (GPU) → SSH → `nvidia-smi` (GPU asks)
//!    → k3s + GPU readiness (where the cloud supports k3s) → terminate →
//!    verify the cloud reports nothing left for the VM.
//! 2. `interrupted_provision_cleans_up`: start a launch, cancel it once the
//!    instance is booting (as Ctrl-C would), and verify compensation removed
//!    everything.
//!
//! These create billable resources, so they are `#[ignore]`d AND require
//! `GPUPROV_ACCEPT=1`. Configuration (environment):
//!
//! | variable | meaning |
//! |---|---|
//! | `GPUPROV_ACCEPT_CLOUD` | provider id (`hyperstack`, `lambda`, `docker`, …) — required |
//! | `GPUPROV_ACCEPT_GPUS` / `GPUPROV_ACCEPT_INSTANCE_TYPE` | the ask (one of them) |
//! | `GPUPROV_ACCEPT_REGIONS` | comma-separated regions (else the cloud's default) |
//! | `GPUPROV_ACCEPT_K8S` | `0` to skip the k3s stage (default: on where supported) |
//! | `GPUPROV_ACCEPT_CONFIG` | config file with credentials (else `~/.gpu-provisioner/config.toml`, else env vars) |
//!
//! ```text
//! GPUPROV_ACCEPT=1 GPUPROV_ACCEPT_CLOUD=hyperstack GPUPROV_ACCEPT_GPUS=A6000:1 \
//!   cargo test --test acceptance -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Local state goes to a temporary directory; a drop guard tears the VM down
//! if a test fails midway, so a failed run does not keep billing.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpu_provisioner::cluster::k3s::{K3sMachine, K3sState};
use gpu_provisioner::config::AppConfig;
use gpu_provisioner::db::{Db, VmState};
use gpu_provisioner::machine::{Cancel, Executor};
use gpu_provisioner::planner;
use gpu_provisioner::provider::{self, CloudProvider, Location};
use gpu_provisioner::spec::VmSpec;
use gpu_provisioner::ssh::keys;
use gpu_provisioner::ssh::runner;
use gpu_provisioner::ssh::wait;
use gpu_provisioner::util::{self, Paths};
use gpu_provisioner::vm::launch::{LaunchMachine, LaunchState, ReadyVm, SshConnector};
use gpu_provisioner::vm::teardown::{TeardownMachine, TeardownState};

/// How long the interrupt test waits for the launch to reach `Booting`.
const REACH_BOOTING_TIMEOUT: Duration = Duration::from_secs(1200);

struct Settings {
    cloud: String,
    gpus: Option<String>,
    instance_type: Option<String>,
    regions: Vec<String>,
    k8s: bool,
    config: AppConfig,
}

/// `None` (test skipped with a notice) unless `GPUPROV_ACCEPT=1`.
fn settings() -> Option<Settings> {
    if std::env::var("GPUPROV_ACCEPT").as_deref() != Ok("1") {
        eprintln!("SKIP: acceptance tests create billable resources; set GPUPROV_ACCEPT=1");
        return None;
    }
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    let cloud = env("GPUPROV_ACCEPT_CLOUD").expect("GPUPROV_ACCEPT_CLOUD is required");
    let gpus = env("GPUPROV_ACCEPT_GPUS");
    let instance_type = env("GPUPROV_ACCEPT_INSTANCE_TYPE");
    assert!(
        gpus.is_some() != instance_type.is_some(),
        "set exactly one of GPUPROV_ACCEPT_GPUS / GPUPROV_ACCEPT_INSTANCE_TYPE"
    );
    let regions = env("GPUPROV_ACCEPT_REGIONS")
        .map(|r| r.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let k8s = env("GPUPROV_ACCEPT_K8S").as_deref() != Some("0");
    let default_config = dirs_home().join(".gpu-provisioner").join("config.toml");
    let explicit = env("GPUPROV_ACCEPT_CONFIG").map(PathBuf::from);
    let config = AppConfig::load(explicit.as_deref(), &default_config).expect("config file");
    Some(Settings {
        cloud,
        gpus,
        instance_type,
        regions,
        k8s,
        config,
    })
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn unique_name(prefix: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{prefix}-{}", secs % 1_000_000)
}

/// Everything one test run needs: temp state, the provider, a plan.
struct Run {
    name: String,
    _dir: tempfile::TempDir,
    paths: Paths,
    db: Db,
    provider: Box<dyn CloudProvider>,
    spec: VmSpec,
    settings: Settings,
}

impl Run {
    async fn new(prefix: &str, settings: Settings) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            base: dir.path().join("gpuprov"),
        };
        paths.ensure_dirs().unwrap();
        let db = Db::open(&paths.db_path()).unwrap();
        let provider = provider::build(&settings.cloud, &settings.config)
            .await
            .expect("provider credentials");
        let name = unique_name(prefix);
        let spec = VmSpec {
            name: Some(name.clone()),
            cloud: Some(settings.cloud.clone()),
            gpus: settings.gpus.clone(),
            instance_type: settings.instance_type.clone(),
            regions: settings.regions.clone(),
            ..VmSpec::default()
        };
        Run {
            name,
            _dir: dir,
            paths,
            db,
            provider,
            spec,
            settings,
        }
    }

    fn launch_machine(&self) -> LaunchMachine<'_> {
        let keypair = keys::ensure_keypair(&self.paths.keys_dir()).unwrap();
        let plan = planner::prepare(
            &self.spec,
            &self.settings.config.defaults,
            self.settings.config.default_regions(&self.settings.cloud),
            &keypair.public_key_openssh,
        )
        .expect("valid acceptance spec");
        LaunchMachine {
            provider: &*self.provider,
            db: &self.db,
            paths: &self.paths,
            connector: &SshConnector,
            spec: plan.spec,
            request: plan.request,
            required_features: plan.required_features,
            spec_json: serde_json::to_string(&self.spec).unwrap(),
            ssh_key_path: keypair.private_path,
            retry_until_up: false,
            ssh_timeout: wait::DEFAULT_TIMEOUT,
        }
    }

    fn wants_k8s(&self) -> bool {
        self.settings.k8s
            && self
                .provider
                .capabilities()
                .supports(provider::Feature::Kubernetes)
    }

    /// Where the VM's resources live, per the local record.
    fn location(&self) -> Option<Location> {
        let record = self.db.get(&self.name).unwrap()?;
        Some(Location {
            region: record.region?,
            zone: record.zone.filter(|z| !z.is_empty()),
        })
    }

    async fn teardown(&self) -> TeardownState {
        let Some(record) = self.db.get(&self.name).unwrap() else {
            return TeardownState::Done;
        };
        let Some(start) = TeardownState::for_record(&record) else {
            return TeardownState::Done;
        };
        let machine = TeardownMachine {
            provider: &*self.provider,
            db: &self.db,
            paths: &self.paths,
            vm_name: self.name.clone(),
        };
        Executor::new(&machine, &self.db, &self.name, Cancel::new())
            .run(start)
            .await
            .expect("teardown journal")
    }

    /// Assert the cloud reports nothing left for the VM at its location.
    async fn assert_nothing_left(&self) {
        let Some(location) = self.location() else {
            return;
        };
        let left = self
            .provider
            .observe(&location, &self.name)
            .await
            .expect("observe after teardown");
        assert!(
            left.is_empty(),
            "instances still present after teardown: {left:?}"
        );
        // Side resources must be gone too; cleanup is idempotent.
        self.provider
            .cleanup(&location, &self.name)
            .await
            .expect("cleanup is idempotent after teardown");
    }

    /// Print the journaled transitions with elapsed seconds.
    fn report(&self, test: &str) {
        let events = self.db.machine_events(&self.name, 500).unwrap();
        let start = events.first().map(|e| e.ts).unwrap_or_default();
        eprintln!("== {test} on {} ({}) ==", self.settings.cloud, self.name);
        for event in &events {
            eprintln!(
                "  +{:>5}s  {}  [{}] {}",
                event.ts - start,
                util::format_unix_utc(event.ts),
                event.kind,
                event.label
            );
        }
    }
}

/// Tears the VM down if a test panics before its own teardown ran, so a
/// failed acceptance run does not keep billing.
struct Guard<'a> {
    run: &'a Run,
    armed: bool,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        eprintln!(
            "acceptance guard: tearing down '{}' after a failure",
            self.run.name
        );
        let run = self.run;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                let end = rt.block_on(run.teardown());
                eprintln!("acceptance guard: teardown ended in {end:?}");
            });
        });
    }
}

async fn ssh_capture(vm: &ReadyVm, command: &str) -> (bool, String) {
    let out = runner::run_command_capture(
        vm.user(),
        &vm.endpoint.host,
        &vm.ssh_options().batch_mode(true),
        &[command.to_string()],
    )
    .await
    .expect("ssh runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[tokio::test]
#[ignore = "creates billable cloud resources; see module docs"]
async fn full_lifecycle() {
    let Some(settings) = settings() else { return };
    let run = Run::new("accept", settings).await;
    let mut guard = Guard {
        run: &run,
        armed: true,
    };

    // 1–2. Allocate and reach SSH (and k3s + GPU readiness where supported).
    let launch = run.launch_machine();
    let vm = if run.wants_k8s() {
        let machine = K3sMachine::new(launch, &run.paths);
        match Executor::new(&machine, &run.db, &run.name, Cancel::new())
            .run(K3sState::start())
            .await
            .unwrap()
        {
            K3sState::Ready { vm, access } => {
                eprintln!("k3s ready: {}", access.api_endpoint);
                vm
            }
            other => {
                run.report("full_lifecycle");
                panic!("cluster did not become ready: {other:?}");
            }
        }
    } else {
        match Executor::new(&launch, &run.db, &run.name, Cancel::new())
            .run(LaunchState::Reconciling)
            .await
            .unwrap()
        {
            LaunchState::Ready(vm) => vm,
            other => {
                run.report("full_lifecycle");
                panic!("VM did not become ready: {other:?}");
            }
        }
    };
    assert_eq!(run.db.get(&run.name).unwrap().unwrap().state, VmState::Up);

    // 3. Real SSH works; a GPU ask actually has a GPU.
    let (ok, out) = ssh_capture(&vm, "uname -a").await;
    assert!(ok, "ssh failed: {out}");
    if run.settings.gpus.is_some() {
        let (ok, out) = ssh_capture(&vm, "nvidia-smi -L").await;
        assert!(ok && out.contains("GPU"), "nvidia-smi failed: {out}");
        eprintln!("GPUs:\n{}", out.trim());
    }

    // 4. Terminate and verify nothing billable remains.
    let end = run.teardown().await;
    assert_eq!(end, TeardownState::Done, "teardown did not complete");
    guard.armed = false;
    run.assert_nothing_left().await;
    assert_eq!(
        run.db.get(&run.name).unwrap().unwrap().state,
        VmState::Terminated
    );
    run.report("full_lifecycle");
}

#[tokio::test]
#[ignore = "creates billable cloud resources; see module docs"]
async fn interrupted_provision_cleans_up() {
    let Some(settings) = settings() else { return };
    let run = Run::new("accept-int", settings).await;
    let mut guard = Guard {
        run: &run,
        armed: true,
    };

    let launch = run.launch_machine();
    let cancel = Cancel::new();
    let trigger = cancel.clone();
    // Watch the journal and "press Ctrl-C" once an instance exists.
    let watcher = async {
        let deadline = tokio::time::Instant::now() + REACH_BOOTING_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let label = run
                .db
                .load_machine(&run.name)
                .unwrap()
                .map(|row| row.label)
                .unwrap_or_default();
            if label.starts_with("booting") || label.starts_with("waiting for SSH") {
                eprintln!("interrupting at: {label}");
                trigger.cancel();
                return true;
            }
            if label.starts_with("UP") || label.starts_with("FAILED") {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    };
    let executor = Executor::new(&launch, &run.db, &run.name, cancel);
    let (end, interrupted) = tokio::join!(executor.run(LaunchState::Reconciling), watcher);
    let end = end.unwrap();
    run.report("interrupted_provision_cleans_up");
    assert!(
        interrupted,
        "launch never reached Booting; ended in {end:?}"
    );
    match &end {
        LaunchState::Aborted { reason, .. } => assert!(reason.contains("cancelled"), "{reason}"),
        other => panic!("expected a cancelled launch, got {other:?}"),
    }
    guard.armed = false;
    run.assert_nothing_left().await;
}
