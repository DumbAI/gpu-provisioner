//! Health-check tests on the simulated cloud with scriptable probes.

use std::sync::Mutex;

use super::*;
use crate::db::UpFields;
use crate::provider::fake::FakeProvider;
use crate::provider::{
    Capabilities, Diagnosis, Finding, InstanceSpec, Observed, Offer, ProviderError, ResourceRequest,
};

/// Scriptable probes; everything succeeds unless told otherwise.
struct Probes {
    ping: bool,
    tcp: bool,
    ssh: Result<(), String>,
    exec: Mutex<Vec<(String, (bool, String))>>,
}

impl Default for Probes {
    fn default() -> Self {
        Probes {
            ping: true,
            tcp: true,
            ssh: Ok(()),
            exec: Mutex::new(Vec::new()),
        }
    }
}

impl Probes {
    /// Reply `out` to commands containing `needle`.
    fn on_exec(self, needle: &str, ok: bool, out: &str) -> Self {
        self.exec
            .lock()
            .unwrap()
            .push((needle.to_string(), (ok, out.to_string())));
        self
    }
}

#[async_trait]
impl HealthProbes for Probes {
    async fn ping(&self, _: &str) -> bool {
        self.ping
    }
    async fn tcp(&self, _: &str, _: u16) -> bool {
        self.tcp
    }
    async fn ssh(&self, _: &SshEndpoint, _: &str, _: &Path) -> Result<(), String> {
        self.ssh.clone()
    }
    async fn exec(
        &self,
        _: &SshEndpoint,
        _: &str,
        _: &Path,
        command: &str,
    ) -> Result<(bool, String), String> {
        self.exec
            .lock()
            .unwrap()
            .iter()
            .find(|(needle, _)| command.contains(needle.as_str()))
            .map(|(_, reply)| reply.clone())
            .ok_or_else(|| format!("unexpected command: {command}"))
    }
}

/// An adapter that adds vendor-native findings on top of the default
/// diagnosis — the extension point vendors use.
struct WithFindings {
    inner: FakeProvider,
    findings: Vec<Finding>,
}

#[async_trait]
impl CloudProvider for WithFindings {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn plan(&self, r: &ResourceRequest) -> Result<Vec<Offer>, ProviderError> {
        self.inner.plan(r).await
    }
    async fn prepare(
        &self,
        o: &Offer,
        s: &InstanceSpec,
    ) -> Result<serde_json::Value, ProviderError> {
        self.inner.prepare(o, s).await
    }
    async fn create(
        &self,
        o: &Offer,
        p: &serde_json::Value,
        s: &InstanceSpec,
    ) -> Result<String, ProviderError> {
        self.inner.create(o, p, s).await
    }
    async fn observe(&self, at: &Location, vm: &str) -> Result<Vec<Observed>, ProviderError> {
        self.inner.observe(at, vm).await
    }
    async fn delete(&self, at: &Location, vm: &str, ids: &[String]) -> Result<(), ProviderError> {
        self.inner.delete(at, vm, ids).await
    }
    async fn cleanup(&self, at: &Location, vm: &str) -> Result<(), ProviderError> {
        self.inner.cleanup(at, vm).await
    }
    async fn diagnose(
        &self,
        at: &Location,
        vm: &str,
        id: Option<&str>,
    ) -> Result<Diagnosis, ProviderError> {
        let observed = self.inner.observe(at, vm).await?;
        Ok(Diagnosis {
            instance: crate::provider::pick_instance(observed, id),
            findings: self.findings.clone(),
        })
    }
}

