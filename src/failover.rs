//! The failover engine ("Phase 4") — a port of SkyPilot's
//! `RetryingVmProvisioner.provision_with_retries` collapsed to one cloud:
//! walk the provider-planned candidates (`{instance_type, region, zone}`,
//! already in preference order) and per candidate run bootstrap →
//! run_instance → wait; on failure classify the error, tear down partial
//! resources when needed, block the candidate's (type, region), and continue.
//! Fatal errors (credentials, failed teardown, unplannable request) abort.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use crate::db::{Db, VmState};
use crate::planner::{build_request, LaunchSpec};
use crate::provider::{
    Candidate, CloudProvider, ProvisionError, ProvisionRecord, ProvisionRequest,
};

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
    pub candidate: Candidate,
    pub error: ProvisionError,
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
                write!(f, "provisioning failed for all candidates:")?;
                for attempt in history {
                    write!(f, "\n  - {}: {}", attempt.candidate, attempt.error)?;
                }
                Ok(())
            }
            FailoverError::Fatal { error, history } => {
                write!(f, "{error}")?;
                if !history.is_empty() {
                    write!(f, "\nearlier failover attempts:")?;
                    for attempt in history {
                        write!(f, "\n  - {}: {}", attempt.candidate, attempt.error)?;
                    }
                }
                Ok(())
            }
            FailoverError::Db(e) => write!(f, "state db error: {e}"),
        }
    }
}

impl std::error::Error for FailoverError {}

/// Try each candidate in order until one yields a running instance.
/// On success the DB row is left in `PROVISIONING` (the orchestrator marks
/// `UP` after SSH is confirmed); on failure it is marked `FAILED`.
pub async fn provision_with_failover(
    provider: &dyn CloudProvider,
    launch: &LaunchSpec,
    candidates: &[Candidate],
    db: &Db,
    spec_json: &str,
) -> Result<ProvisionRecord, FailoverError> {
    let mut history: Vec<AttemptFailure> = Vec::new();
    // Blocklist keyed on (instance_type, region) — capacity for one type must
    // not block other types in the same region (SkyPilot's blocked Resources
    // carry the instance type too).
    let mut blocked: HashSet<(String, String)> = HashSet::new();

    for candidate in candidates {
        let block_key = (candidate.instance_type.clone(), candidate.region.clone());
        if blocked.contains(&block_key) {
            continue;
        }
        tracing::info!(
            "[{}] attempting to provision '{}' as {}",
            candidate.region,
            launch.vm_name,
            candidate
        );
        // ≈ SkyPilot recording the cluster as INIT before provisioning.
        db.upsert_provisioning(
            &launch.vm_name,
            provider.name(),
            &candidate.region,
            &candidate.instance_type,
            spec_json,
        )
        .map_err(FailoverError::Db)?;
        let request = build_request(launch, candidate);

        match attempt(provider, &request).await {
            Ok(record) => return Ok(record),
            Err(error) if error.is_fatal() => {
                db.set_state(&launch.vm_name, VmState::Failed)
                    .map_err(FailoverError::Db)?;
                return Err(FailoverError::Fatal { error, history });
            }
            Err(error) => {
                tracing::warn!("[{candidate}] attempt failed: {error}");
                if error.needs_teardown() {
                    if let Err(teardown_error) =
                        teardown(provider, &candidate.region, &launch.vm_name).await
                    {
                        history.push(AttemptFailure {
                            candidate: candidate.clone(),
                            error,
                        });
                        db.set_state(&launch.vm_name, VmState::Failed)
                            .map_err(FailoverError::Db)?;
                        return Err(FailoverError::Fatal {
                            error: teardown_error,
                            history,
                        });
                    }
                }
                blocked.insert(block_key);
                history.push(AttemptFailure {
                    candidate: candidate.clone(),
                    error,
                });
            }
        }
    }

    db.set_state(&launch.vm_name, VmState::Failed)
        .map_err(FailoverError::Db)?;
    Err(FailoverError::Exhausted { history })
}

