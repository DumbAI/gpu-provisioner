//! Launch-machine tests against the simulated cloud (`provider::fake`).
//! Async tests run on paused time, so polls and backoffs cost nothing.

use std::time::Duration;

use super::*;
use crate::db::Db;
use crate::machine::{Cancel, Executor};
use crate::provider::fake::{Behavior, FakeProvider};
use crate::provider::ResourceAsk;

struct OkConnector;

#[async_trait]
impl Connector for OkConnector {
    async fn probe(&self, _: &SshEndpoint, _: &str, _: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
}

struct DeadConnector;

#[async_trait]
impl Connector for DeadConnector {
    async fn probe(&self, _: &SshEndpoint, _: &str, _: &std::path::Path) -> Result<(), String> {
        Err("connection refused".into())
    }
}

struct Harness {
    db: Db,
    paths: Paths,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            base: dir.path().to_path_buf(),
        };
        paths.ensure_dirs().unwrap();
        Harness {
            db: Db::open_in_memory().unwrap(),
            paths,
            _dir: dir,
        }
    }

    fn machine<'a>(
        &'a self,
        provider: &'a FakeProvider,
        types: &str,
        regions: &[&str],
    ) -> LaunchMachine<'a> {
        LaunchMachine {
            provider,
            db: &self.db,
            paths: &self.paths,
            connector: &OkConnector,
            spec: InstanceSpec {
                vm_name: "vm1".into(),
                image_id: None,
                disk_gb: 100,
                spot: false,
                open_ports: Vec::new(),
                user_data: "#cloud-config".into(),
                ssh_public_key: "ssh-ed25519 AAAA".into(),
                ssh_user: "ubuntu".into(),
                boot_script: None,
            },
            request: ResourceRequest {
                ask: ResourceAsk::InstanceType(types.into()),
                regions: regions.iter().map(|r| r.to_string()).collect(),
                zone: None,
                spot: false,
            },
            required_features: Vec::new(),
            spec_json: "{}".into(),
            ssh_key_path: self.paths.keys_dir().join("id"),
            retry_until_up: false,
            ssh_timeout: Duration::from_secs(60),
        }
    }

    async fn run(&self, machine: &LaunchMachine<'_>) -> LaunchState {
        self.run_with(machine, Cancel::new()).await
    }

    async fn run_with(&self, machine: &LaunchMachine<'_>, cancel: Cancel) -> LaunchState {
        Executor::new(machine, &self.db, "vm1", cancel)
            .run(LaunchState::Reconciling)
            .await
            .unwrap()
    }

    fn vm_state(&self) -> Option<VmState> {
        self.db.get("vm1").unwrap().map(|r| r.state)
    }
}

fn ready(state: &LaunchState) -> &ReadyVm {
    match state {
        LaunchState::Ready(vm) => vm,
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn happy_path_records_up_with_ssh_config() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![]);
    let end = h
        .run(&h.machine(&provider, "t3.micro", &["us-west-2"]))
        .await;
    let vm = ready(&end);
    assert_eq!(vm.offer.region, "us-west-2");
    assert!(!vm.reused);
    let rec = h.db.get("vm1").unwrap().unwrap();
    assert_eq!(rec.state, VmState::Up);
    assert_eq!(rec.provider.as_deref(), Some("fake"));
    assert_eq!(rec.ssh_port, Some(22));
    assert!(vm.ssh_config_path.as_ref().unwrap().exists());
    // The journal tells the whole story, ending in Ready.
    let labels: Vec<String> =
        h.db.machine_events("vm1", 50)
            .unwrap()
            .into_iter()
            .map(|e| e.label)
            .collect();
    assert!(labels.first().unwrap().starts_with("reconciling"));
    assert!(labels.iter().any(|l| l.starts_with("booting")));
    assert!(labels.last().unwrap().starts_with("UP"));
}

#[tokio::test(start_paused = true)]
async fn capacity_compensates_and_fails_over_to_next_region() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-west-2",
        Behavior::CreatedButErrors(ProviderError::capacity("InsufficientInstanceCapacity")),
    )]);
    let end = h
        .run(&h.machine(&provider, "t3.micro", &["us-west-2", "us-east-1"]))
        .await;
    assert_eq!(ready(&end).offer.region, "us-east-1");
    // The partial instance in us-west-2 was deleted, then side resources cleaned.
    assert_eq!(provider.count_calls("delete:us-west-2"), 1);
    assert_eq!(provider.count_calls("cleanup:us-west-2"), 1);
    assert_eq!(provider.live(), 1, "only the winner survives");
}

