//! End-to-end tests of the launch/teardown state machines against the local
//! Docker test cloud (`provider::docker`): real containers, real sshd, real
//! OpenSSH probing, the real SQLite journal.
//!
//! Ignored by default (they build an image and start containers):
//!
//! ```text
//! cargo test --test docker_e2e -- --ignored --nocapture
//! ```
//!
//! Each test also skips (passes with a notice) when no Docker daemon is
//! reachable — unless `GPUPROV_E2E_REQUIRE_DOCKER=1` (set in CI), where a
//! missing daemon is a failure rather than a silent green. Containers are
//! removed in a drop guard even on failure.

use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpu_provisioner::config::{Defaults, DockerConfig};
use gpu_provisioner::db::{Db, VmState};
use gpu_provisioner::machine::{Cancel, Executor};
use gpu_provisioner::planner::{self, Plan};
use gpu_provisioner::provider::docker::{DockerProvider, LABEL_VM_NAME, REGION};
use gpu_provisioner::provider::{CloudProvider, Offer};
use gpu_provisioner::spec::VmSpec;
use gpu_provisioner::ssh::keys::{self, KeyPair};
use gpu_provisioner::ssh::options::SshOptions;
use gpu_provisioner::ssh::runner;
use gpu_provisioner::util::Paths;
use gpu_provisioner::vm::launch::{LaunchMachine, LaunchState, ReadyVm, Search, SshConnector};
use gpu_provisioner::vm::teardown::{TeardownMachine, TeardownState};

fn docker_available() -> bool {
    let ok = Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        assert!(
            std::env::var_os("GPUPROV_E2E_REQUIRE_DOCKER").is_none(),
            "GPUPROV_E2E_REQUIRE_DOCKER is set but no Docker daemon is reachable"
        );
        eprintln!("SKIP: no reachable Docker daemon");
    }
    ok
}

/// Container ids carrying the VM identity label (any state).
fn containers(vm_name: &str) -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &format!("label={LABEL_VM_NAME}={vm_name}"),
        ])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Removes the VM's containers however the test ends.
struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let ids = containers(&self.0);
        if !ids.is_empty() {
            let _ = Command::new("docker")
                .arg("rm")
                .arg("-f")
                .args(&ids)
                .output();
        }
    }
}

/// A throwaway gpuprov home + DB + key, and a unique VM name.
struct Env {
    _home: tempfile::TempDir,
    paths: Paths,
    db: Db,
    key: KeyPair,
    name: String,
}

impl Env {
    fn new(prefix: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths {
            base: home.path().to_path_buf(),
        };
        paths.ensure_dirs().unwrap();
        let db = Db::open(&paths.db_path()).unwrap();
        let key = keys::ensure_keypair(&paths.keys_dir()).unwrap();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let name = format!("{prefix}-{}-{nanos:x}", std::process::id());
        Env {
            _home: home,
            paths,
            db,
            key,
            name,
        }
    }

    fn plan(&self) -> Plan {
        let spec = VmSpec {
            name: Some(self.name.clone()),
            cloud: Some("docker".into()),
            instance_type: Some("container".into()),
            ..Default::default()
        };
        planner::prepare(
            &spec,
            &Defaults::default(),
            &[],
            &self.key.public_key_openssh,
        )
        .unwrap()
    }

    fn machine<'a>(&'a self, provider: &'a DockerProvider, plan: &Plan) -> LaunchMachine<'a> {
        LaunchMachine {
            provider,
            db: &self.db,
            paths: &self.paths,
            connector: &SshConnector,
            spec: plan.spec.clone(),
            request: plan.request.clone(),
            required_features: plan.required_features.clone(),
            spec_json: "{}".into(),
            ssh_key_path: self.key.private_path.clone(),
            retry_until_up: false,
            ssh_timeout: Duration::from_secs(120),
        }
    }

    async fn launch(&self, provider: &DockerProvider) -> ReadyVm {
        let plan = self.plan();
        let machine = self.machine(provider, &plan);
        let end = Executor::new(&machine, &self.db, &self.name, Cancel::new())
            .run(LaunchState::Reconciling)
            .await
            .unwrap();
        match end {
            LaunchState::Ready(vm) => vm,
            other => panic!("launch did not reach Ready: {other:?}"),
        }
    }