/// One candidate attempt: bootstrap → run_instance → wait-until-running
/// (with a small retry around the wait, mirroring SkyPilot's `_bulk_provision`).
async fn attempt(
    provider: &dyn CloudProvider,
    request: &ProvisionRequest,
) -> Result<ProvisionRecord, ProvisionError> {
    let record = provider.provision(request).await?;

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
    use crate::provider::{ResourceAsk, ResourceRequest, VmInfo};

    /// Scripted `provision` outcome for the fake provider, keyed by `region`
    /// or by `region:instance_type` (the more specific key wins).
    #[derive(Clone, Copy, PartialEq)]
    enum Behavior {
        Success,
        Capacity,
        Config,
        Credentials,
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

        fn behavior(&self, region: &str, instance_type: &str) -> Behavior {
            let specific = format!("{region}:{instance_type}");
            self.behaviors
                .iter()
                .find(|(key, _)| *key == specific)
                .or_else(|| self.behaviors.iter().find(|(key, _)| *key == region))
                .map(|(_, behavior)| *behavior)
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
        fn name(&self) -> &'static str {
            "fake"
        }

        async fn plan_candidates(
            &self,
            request: &ResourceRequest,
        ) -> Result<Vec<Candidate>, ProvisionError> {
            // Trivial expansion for tests: one candidate per region.
            let instance_type = match &request.ask {
                ResourceAsk::InstanceType(it) => it.clone(),
                ResourceAsk::Gpus(g) => format!("fake-{}", g.name),
            };
            Ok(request
                .regions
                .iter()
                .map(|region| Candidate {
                    instance_type: instance_type.clone(),
                    region: region.clone(),
                    zone: request.zone.clone(),
                })
                .collect())
        }

        async fn provision(
            &self,
            req: &ProvisionRequest,
        ) -> Result<ProvisionRecord, ProvisionError> {
            self.log(format!("provision:{}:{}", req.region, req.instance_type));
            match self.behavior(&req.region, &req.instance_type) {
                Behavior::Capacity => Err(ProvisionError::Capacity {
                    region: req.region.clone(),
                    zone: Some(format!("{}a", req.region)),
                    msg: "InsufficientInstanceCapacity".into(),
                }),
                Behavior::Config => Err(ProvisionError::Config {
                    region: req.region.clone(),
                    msg: "no default VPC".into(),
                }),
                Behavior::Credentials => Err(ProvisionError::Credentials {
                    msg: "ExpiredToken".into(),
                }),
                Behavior::Success => Ok(ProvisionRecord {
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

    fn make_launch() -> LaunchSpec {
        LaunchSpec {
            vm_name: "vm1".into(),
            image_id: None,
            disk_gb: 100,
            spot: false,
            extra_ports: Vec::new(),
            ssh_user: "ubuntu".into(),
            user_data: "#cloud-config".into(),
        }
    }

    fn candidate(instance_type: &str, region: &str) -> Candidate {
        Candidate {
            instance_type: instance_type.into(),
            region: region.into(),
            zone: None,
        }
    }

    fn test_db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_fails_over_to_next_region_with_teardown() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::Capacity),
            ("us-east-1", Behavior::Success),
        ]);
        let candidates = vec![
            candidate("t3.micro", "us-west-2"),
            candidate("t3.micro", "us-east-1"),
        ];
        let db = test_db();

        let record = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
            .await
            .unwrap();
        assert_eq!(record.region, "us-east-1");
        let calls = provider.calls();
        assert!(calls.contains(&"terminate:us-west-2".to_string()));
        // DB reflects the winning candidate, still PROVISIONING (orchestrator marks UP).
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.region.as_deref(), Some("us-east-1"));
        // The provisioning provider's id is recorded on the row.
        assert_eq!(rec.provider.as_deref(), Some("fake"));
    }

    /// One GPU ask expands to a type ladder; capacity for the small type must
    /// fail over to the next type in the SAME region (blocklist is keyed on
    /// (type, region), not region alone).
    #[tokio::test(start_paused = true)]
    async fn capacity_fails_over_to_next_instance_type_in_same_region() {
        let provider = FakeProvider::new(vec![
            ("us-east-2:g6e.xlarge", Behavior::Capacity),
            ("us-east-2:g6e.2xlarge", Behavior::Success),
        ]);
        let candidates = vec![
            candidate("g6e.xlarge", "us-east-2"),
            candidate("g6e.2xlarge", "us-east-2"),
        ];
        let db = test_db();

        let record = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
            .await
            .unwrap();
        assert_eq!(record.region, "us-east-2");
        // The winning instance type is recorded on the row.
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.instance_type.as_deref(), Some("g6e.2xlarge"));
        // Both types were actually attempted.
        let calls = provider.calls();
        assert!(calls.contains(&"provision:us-east-2:g6e.xlarge".to_string()));
        assert!(calls.contains(&"provision:us-east-2:g6e.2xlarge".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn credentials_error_aborts_immediately() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::Credentials),
            ("us-east-1", Behavior::Success),
        ]);
        let candidates = vec![
            candidate("t3.micro", "us-west-2"),
            candidate("t3.micro", "us-east-1"),
        ];
        let db = test_db();

        let err = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
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
        // Never moved on to the second candidate, never tore down.
        assert!(!calls.iter().any(|c| c.contains("us-east-1")));
        assert!(!calls.iter().any(|c| c.starts_with("terminate")));
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Failed);
    }

    #[tokio::test(start_paused = true)]
    async fn config_error_skips_teardown_and_continues() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::Config),
            ("us-east-1", Behavior::Success),
        ]);
        let candidates = vec![
            candidate("t3.micro", "us-west-2"),
            candidate("t3.micro", "us-east-1"),
        ];
        let db = test_db();

        let record = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
            .await
            .unwrap();
        assert_eq!(record.region, "us-east-1");
        assert!(!provider.calls().iter().any(|c| c.starts_with("terminate")));
    }

    #[tokio::test(start_paused = true)]
    async fn teardown_failure_is_fatal() {
        let mut provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::Capacity),
            ("us-east-1", Behavior::Success),
        ]);
        provider.fail_terminate = true;
        let candidates = vec![
            candidate("t3.micro", "us-west-2"),
            candidate("t3.micro", "us-east-1"),
        ];
        let db = test_db();

        let err = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            FailoverError::Fatal {
                error: ProvisionError::TeardownFailed { .. },
                ..
            }
        ));
        // Retried teardown 3 times, never reached the second candidate.
        let terminates = provider
            .calls()
            .iter()
            .filter(|c| *c == "terminate:us-west-2")
            .count();
        assert_eq!(terminates, TEARDOWN_MAX_ATTEMPTS);
        assert!(!provider.calls().iter().any(|c| c.contains("us-east-1")));
    }

    #[tokio::test(start_paused = true)]
    async fn exhaustion_reports_full_history_and_marks_failed() {
        let provider = FakeProvider::new(vec![
            ("us-west-2", Behavior::Capacity),
            ("us-east-1", Behavior::Capacity),
        ]);
        let candidates = vec![
            candidate("t3.micro", "us-west-2"),
            candidate("t3.micro", "us-east-1"),
        ];
        let db = test_db();

        let err = provision_with_failover(&provider, &make_launch(), &candidates, &db, "{}")
            .await
            .unwrap_err();
        match &err {
            FailoverError::Exhausted { history } => {
                assert_eq!(history.len(), 2);
                let msg = err.to_string();
                assert!(msg.contains("t3.micro@us-west-2"));
                assert!(msg.contains("t3.micro@us-east-1"));
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Failed);
    }
}