#[tokio::test(start_paused = true)]
async fn capacity_blocks_only_the_offer_so_next_type_in_same_region_is_tried() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-east-2:g6e.xlarge",
        Behavior::CreateFails(ProviderError::capacity("no capacity")),
    )]);
    let end = h
        .run(&h.machine(&provider, "g6e.xlarge,g6e.2xlarge", &["us-east-2"]))
        .await;
    assert_eq!(ready(&end).offer.instance_type, "g6e.2xlarge");
}

#[tokio::test(start_paused = true)]
async fn quota_blocks_every_type_in_the_region() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-west-2",
        Behavior::CreateFails(ProviderError::quota("VcpuLimitExceeded")),
    )]);
    let end = h
        .run(&h.machine(
            &provider,
            "g5.xlarge,g5.2xlarge",
            &["us-west-2", "us-east-1"],
        ))
        .await;
    let vm = ready(&end);
    // Type-major order would try g5.2xlarge@us-west-2 second; quota skipped it.
    assert_eq!(vm.offer.region, "us-east-1");
    assert_eq!(vm.offer.instance_type, "g5.xlarge");
    assert_eq!(provider.count_calls("create:us-west-2"), 1);
}

#[tokio::test(start_paused = true)]
async fn unavailable_skips_without_teardown() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-west-2",
        Behavior::PrepareFails(ProviderError::unavailable("type not offered here")),
    )]);
    let end = h
        .run(&h.machine(&provider, "t3.micro", &["us-west-2", "us-east-1"]))
        .await;
    assert_eq!(ready(&end).offer.region, "us-east-1");
    assert_eq!(provider.count_calls("delete"), 0);
    assert_eq!(provider.count_calls("cleanup"), 0);
}

#[tokio::test(start_paused = true)]
async fn fatal_error_compensates_then_aborts_without_trying_more_offers() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-west-2",
        Behavior::CreatedButErrors(ProviderError::credentials("ExpiredToken")),
    )]);
    let end = h
        .run(&h.machine(&provider, "t3.micro", &["us-west-2", "us-east-1"]))
        .await;
    assert!(matches!(end, LaunchState::Aborted { .. }), "{end:?}");
    assert_eq!(provider.count_calls("create:us-east-1"), 0);
    // Unlike the old engine, the instance created before the fatal error is
    // not left behind.
    assert_eq!(provider.live(), 0);
    assert_eq!(h.vm_state(), Some(VmState::Failed));
}

#[tokio::test(start_paused = true)]
async fn transient_create_error_adopts_the_landed_instance_instead_of_duplicating() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "us-west-2",
        Behavior::CreatedButErrors(ProviderError::transient("read timeout")),
    )]);
    let end = h
        .run(&h.machine(&provider, "t3.micro", &["us-west-2"]))
        .await;
    ready(&end);
    assert_eq!(provider.count_calls("create"), 1, "retry observed first");
    assert_eq!(provider.max_live.lock().unwrap()["vm1"], 1);
}

#[tokio::test(start_paused = true)]
async fn boot_failure_and_boot_timeout_fail_over() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![
        ("r1", Behavior::BootFails),
        ("r2", Behavior::NeverBoots),
    ]);
    let end = h.run(&h.machine(&provider, "t", &["r1", "r2", "r3"])).await;
    assert_eq!(ready(&end).offer.region, "r3");
    assert_eq!(provider.live(), 1);
    let history = match h.db.machine_events("vm1", 100) {
        Ok(events) => events
            .into_iter()
            .filter_map(|e| e.detail)
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => panic!("{e}"),
    };
    assert!(history.contains("kernel panic"), "{history}");
    assert!(history.contains("not ready after"), "{history}");
}

