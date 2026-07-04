//! The failover engine ("Phase 4") — a port of SkyPilot's
//! `RetryingVmProvisioner.provision_with_retries` collapsed to one cloud:
//! walk the planner's candidate regions in order; per candidate run
//! bootstrap → run_instance → wait; on failure classify the error, tear down
//! partial resources when needed, block the region, and continue. Fatal
//! errors (credentials, failed teardown) abort the whole loop.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use crate::db::{Db, VmState};
use crate::planner::{build_request, Plan};
use crate::provider::{CloudProvider, ProvisionError, ProvisionRecord, ProvisionRequest};

/// SkyPilot tears down partial resources with up to 3 attempts, 5s apart.
const TEARDOWN_MAX_ATTEMPTS: usize = 3;
const TEARDOWN_BACKOFF: Duration = Duration::from_secs(5);
/// SkyPilot `_bulk_provision`: 1s settle, then up to 3 wait attempts with
/// backoff capped at 3s.
const WAIT_RETRY_MAX: usize = 3;
const WAIT_SETTLE: Duration = Duration::from_secs(1);
const WAIT_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct AttemptFailure {
    pub region: String,
    pub error: ProvisionError,
}

#[derive(Debug)]
pub struct FailoverOutcome {
    pub record: ProvisionRecord,
    pub request: ProvisionRequest,
}

#[derive(Debug)]
pub enum FailoverError {
    /// Every candidate failed with a retryable error (the
    /// `ResourcesUnavailableError` analog) — `--retry-until-up` retries this.
    Exhausted {
        history: Vec<AttemptFailure>,
    },
    /// A fatal error ended failover early (credentials, failed teardown).
    Fatal {
        error: ProvisionError,
        history: Vec<AttemptFailure>,
    },
    Db(rusqlite::Error),
}

impl fmt::Display for FailoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailoverError::Exhausted { history } => {
                write!(f, "provisioning failed in all candidate regions:")?;
                for attempt in history {
                    write!(f, "\n  - {}: {}", attempt.region, attempt.error)?;
                }
                Ok(())
            }
            FailoverError::Fatal { error, history } => {
                write!(f, "{error}")?;
                if !history.is_empty() {
                    write!(f, "\nearlier failover attempts:")?;
                    for attempt in history {
                        write!(f, "\n  - {}: {}", attempt.region, attempt.error)?;
                    }
                }
                Ok(())
            }
            FailoverError::Db(e) => write!(f, "state db error: {e}"),
        }
    }
}

impl std::error::Error for FailoverError {}

/// Try each candidate region in order until one yields a running instance.
/// On success the DB row is left in `PROVISIONING` (the orchestrator marks
/// `UP` after SSH is confirmed); on failure it is marked `FAILED`.
pub async fn provision_with_failover(
    provider: &dyn CloudProvider,
    plan: &Plan,
    db: &Db,
    spec_json: &str,
) -> Result<FailoverOutcome, FailoverError> {
    let mut history: Vec<AttemptFailure> = Vec::new();
    let mut blocked: HashSet<String> = HashSet::new();

    for candidate in &plan.candidates {
        if blocked.contains(&candidate.region) {
            continue;
        }
        tracing::info!(
            "[{}] attempting to provision '{}' ({})",
            candidate.region,
            plan.vm_name,
            plan.instance_type
        );
        // ≈ SkyPilot recording the cluster as INIT before provisioning.
        db.upsert_provisioning(&plan.vm_name, &candidate.region, spec_json)
            .map_err(FailoverError::Db)?;
        let request = build_request(plan, candidate);

        match attempt(provider, &request).await {
            Ok(record) => return Ok(FailoverOutcome { record, request }),
            Err(error) if error.is_fatal() => {
                db.set_state(&plan.vm_name, VmState::Failed)
                    .map_err(FailoverError::Db)?;
                return Err(FailoverError::Fatal { error, history });
            }
            Err(error) => {
                tracing::warn!("[{}] attempt failed: {error}", candidate.region);
                if error.needs_teardown() {
                    if let Err(teardown_error) =
                        teardown(provider, &candidate.region, &plan.vm_name).await
                    {
                        history.push(AttemptFailure {
                            region: candidate.region.clone(),
                            error,
                        });
                        db.set_state(&plan.vm_name, VmState::Failed)
                            .map_err(FailoverError::Db)?;
                        return Err(FailoverError::Fatal {
                            error: teardown_error,
                            history,
                        });
                    }
                }
                blocked.insert(candidate.region.clone());
                history.push(AttemptFailure {
                    region: candidate.region.clone(),
                    error,
                });
            }
        }
    }

    db.set_state(&plan.vm_name, VmState::Failed)
        .map_err(FailoverError::Db)?;
    Err(FailoverError::Exhausted { history })
}