/// A recorded UP VM "vm1" in r1 pointing at `instance_id`/`ip`.
fn record(db: &Db, instance_id: &str, ip: &str, spec_json: &str) -> VmRecord {
    db.upsert_provisioning("vm1", "fake", "r1", None, "t", spec_json)
        .unwrap();
    db.mark_up(
        "vm1",
        &UpFields {
            region: "r1".into(),
            zone: String::new(),
            instance_id: instance_id.into(),
            public_ip: ip.into(),
            private_ip: None,
            ssh_user: "ubuntu".into(),
            ssh_port: 22,
            ssh_key_path: "/keys/id".into(),
            ssh_config_path: "/ssh/vm1".into(),
        },
    )
    .unwrap();
    db.get("vm1").unwrap().unwrap()
}

/// A provider with a running instance for vm1; returns (provider, id, ip).
async fn running() -> (FakeProvider, String, String) {
    let provider = FakeProvider::new(vec![]);
    let id = provider.seed_instance("vm1", "r1", InstanceStatus::Running);
    let ip = provider
        .observe(
            &Location {
                region: "r1".into(),
                zone: None,
            },
            "vm1",
        )
        .await
        .unwrap()[0]
        .endpoint
        .clone()
        .unwrap()
        .host;
    (provider, id, ip)
}

fn status_of(report: &HealthReport, name: &str) -> Vec<CheckStatus> {
    report
        .checks
        .iter()
        .filter(|c| c.name == name)
        .map(|c| c.status)
        .collect()
}

#[tokio::test]
async fn healthy_vm_passes_every_rung() {
    let db = Db::open_in_memory().unwrap();
    let (provider, id, ip) = running().await;
    let record = record(&db, &id, &ip, "{}");
    let report = check(
        Some(&provider),
        &record,
        &Probes::default(),
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Healthy, "{report:#?}");
    assert_eq!(report.source, Source::Vendor);
    for rung in ["record", "vendor", "ping", "network", "ssh"] {
        assert_eq!(status_of(&report, rung), [CheckStatus::Pass], "{rung}");
    }
    assert_eq!(
        report.endpoint.as_deref(),
        Some(&*format!("ubuntu@{ip}:22"))
    );
}

#[tokio::test]
async fn instance_missing_at_the_vendor_is_gone() {
    let db = Db::open_in_memory().unwrap();
    let provider = FakeProvider::new(vec![]);
    let record = record(&db, "i-9", "203.0.113.9", "{}");
    let report = check(
        Some(&provider),
        &record,
        &Probes::default(),
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Gone);
    assert!(report.checks.iter().any(|c| c.detail.contains("reclaimed")));
    assert_eq!(status_of(&report, "ssh"), [CheckStatus::Skip]);
}

#[tokio::test]
async fn stopped_or_failed_instance_is_unhealthy() {
    for status in [
        InstanceStatus::Stopped,
        InstanceStatus::Failed("preempted".into()),
    ] {
        let db = Db::open_in_memory().unwrap();
        let provider = FakeProvider::new(vec![]);
        let id = provider.seed_instance("vm1", "r1", status.clone());
        let record = record(&db, &id, "203.0.113.1", "{}");
        let report = check(
            Some(&provider),
            &record,
            &Probes::default(),
            HealthOptions::default(),
        )
        .await;
        assert_eq!(report.verdict, Verdict::Unhealthy, "{status:?}");
    }
}