#[tokio::test(start_paused = true)]
async fn exhaustion_reports_history_and_marks_failed() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![
        ("r1", Behavior::CreateFails(ProviderError::capacity("full"))),
        ("r2", Behavior::CreateFails(ProviderError::capacity("full"))),
    ]);
    let end = h.run(&h.machine(&provider, "t", &["r1", "r2"])).await;
    match &end {
        LaunchState::Exhausted { history } => {
            assert_eq!(history.len(), 2);
            let text = History(history).to_string();
            assert!(text.contains("t@r1") && text.contains("t@r2"), "{text}");
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
    assert_eq!(h.vm_state(), Some(VmState::Failed));
}

#[tokio::test(start_paused = true)]
async fn failed_teardown_is_leaked_and_cleaned_by_the_next_run() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![(
        "r1",
        Behavior::CreatedButErrors(ProviderError::capacity("full")),
    )]);
    *provider.delete_failures.lock().unwrap() = u32::MAX;
    let end = h.run(&h.machine(&provider, "t", &["r1", "r2"])).await;
    assert!(matches!(end, LaunchState::Leaked { .. }), "{end:?}");
    assert_eq!(h.vm_state(), Some(VmState::Leaked));
    assert_eq!(
        provider.count_calls("create:r2"),
        0,
        "never moved on while leaking"
    );

    // The API recovers; the next provision cleans up r1 before anything else.
    *provider.delete_failures.lock().unwrap() = 0;
    let end = h.run(&h.machine(&provider, "t", &["r2"])).await;
    assert_eq!(ready(&end).offer.region, "r2");
    assert_eq!(provider.live(), 1);
    let calls = provider.calls();
    let cleanup_r1 = calls.iter().position(|c| c == "cleanup:r1").unwrap();
    let create_r2 = calls.iter().position(|c| c == "create:r2:t").unwrap();
    assert!(cleanup_r1 < create_r2);
}

#[tokio::test(start_paused = true)]
async fn crash_while_booting_resumes_without_a_second_create() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![]);
    let machine = h.machine(&provider, "t", &["r1"]);
    // Simulate a previous process: it created i-1 and journaled Booting, then died.
    let instance_id = provider.seed_instance("vm1", "r1", InstanceStatus::Pending);
    let search = Search::start(vec![Offer::new("t", "r1")]).unwrap();
    let crashed = LaunchState::Booting {
        search,
        instance_id: instance_id.clone(),
    };
    h.db.save_machine(
        "vm1",
        "vm",
        &serde_json::to_string(&crashed).unwrap(),
        "booting",
        false,
        None,
    )
    .unwrap();

    let end = h.run(&machine).await;
    assert_eq!(ready(&end).instance_id, instance_id);
    assert_eq!(provider.count_calls("create"), 0);
}

#[tokio::test(start_paused = true)]
async fn up_record_with_running_instance_is_reused() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![]);
    let first = h.run(&h.machine(&provider, "t", &["r1"])).await;
    let second = h.run(&h.machine(&provider, "t", &["r1"])).await;
    assert!(ready(&second).reused);
    assert_eq!(ready(&second).instance_id, ready(&first).instance_id);
    assert_eq!(provider.count_calls("create"), 1);
}

#[tokio::test(start_paused = true)]
async fn vm_owned_by_another_provider_aborts() {
    let h = Harness::new();
    h.db.upsert_provisioning("vm1", "aws", "us-west-2", None, "t3.micro", "{}")
        .unwrap();
    h.db.set_state("vm1", VmState::Up).unwrap();
    let provider = FakeProvider::new(vec![]);
    let end = h.run(&h.machine(&provider, "t", &["r1"])).await;
    match end {
        LaunchState::Aborted { reason, .. } => assert!(reason.contains("gpuprov down"), "{reason}"),
        other => panic!("expected Aborted, got {other:?}"),
    }
    assert_eq!(provider.count_calls("create"), 0);
    // The other cloud's VM record is untouched.
    assert_eq!(h.vm_state(), Some(VmState::Up));
}

#[tokio::test(start_paused = true)]
async fn unsupported_feature_is_rejected_even_when_a_vm_could_be_reused() {
    let h = Harness::new();
    let mut provider = FakeProvider::new(vec![]);
    provider.unsupported = &[(Feature::OpenPorts, "no firewall")];
    ready(&h.run(&h.machine(&provider, "t", &["r1"])).await);

    let mut machine = h.machine(&provider, "t", &["r1"]);
    machine.required_features = vec![Feature::OpenPorts];
    let end = h.run(&machine).await;
    assert!(matches!(end, LaunchState::Aborted { .. }), "{end:?}");
    // The healthy VM is neither reused nor marked FAILED.
    assert_eq!(h.vm_state(), Some(VmState::Up));
    assert_eq!(provider.live(), 1);
}

