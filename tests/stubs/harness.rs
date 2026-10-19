//! The scenarios every vendor stub must pass, written once against the
//! [`VendorStub`] trait and run for each vendor by [`vendor_suite!`].
//!
//! Each scenario drives the REAL adapter (built through `provider::build`
//! from an `AppConfig` whose `base_url` points at the stub) and the REAL
//! launch/teardown state machines. Only two things are simulated: the vendor
//! API (the stub) and SSH (an accepting [`Connector`] that records the
//! endpoint it was asked to reach, so the provider-reported login is still
//! checked). The k3s layer needs a real host and is covered by unit tests
//! and the acceptance suite instead.
//!
//! | scenario | proves |
//! |---|---|
//! | `lifecycle` | plan → create → boot → ready (correct SSH login) → down, and the stub holds **no** per-VM billable resources afterwards |
//! | `capacity_failover` | the vendor's documented capacity error on create makes gpuprov compensate and launch the next offer |
//! | `interrupt` | cancelling while the instance boots tears it down: nothing left |
//! | `reuse` | provisioning the same name again adopts the running instance (no second create) |
//! | `health` | `gpuprov health` reports the running VM healthy from the vendor's view, then **gone** after the vendor reclaims it behind gpuprov's back; `down` still cleans up |
//! | `boot_script` | on clouds that run a boot script, the script reaches the vendor in its native form (cloud-init `user_data`, a startup-script resource, …) and teardown removes every boot-script resource |

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use gpu_provisioner::config::AppConfig;
use gpu_provisioner::db::{Db, VmState};
use gpu_provisioner::machine::{Cancel, Executor};
use gpu_provisioner::planner;
use gpu_provisioner::provider::{self, BootScript, CloudProvider, SshEndpoint};
use gpu_provisioner::spec::VmSpec;
use gpu_provisioner::ssh::keys;
use gpu_provisioner::util::Paths;
use gpu_provisioner::vm::health::{self, HealthOptions, HealthProbes, Source, Verdict};
use gpu_provisioner::vm::launch::{Connector, LaunchMachine, LaunchState, ReadyVm};
use gpu_provisioner::vm::teardown::{TeardownMachine, TeardownState};

/// The resource ask a scenario launches.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub gpus: Option<String>,
    pub instance_type: Option<String>,
    pub regions: Vec<String>,
}

/// A stateful simulator of one vendor's API, as far as gpuprov uses it.
#[async_trait]
pub trait VendorStub: Sized + Send + Sync {
    /// The gpuprov provider id this stub stands in for.
    const CLOUD: &'static str;

    /// Start the simulator, seeded with at least two in-stock offers that
    /// satisfy [`VendorStub::ask`] (so failover has somewhere to go).
    async fn start() -> Self;

    /// An `AppConfig` pointing the adapter at this stub: credentials plus the
    /// stub's URL in the cloud's `base_url` (`endpoint_url` for AWS).
    fn config(&self) -> AppConfig;

    /// What the scenarios provision.
    fn ask(&self) -> Ask;

    /// Make the next create fail with the vendor's documented "no capacity"
    /// response (nothing is created).
    fn fail_next_create_with_capacity(&self);

    /// Keep newly created instances in the vendor's booting state until
    /// released (`false`), so a launch can be interrupted mid-boot.
    fn hold_boot(&self, hold: bool);

    /// Per-VM billable resources the simulated account still holds
    /// (instances, volumes, startup scripts, firewall rulesets, …). Shared
    /// resources gpuprov deliberately keeps (SSH keys, per-region
    /// environments) are not leftovers.
    fn leftovers(&self) -> Vec<String>;

    /// Instances the stub accepted (successful creates).
    fn creates(&self) -> usize;

    /// The SSH login (user, port) the vendor reports for a running instance.
    fn expected_login(&self) -> (String, u16);

    /// The boot payloads attached to created instances, as the vendor
    /// received them (cloud-init `user_data`, startup-script bodies, …).
    /// Stubs of clouds without boot scripts keep the default.
    fn boot_payloads(&self) -> Vec<String> {
        Vec::new()
    }