/// One candidate attempt: bootstrap → run_instance → wait-until-running
/// (with a small retry around the wait, mirroring SkyPilot's `_bulk_provision`).
async fn attempt(
    provider: &dyn CloudProvider,
    request: &ProvisionRequest,
) -> Result<ProvisionRecord, ProvisionError> {
    let bootstrapped = provider.bootstrap(request).await?;
    let record = provider.run_instance(request, &bootstrapped).await?;

    tokio::time::sleep(WAIT_SETTLE).await;
    let mut delay = Duration::from_secs(1);
    let mut last_error = None;
    for wait_attempt in 0..WAIT_RETRY_MAX {
        match provider
            .wait_running(&request.region, &request.vm_name)
            .await
        {
            Ok(()) => return Ok(record),
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                tracing::warn!(
                    "[{}] waiting for instance failed (attempt {}/{WAIT_RETRY_MAX}): {e}",
                    request.region,
                    wait_attempt + 1
                );
                last_error = Some(e);
                if wait_attempt + 1 < WAIT_RETRY_MAX {
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 3, WAIT_RETRY_MAX_BACKOFF);
                }
            }
        }
    }
    Err(last_error.expect("wait loop ran at least once"))
}

/// Tear down partial resources after a failed attempt. Exhausting retries is
/// fatal (`TeardownFailed`) — continuing to fail over while resources leak
/// would multiply the leak (SkyPilot's `StopFailoverError`).
async fn teardown(
    provider: &dyn CloudProvider,
    region: &str,
    vm_name: &str,
) -> Result<(), ProvisionError> {
    tracing::info!("[{region}] tearing down partial resources for '{vm_name}'");
    let mut last_msg = String::new();
    for attempt in 0..TEARDOWN_MAX_ATTEMPTS {
        match provider.terminate(region, vm_name).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "[{region}] teardown attempt {}/{TEARDOWN_MAX_ATTEMPTS} failed: {e}",
                    attempt + 1
                );
                last_msg = e.to_string();
                if attempt + 1 < TEARDOWN_MAX_ATTEMPTS {
                    tokio::time::sleep(TEARDOWN_BACKOFF).await;
                }
            }
        }
    }
    Err(ProvisionError::TeardownFailed {
        region: region.to_string(),
        msg: last_msg,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::config::Defaults;
    use crate::provider::{Bootstrapped, SubnetInfo, VmInfo};
    use crate::spec::VmSpec;

    /// Scripted per-region behavior for the fake provider.
    #[derive(Clone, Copy, PartialEq)]
    enum Behavior {
        Success,
        CapacityOnRun,
        ConfigOnBootstrap,
        CredentialsOnRun,
    }

    struct FakeProvider {
        behaviors: Vec<(&'static str, Behavior)>,
        fail_terminate: bool,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProvider {
        fn new(behaviors: Vec<(&'static str, Behavior)>) -> Self {
            Self {
                behaviors,
                fail_terminate: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn behavior(&self, region: &str) -> Behavior {
            self.behaviors
                .iter()
                .find(|(r, _)| *r == region)
                .map(|(_, b)| *b)
                .unwrap_or(Behavior::Success)
        }

        fn log(&self, entry: String) {
            self.calls.lock().unwrap().push(entry);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CloudProvider for FakeProvider {
        async fn bootstrap(&self, req: &ProvisionRequest) -> Result<Bootstrapped, ProvisionError> {
            self.log(format!("bootstrap:{}", req.region));
            if self.behavior(&req.region) == Behavior::ConfigOnBootstrap {
                return Err(ProvisionError::Config {
                    region: req.region.clone(),
                    msg: "no default VPC".into(),
                });
            }
            Ok(Bootstrapped {
                subnets: vec![SubnetInfo {
                    subnet_id: "subnet-1".into(),
                    zone: format!("{}a", req.region),
                }],
                security_group_id: "sg-1".into(),
                resolved_ami: "ami-1".into(),
            })
        }

        async fn run_instance(
            &self,
            req: &ProvisionRequest,
            _bootstrapped: &Bootstrapped,
        ) -> Result<ProvisionRecord, ProvisionError> {
            self.log(format!("run:{}", req.region));
            match self.behavior(&req.region) {
                Behavior::CapacityOnRun => Err(ProvisionError::Capacity {
                    region: req.region.clone(),
                    zone: Some(format!("{}a", req.region)),
                    msg: "InsufficientInstanceCapacity".into(),
                }),
                Behavior::CredentialsOnRun => Err(ProvisionError::Credentials {
                    msg: "ExpiredToken".into(),
                }),
                _ => Ok(ProvisionRecord {
                    region: req.region.clone(),
                    zone: format!("{}a", req.region),
                    instance_id: "i-123".into(),
                    reused: false,
                }),
            }
        }

        async fn wait_running(&self, region: &str, _vm_name: &str) -> Result<(), ProvisionError> {
            self.log(format!("wait:{region}"));
            Ok(())
        }

        async fn describe(
            &self,
            _region: &str,
            _vm_name: &str,
        ) -> Result<Option<VmInfo>, ProvisionError> {
            Ok(None)
        }

        async fn terminate(&self, region: &str, _vm_name: &str) -> Result<(), ProvisionError> {
            self.log(format!("terminate:{region}"));
            if self.fail_terminate {
                return Err(ProvisionError::Other {
                    region: region.to_string(),
                    msg: "api down".into(),
                });
            }
            Ok(())
        }
    }

    fn make_plan(regions: &[&str]) -> Plan {
        let spec = VmSpec {
            name: Some("vm1".into()),
            instance_type: Some("t3.micro".into()),
            regions: regions.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        crate::planner::plan(&spec, &Defaults::default(), "ssh-ed25519 AAAA t").unwrap()
    }

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_fails_over_to_next_region_with_teardown() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::CapacityOnRun),
            ("us-east-1", Behavior::Success),
        ]);
        let plan = make_plan(&["us-west-2", "us-east-1"]);
        let db = test_db();

        let outcome = provision_with_failover(&provider, &plan, &db, "{}")
            .await
            .unwrap();
        assert_eq!(outcome.record.region, "us-east-1");
        let calls = provider.calls();
        assert!(calls.contains(&"terminate:us-west-2".to_string()));
        // DB reflects the winning region, still PROVISIONING (orchestrator marks UP).
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.region.as_deref(), Some("us-east-1"));
    }

    #[tokio::test(start_paused = true)]
    async fn credentials_error_aborts_immediately() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::CredentialsOnRun),
            ("us-east-1", Behavior::Success),
        ]);
        let plan = make_plan(&["us-west-2", "us-east-1"]);
        let db = test_db();

        let err = provision_with_failover(&provider, &plan, &db, "{}")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            FailoverError::Fatal {
                error: ProvisionError::Credentials { .. },
                ..
            }
        ));
        let calls = provider.calls();
        // Never moved on to the second region, never tore down.
        assert!(!calls.iter().any(|c| c.ends_with("us-east-1")));
        assert!(!calls.iter().any(|c| c.starts_with("terminate")));
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Failed);
    }

    #[tokio::test(start_paused = true)]
    async fn config_error_skips_teardown_and_continues() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::ConfigOnBootstrap),
            ("us-east-1", Behavior::Success),
        ]);
        let plan = make_plan(&["us-west-2", "us-east-1"]);
        let db = test_db();

        let outcome = provision_with_failover(&provider, &plan, &db, "{}")
            .await
            .unwrap();
        assert_eq!(outcome.record.region, "us-east-1");
        assert!(!provider.calls().iter().any(|c| c.starts_with("terminate")));
    }

    #[tokio::test(start_paused = true)]
    async fn teardown_failure_is_fatal() {
        let mut provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::CapacityOnRun),
            ("us-east-1", Behavior::Success),
        ]);
        provider.fail_terminate = true;
        let plan = make_plan(&["us-west-2", "us-east-1"]);
        let db = test_db();

        let err = provision_with_failover(&provider, &plan, &db, "{}")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            FailoverError::Fatal {
                error: ProvisionError::TeardownFailed { .. },
                ..
            }
        ));
        // Retried teardown 3 times, never reached the second region.
        let terminates = provider
            .calls()
            .iter()
            .filter(|c| *c == "terminate:us-west-2")
            .count();
        assert_eq!(terminates, TEARDOWN_MAX_ATTEMPTS);
        assert!(!provider.calls().iter().any(|c| c.ends_with("us-east-1")));
    }

    #[tokio::test(start_paused = true)]
    async fn exhaustion_reports_full_history_and_marks_failed() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::CapacityOnRun),
            ("us-east-1", Behavior::CapacityOnRun),
        ]);
        let plan = make_plan(&["us-west-2", "us-east-1"]);
        let db = test_db();

        let err = provision_with_failover(&provider, &plan, &db, "{}")
            .await
            .unwrap_err();
        match &err {
            FailoverError::Exhausted { history } => {
                assert_eq!(history.len(), 2);
                let msg = err.to_string();
                assert!(msg.contains("us-west-2"));
                assert!(msg.contains("us-east-1"));
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Failed);
    }
}