#[tokio::test(start_paused = true)]
async fn unsupported_feature_aborts_before_planning() {
    let h = Harness::new();
    let mut provider = FakeProvider::new(vec![]);
    provider.unsupported = &[(Feature::OpenPorts, "no firewall")];
    let mut machine = h.machine(&provider, "t", &["r1"]);
    machine.required_features = vec![Feature::OpenPorts];
    let end = h.run(&machine).await;
    assert!(matches!(end, LaunchState::Aborted { .. }), "{end:?}");
    assert_eq!(provider.count_calls("prepare"), 0);
}

#[tokio::test(start_paused = true)]
async fn ssh_timeout_compensates_and_fails_over() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![]);
    let mut machine = h.machine(&provider, "t", &["r1", "r2"]);
    machine.connector = &DeadConnector;
    let end = h.run(&machine).await;
    // Both offers boot but never accept SSH: both torn down, then exhausted.
    assert!(matches!(end, LaunchState::Exhausted { .. }), "{end:?}");
    assert_eq!(provider.live(), 0);
}

#[tokio::test(start_paused = true)]
async fn provider_dictated_ssh_user_and_port_are_recorded() {
    let h = Harness::new();
    let mut provider = FakeProvider::new(vec![]);
    provider.ssh_user = Some("root".into());
    provider.ssh_port = 2201;
    let end = h.run(&h.machine(&provider, "t", &["r1"])).await;
    assert_eq!(ready(&end).user(), "root");
    let rec = h.db.get("vm1").unwrap().unwrap();
    assert_eq!(rec.ssh_user.as_deref(), Some("root"));
    assert_eq!(rec.ssh_port, Some(2201));
    let config = std::fs::read_to_string(rec.ssh_config_path.unwrap()).unwrap();
    assert!(config.contains("Port 2201") && config.contains("User root"));
}

#[tokio::test(start_paused = true)]
async fn retry_until_up_replans_after_exhaustion() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![]);
    let mut machine = h.machine(&provider, "t", &["r1"]);
    machine.retry_until_up = true;
    // Exhaustion becomes a wait-and-replan instead of a terminal failure...
    let search = Search::start(vec![Offer::new("t", "r1")]).unwrap();
    let history = vec![Failure {
        offer: search.offer().to_string(),
        error: ProviderError::capacity("full"),
    }];
    assert!(matches!(
        machine.exhausted(history.clone()),
        LaunchState::WaitingToRetry { .. }
    ));
    // ...which re-plans after the gap and succeeds once capacity is back.
    let waiting = LaunchState::WaitingToRetry { history };
    h.db.save_machine(
        "vm1",
        "vm",
        &serde_json::to_string(&waiting).unwrap(),
        "w",
        false,
        None,
    )
    .unwrap();
    let end = h.run(&machine).await;
    ready(&end);
}

#[tokio::test(start_paused = true)]
async fn cancel_mid_attempt_tears_down_and_aborts() {
    let h = Harness::new();
    let provider = FakeProvider::new(vec![("r1", Behavior::NeverBoots)]);
    let machine = h.machine(&provider, "t", &["r1"]);
    let cancel = Cancel::new();
    let trigger = cancel.clone();
    let handle = async {
        // Let it get into Booting, then hit Ctrl-C.
        tokio::time::sleep(Duration::from_secs(5)).await;
        trigger.cancel();
    };
    let (end, ()) = tokio::join!(h.run_with(&machine, cancel), handle);
    match end {
        LaunchState::Aborted { reason, .. } => assert!(reason.contains("cancelled")),
        other => panic!("expected Aborted, got {other:?}"),
    }
    assert_eq!(provider.live(), 0);
}

#[test]
fn search_skips_blocked_offers() {
    let offers = vec![
        Offer::new("a", "r1"),
        Offer::new("b", "r1"),
        Offer::new("a", "r2"),
    ];
    let search = Search::start(offers).unwrap();
    let search = search
        .fail_and_advance(ProviderError::quota("limit"))
        .unwrap();
    assert_eq!(search.offer(), &Offer::new("a", "r2"));
    let history = search
        .fail_and_advance(ProviderError::capacity("full"))
        .unwrap_err();
    assert_eq!(history.len(), 2);
    assert!(Search::start(Vec::new()).is_none());
}