    async fn teardown(&self, provider: &DockerProvider) -> TeardownState {
        let machine = TeardownMachine {
            provider,
            db: &self.db,
            paths: &self.paths,
            vm_name: self.name.clone(),
        };
        // Start from the DB row, exactly as `gpuprov down` does.
        let record = self.db.get(&self.name).unwrap().expect("vms row");
        let start = TeardownState::for_record(&record).expect("row has a location");
        Executor::new(&machine, &self.db, &self.name, Cancel::new())
            .run(start)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn launch_ssh_reuse_and_teardown() {
    if !docker_available() {
        return;
    }
    let env = Env::new("e2e");
    let _guard = ContainerGuard(env.name.clone());
    let provider = DockerProvider::new(&DockerConfig::default());

    // 1. Launch: plan → prepare (image) → create → boot → SSH → UP.
    let vm = env.launch(&provider).await;
    assert!(!vm.reused);
    assert_eq!(vm.endpoint.host, "127.0.0.1");
    assert_ne!(
        vm.endpoint.port, 22,
        "SSH is published on a random host port"
    );
    let record = env.db.get(&env.name).unwrap().unwrap();
    assert_eq!(record.state, VmState::Up);
    assert_eq!(record.provider.as_deref(), Some("docker"));
    assert_eq!(record.ssh_port, Some(vm.endpoint.port));
    assert!(
        std::fs::read_to_string(record.ssh_config_path.as_ref().unwrap())
            .unwrap()
            .contains(&format!("Port {}", vm.endpoint.port))
    );

    // 2. The recorded endpoint really works, with passwordless sudo.
    let user = record.ssh_user.clone().unwrap();
    let opts = SshOptions::new(env.key.private_path.clone())
        .port(record.ssh_port.unwrap())
        .batch_mode(true);
    let out = runner::run_command_capture(
        &user,
        record.public_ip.as_deref().unwrap(),
        &opts,
        &["echo hello && sudo -n true".to_string()],
    )
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "ssh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");

    // 3. Re-provisioning the same name reconciles to the running container.
    let again = env.launch(&provider).await;
    assert!(again.reused);
    assert_eq!(again.instance_id, vm.instance_id);
    assert_eq!(containers(&env.name).len(), 1);

    // 4. Teardown removes the container and records TERMINATED.
    assert_eq!(env.teardown(&provider).await, TeardownState::Done);
    assert!(containers(&env.name).is_empty());
    let record = env.db.get(&env.name).unwrap().unwrap();
    assert_eq!(record.state, VmState::Terminated);
    assert!(!record
        .ssh_config_path
        .map(std::path::PathBuf::from)
        .unwrap()
        .exists());
}

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn crash_after_create_resumes_by_adopting_the_container() {
    if !docker_available() {
        return;
    }
    let env = Env::new("e2e-resume");
    let _guard = ContainerGuard(env.name.clone());
    let provider = DockerProvider::new(&DockerConfig::default());
    let plan = env.plan();

    // Simulate a run that got through Preparing + Creating and then died:
    // the container exists and the journal says Booting.
    let offer = Offer::new("container", REGION);
    env.db
        .upsert_provisioning(&env.name, "docker", REGION, None, "container", "{}")
        .unwrap();
    let prepared = provider.prepare(&offer, &plan.spec).await.unwrap();
    let instance_id = provider
        .create(&offer, &prepared, &plan.spec)
        .await
        .unwrap();
    let crashed = LaunchState::Booting {
        search: Search::start(vec![offer]).unwrap(),
        instance_id: instance_id.clone(),
    };
    env.db
        .save_machine(
            &env.name,
            "vm",
            &serde_json::to_string(&crashed).unwrap(),
            "booting",
            false,
            None,
        )
        .unwrap();

    // The next run resumes at Booting and adopts the same container.
    let vm = env.launch(&provider).await;
    assert_eq!(vm.instance_id, instance_id);
    assert_eq!(containers(&env.name), vec![instance_id]);
    assert_eq!(env.db.get(&env.name).unwrap().unwrap().state, VmState::Up);

    assert_eq!(env.teardown(&provider).await, TeardownState::Done);
    assert!(containers(&env.name).is_empty());
}