    /// Simulate the vendor reclaiming every instance behind gpuprov's back
    /// (spot eviction, preemption, account-side termination).
    fn reclaim(&self);
}

/// Accepts every SSH probe and records the endpoints it was asked to reach.
#[derive(Default)]
pub struct RecordingConnector {
    pub probed: Mutex<Vec<(String, String, u16)>>,
}

#[async_trait]
impl Connector for RecordingConnector {
    async fn probe(&self, endpoint: &SshEndpoint, user: &str, _key: &Path) -> Result<(), String> {
        self.probed
            .lock()
            .unwrap()
            .push((user.to_string(), endpoint.host.clone(), endpoint.port));
        Ok(())
    }
}

/// One scenario run: temp state, the real adapter, an SSH recorder.
pub struct Run {
    pub name: String,
    pub paths: Paths,
    pub db: Db,
    pub provider: Box<dyn CloudProvider>,
    pub connector: RecordingConnector,
    pub spec: VmSpec,
    pub config: AppConfig,
    _dir: tempfile::TempDir,
}

impl Run {
    pub async fn new<S: VendorStub>(stub: &S, name: &str) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            base: dir.path().join("gpuprov"),
        };
        paths.ensure_dirs().unwrap();
        let db = Db::open(&paths.db_path()).unwrap();
        let config = stub.config();
        let provider = provider::build(S::CLOUD, &config)
            .await
            .expect("adapter builds against the stub");
        let ask = stub.ask();
        let spec = VmSpec {
            name: Some(name.to_string()),
            cloud: Some(S::CLOUD.to_string()),
            gpus: ask.gpus,
            instance_type: ask.instance_type,
            regions: ask.regions,
            // The k3s layer needs a real host (see module docs).
            kubernetes: Some(false),
            ..VmSpec::default()
        };
        Run {
            name: name.to_string(),
            paths,
            db,
            provider,
            connector: RecordingConnector::default(),
            spec,
            config,
            _dir: dir,
        }
    }

    pub fn machine(&self) -> LaunchMachine<'_> {
        let keypair = keys::ensure_keypair(&self.paths.keys_dir()).unwrap();
        let plan = planner::prepare(
            &self.spec,
            &self.config.defaults,
            self.config.default_regions(self.provider.name()),
            &keypair.public_key_openssh,
        )
        .expect("valid scenario spec");
        LaunchMachine {
            provider: &*self.provider,
            db: &self.db,
            paths: &self.paths,
            connector: &self.connector,
            spec: plan.spec,
            request: plan.request,
            required_features: plan.required_features,
            spec_json: serde_json::to_string(&self.spec).unwrap(),
            ssh_key_path: keypair.private_path,
            retry_until_up: false,
            ssh_timeout: Duration::from_secs(60),
        }
    }

    pub async fn launch(&self, cancel: Cancel) -> LaunchState {
        self.launch_with(&self.machine(), cancel).await
    }

    pub async fn launch_with(&self, machine: &LaunchMachine<'_>, cancel: Cancel) -> LaunchState {
        Executor::new(machine, &self.db, &self.name, cancel)
            .run(LaunchState::Reconciling)
            .await
            .expect("launch journal")
    }

    pub async fn down(&self) -> TeardownState {
        let record = self.db.get(&self.name).unwrap().expect("vms row");
        let start = TeardownState::for_record(&record).expect("row has a location");
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

    /// The journal, for failure messages.
    pub fn journal(&self) -> String {
        self.db
            .machine_events(&self.name, 200)
            .unwrap()
            .into_iter()
            .map(|e| match e.detail {
                Some(d) => format!("[{}] {} — {}", e.kind, e.label, d.trim()),
                None => format!("[{}] {}", e.kind, e.label),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn ready(run: &Run, end: LaunchState) -> ReadyVm {
    match end {
        LaunchState::Ready(vm) => vm,
        other => panic!("expected Ready, got {other:?}\njournal:\n{}", run.journal()),
    }
}

fn assert_no_leftovers<S: VendorStub>(stub: &S, run: &Run) {
    let left = stub.leftovers();
    assert!(
        left.is_empty(),
        "{}: billable resources left behind: {left:?}\njournal:\n{}",
        S::CLOUD,
        run.journal()
    );
}

/// plan → create → boot → ready → down, with the right login and no leftovers.
pub async fn lifecycle<S: VendorStub>() {
    let stub = S::start().await;
    let run = Run::new(&stub, "stub-life").await;
    let vm = ready(&run, run.launch(Cancel::new()).await);

    let (user, port) = stub.expected_login();
    assert_eq!(vm.user(), user, "{}: login user", S::CLOUD);
    assert_eq!(vm.endpoint.port, port, "{}: SSH port", S::CLOUD);
    let probed = run.connector.probed.lock().unwrap().clone();
    assert!(
        probed.contains(&(user.clone(), vm.endpoint.host.clone(), port)),
        "{}: SSH was not attempted at the reported endpoint: {probed:?}",
        S::CLOUD
    );
    assert_eq!(
        stub.creates(),
        1,
        "{}: exactly one instance created",
        S::CLOUD
    );
    assert_eq!(run.db.get(&run.name).unwrap().unwrap().state, VmState::Up);

    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&stub, &run);
    assert_eq!(
        run.db.get(&run.name).unwrap().unwrap().state,
        VmState::Terminated
    );
}

/// The vendor's capacity error on create → compensate → next offer.
pub async fn capacity_failover<S: VendorStub>() {
    let stub = S::start().await;
    stub.fail_next_create_with_capacity();
    let run = Run::new(&stub, "stub-fail").await;
    let vm = ready(&run, run.launch(Cancel::new()).await);
    assert_eq!(
        stub.creates(),
        1,
        "{}: the failed create must not leave an instance",
        S::CLOUD
    );
    let journal = run.journal();
    assert!(
        journal.contains("Capacity"),
        "{}: the first offer should fail with a capacity error:\n{journal}",
        S::CLOUD
    );
    assert_eq!(run.db.get(&run.name).unwrap().unwrap().state, VmState::Up);
    drop(vm);

    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&stub, &run);
}

/// Cancel while booting → compensation → nothing left.
pub async fn interrupt<S: VendorStub>() {
    let stub = S::start().await;
    stub.hold_boot(true);
    let run = Run::new(&stub, "stub-int").await;
    let cancel = Cancel::new();
    let trigger = cancel.clone();
    let watcher = async {
        for _ in 0..600 {
            let label = run
                .db
                .load_machine(&run.name)
                .unwrap()
                .map(|row| row.label)
                .unwrap_or_default();
            if label.starts_with("booting") {
                trigger.cancel();
                return true;
            }
            if label.starts_with("UP") || label.starts_with("FAILED") {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    let (end, interrupted) = tokio::join!(run.launch(cancel), watcher);
    assert!(
        interrupted,
        "{}: never reached booting; ended in {end:?}\njournal:\n{}",
        S::CLOUD,
        run.journal()
    );
    match &end {
        LaunchState::Aborted { reason, .. } => {
            assert!(reason.contains("cancelled"), "{reason}")
        }
        other => panic!(
            "{}: expected a cancelled launch, got {other:?}\njournal:\n{}",
            S::CLOUD,
            run.journal()
        ),
    }
    assert_no_leftovers(&stub, &run);
}

/// A second provision of the same name adopts the running instance.
pub async fn reuse<S: VendorStub>() {
    let stub = S::start().await;
    let run = Run::new(&stub, "stub-reuse").await;
    let first = ready(&run, run.launch(Cancel::new()).await);
    let second = ready(&run, run.launch(Cancel::new()).await);
    assert!(second.reused, "{}: second provision should reuse", S::CLOUD);
    assert_eq!(second.instance_id, first.instance_id);
    assert_eq!(stub.creates(), 1, "{}: no second create", S::CLOUD);
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&stub, &run);
}

/// Probes that always succeed: the stub has no real host behind it.
struct ReachableProbes;

#[async_trait]
impl HealthProbes for ReachableProbes {
    async fn ping(&self, _: &str) -> bool {
        true
    }
    async fn tcp(&self, _: &str, _: u16) -> bool {
        true
    }
    async fn ssh(&self, _: &SshEndpoint, _: &str, _: &Path) -> Result<(), String> {
        Ok(())
    }
    async fn exec(
        &self,
        _: &SshEndpoint,
        _: &str,
        _: &Path,
        _: &str,
    ) -> Result<(bool, String), String> {
        Ok((true, String::new()))
    }
}

/// Health from the vendor's view: healthy while running, gone once the
/// vendor reclaims the instance; teardown still completes afterwards.
pub async fn health<S: VendorStub>() {
    let stub = S::start().await;
    let run = Run::new(&stub, "stub-health").await;
    ready(&run, run.launch(Cancel::new()).await);

    let record = run.db.get(&run.name).unwrap().unwrap();
    let report = health::check(
        Some(&*run.provider),
        &record,
        &ReachableProbes,
        HealthOptions::default(),
    )
    .await;
    assert_eq!(
        (report.verdict, report.source),
        (Verdict::Healthy, Source::Vendor),
        "{}: {report:#?}",
        S::CLOUD
    );

    stub.reclaim();
    let report = health::check(
        Some(&*run.provider),
        &record,
        &ReachableProbes,
        HealthOptions::default(),
    )
    .await;
    assert_eq!(
        report.verdict,
        Verdict::Gone,
        "{}: a reclaimed instance must be reported gone: {report:#?}",
        S::CLOUD
    );

    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&stub, &run);
}

/// A boot script reaches the vendor natively and is fully cleaned up. The
/// same structured `BootScript` the k3s layer uses, with a marker body.
pub async fn boot_script<S: VendorStub>() {
    let stub = S::start().await;
    let run = Run::new(&stub, "stub-boot").await;
    if !run.provider.capabilities().boot_script {
        eprintln!(
            "{}: no boot-script support; scenario not applicable",
            S::CLOUD
        );
        return;
    }
    let mut machine = run.machine();
    machine.spec.boot_script = Some(BootScript {
        script: "#!/usr/bin/env bash\necho gpuprov-boot-marker\n".into(),
        env: vec![("GPUPROV_MARKER".into(), "on".into())],
    });
    ready(&run, run.launch_with(&machine, Cancel::new()).await);

    let payloads = stub.boot_payloads();
    assert!(
        payloads
            .iter()
            .any(|p| p.contains("echo gpuprov-boot-marker") && p.contains("GPUPROV_MARKER")),
        "{}: the boot script (body and env) did not reach the vendor; payloads: {payloads:?}",
        S::CLOUD
    );

    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&stub, &run);
}

/// Generate the scenario tests for a vendor stub type.
macro_rules! vendor_suite {
    ($stub:ty) => {
        #[tokio::test]
        async fn lifecycle_leaves_nothing_behind() {
            $crate::stubs::harness::lifecycle::<$stub>().await
        }

        #[tokio::test]
        async fn capacity_error_fails_over_to_next_offer() {
            $crate::stubs::harness::capacity_failover::<$stub>().await
        }

        #[tokio::test]
        async fn interrupt_while_booting_cleans_up() {
            $crate::stubs::harness::interrupt::<$stub>().await
        }

        #[tokio::test]
        async fn reprovision_reuses_the_running_instance() {
            $crate::stubs::harness::reuse::<$stub>().await
        }

        #[tokio::test]
        async fn health_reports_a_reclaimed_vm_as_gone() {
            $crate::stubs::harness::health::<$stub>().await
        }

        #[tokio::test]
        async fn boot_script_is_delivered_and_cleaned_up() {
            $crate::stubs::harness::boot_script::<$stub>().await
        }
    };
}
pub(crate) use vendor_suite;