#[tokio::test]
async fn a_replacement_instance_is_degraded() {
    let db = Db::open_in_memory().unwrap();
    let (provider, _id, ip) = running().await;
    let record = record(&db, "i-old", &ip, "{}");
    let report = check(
        Some(&provider),
        &record,
        &Probes::default(),
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert!(report.checks.iter().any(|c| c.detail.contains("replaced")));
}

#[tokio::test]
async fn a_changed_address_is_degraded() {
    let db = Db::open_in_memory().unwrap();
    let (provider, id, _ip) = running().await;
    let record = record(&db, &id, "198.51.100.200", "{}");
    let report = check(
        Some(&provider),
        &record,
        &Probes::default(),
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert!(report
        .checks
        .iter()
        .any(|c| c.detail.contains("address changed")));
}

#[tokio::test]
async fn running_but_unreachable_or_ssh_failing_is_degraded() {
    let db = Db::open_in_memory().unwrap();
    let (provider, id, ip) = running().await;
    let record = record(&db, &id, &ip, "{}");
    let closed = Probes {
        tcp: false,
        ..Probes::default()
    };
    let report = check(Some(&provider), &record, &closed, HealthOptions::default()).await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert_eq!(status_of(&report, "network"), [CheckStatus::Fail]);

    let locked_out = Probes {
        ssh: Err("Permission denied (publickey)".into()),
        ..Probes::default()
    };
    let report = check(
        Some(&provider),
        &record,
        &locked_out,
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert_eq!(status_of(&report, "ssh"), [CheckStatus::Fail]);
}

#[tokio::test]
async fn blocked_icmp_alone_does_not_degrade() {
    let db = Db::open_in_memory().unwrap();
    let (provider, id, ip) = running().await;
    let record = record(&db, &id, &ip, "{}");
    let no_ping = Probes {
        ping: false,
        ..Probes::default()
    };
    let report = check(Some(&provider), &record, &no_ping, HealthOptions::default()).await;
    assert_eq!(report.verdict, Verdict::Healthy);
    assert_eq!(status_of(&report, "ping"), [CheckStatus::Warn]);
}

#[tokio::test]
async fn vendor_findings_set_the_verdict_by_severity() {
    for (finding, verdict) in [
        (Finding::info("spot instance"), Verdict::Healthy),
        (
            Finding::warn("scheduled retirement on 2026-11-01"),
            Verdict::Degraded,
        ),
        (
            Finding::critical("system status check: impaired"),
            Verdict::Unhealthy,
        ),
    ] {
        let db = Db::open_in_memory().unwrap();
        let (inner, id, ip) = running().await;
        let provider = WithFindings {
            inner,
            findings: vec![finding.clone()],
        };
        let record = record(&db, &id, &ip, "{}");
        let report = check(
            Some(&provider),
            &record,
            &Probes::default(),
            HealthOptions::default(),
        )
        .await;
        assert_eq!(report.verdict, verdict, "{finding:?}");
        assert!(report.checks.iter().any(|c| c.detail == finding.message));
    }
}

#[tokio::test]
async fn without_the_vendor_the_network_decides() {
    let db = Db::open_in_memory().unwrap();
    let record = record(&db, "i-1", "203.0.113.1", "{}");

    // Reachable and our key logs in: healthy, but flagged as network-judged.
    let report = check(None, &record, &Probes::default(), HealthOptions::default()).await;
    assert_eq!(report.verdict, Verdict::Healthy);
    assert_eq!(report.source, Source::Network);
    assert_eq!(status_of(&report, "vendor"), [CheckStatus::Warn]);

    // Silent on every probe: can't tell down from reclaimed.
    let silent = Probes {
        ping: false,
        tcp: false,
        ..Probes::default()
    };
    let report = check(None, &record, &silent, HealthOptions::default()).await;
    assert_eq!(report.verdict, Verdict::Unknown);
}

#[tokio::test]
async fn a_bare_address_gets_network_probes_only() {
    let report = check_address("192.0.2.7", 22, &Probes::default()).await;
    assert_eq!(report.verdict, Verdict::Healthy);
    assert_eq!(report.source, Source::Network);
    assert_eq!(status_of(&report, "ssh"), [CheckStatus::Skip]);

    let silent = Probes {
        ping: false,
        tcp: false,
        ..Probes::default()
    };
    assert_eq!(
        check_address("192.0.2.7", 22, &silent).await.verdict,
        Verdict::Unknown
    );
}

#[tokio::test]
async fn deep_checks_gpu_and_k3s() {
    let db = Db::open_in_memory().unwrap();
    let (provider, id, ip) = running().await;
    let record = record(&db, &id, &ip, r#"{"gpus":"A10G:1","kubernetes":true}"#);
    let deep = HealthOptions { deep: true };

    let good = Probes::default()
        .on_exec("nvidia-smi -L", true, "GPU 0: NVIDIA A10G (UUID: GPU-1)\n")
        .on_exec(
            "GPUPROV_TLS_SAN",
            true,
            "node_ready=True\ncoredns_ready=1\ngpu_present=1\ngpu_allocatable=1\n",
        );
    let report = check(Some(&provider), &record, &good, deep).await;
    assert_eq!(report.verdict, Verdict::Healthy, "{report:#?}");
    assert_eq!(status_of(&report, "gpu"), [CheckStatus::Pass]);
    assert_eq!(status_of(&report, "k3s"), [CheckStatus::Pass]);

    let broken = Probes::default()
        .on_exec("nvidia-smi -L", false, "NVIDIA-SMI has failed")
        .on_exec(
            "GPUPROV_TLS_SAN",
            true,
            "node_ready=True\ncoredns_ready=1\ngpu_present=1\ngpu_allocatable=\n",
        );
    let report = check(Some(&provider), &record, &broken, deep).await;
    assert_eq!(report.verdict, Verdict::Degraded);
    assert_eq!(status_of(&report, "gpu"), [CheckStatus::Fail]);
    assert_eq!(status_of(&report, "k3s"), [CheckStatus::Fail]);
}

#[tokio::test]
async fn a_torn_down_vm_is_gone_without_asking_the_vendor() {
    let db = Db::open_in_memory().unwrap();
    let provider = FakeProvider::new(vec![]);
    let mut rec = record(&db, "i-1", "203.0.113.1", "{}");
    db.set_state("vm1", VmState::Terminated).unwrap();
    rec.state = VmState::Terminated;
    let report = check(
        Some(&provider),
        &rec,
        &Probes::default(),
        HealthOptions::default(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Gone);
    assert_eq!(provider.count_calls("observe"), 0);
}

#[test]
fn resolve_by_name_id_ip_or_bare_address() {
    let db = Db::open_in_memory().unwrap();
    record(&db, "i-1", "203.0.113.1", "{}");
    for handle in ["vm1", "i-1", "203.0.113.1"] {
        assert!(
            matches!(resolve(&db, handle).unwrap(), Target::Vm(r) if r.name == "vm1"),
            "{handle}"
        );
    }
    assert!(matches!(
        resolve(&db, "198.51.100.4").unwrap(),
        Target::Address(a) if a == "198.51.100.4"
    ));
    assert!(matches!(
        resolve(&db, "nope"),
        Err(ResolveError::NotFound(_))
    ));

    // The same IP recorded for two live VMs is ambiguous.
    db.upsert_provisioning("vm2", "fake", "r1", None, "t", "{}")
        .unwrap();
    db.mark_up(
        "vm2",
        &UpFields {
            region: "r1".into(),
            zone: String::new(),
            instance_id: "i-2".into(),
            public_ip: "203.0.113.1".into(),
            private_ip: None,
            ssh_user: "ubuntu".into(),
            ssh_port: 22,
            ssh_key_path: "/k".into(),
            ssh_config_path: "/s".into(),
        },
    )
    .unwrap();
    assert!(matches!(
        resolve(&db, "203.0.113.1"),
        Err(ResolveError::Ambiguous { .. })
    ));
    // …unless only one of them is still live.
    db.set_state("vm2", VmState::Terminated).unwrap();
    assert!(matches!(resolve(&db, "203.0.113.1").unwrap(), Target::Vm(r) if r.name == "vm1"));
}

#[test]
fn exit_codes_rank_by_severity() {
    assert_eq!(Verdict::Healthy.exit_code(), 0);
    assert_eq!(Verdict::Degraded.exit_code(), 1);
    assert_eq!(Verdict::Gone.exit_code(), 2);
    assert_eq!(Verdict::Unknown.exit_code(), 3);
    assert!(Verdict::Gone > Verdict::Unhealthy && Verdict::Unhealthy > Verdict::Degraded);
}
