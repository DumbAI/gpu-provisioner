//! The VM launch state machine — failover across offers, instance creation,
//! boot wait and SSH verification, as one durable machine:
//!
//! ```text
//! Reconciling → Planning → [Preparing → Creating → Booting → Connecting]* → Ready
//!                              │           │          │          │
//!                              └───────────┴──────────┴──────────┴─→ Compensating → next offer | Aborted | Leaked
//! ```
//!
//! Failover state (`Search`: offers, cursor, blocklist, history) rides inside
//! every attempt state, so the search survives a crash. Every step observes
//! before acting (`Creating` adopts an instance created by an interrupted run
//! instead of creating a second one). See `doc/STATE_MACHINE_DESIGN.md` §4.3.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::db::{Db, UpFields, VmRecord, VmState};
use crate::machine::{Machine, RetryPolicy, Step, StepCx};
use crate::provider::{
    BlockScope, CloudProvider, ErrorKind, Feature, InstanceSpec, InstanceStatus, Location,
    Observed, Offer, ProviderError, ResourceRequest, SshEndpoint,
};
use crate::ssh::config_file;
use crate::ssh::options::SshOptions;
use crate::ssh::wait;
use crate::util::Paths;

use super::teardown::{step_teardown, Teardown, TeardownStep};

/// Gap between full failover rounds with `--retry-until-up`.
pub const RETRY_UNTIL_UP_GAP: Duration = Duration::from_secs(30);
/// How long a just-created instance may be invisible to `observe` (eventual
/// consistency of list APIs) before it is considered vanished.
const VISIBILITY_GRACE: Duration = Duration::from_secs(60);
/// SSH reachability poll interval while `Connecting`.
const SSH_POLL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One failed offer, for the user-facing failure report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub offer: String,
    pub error: ProviderError,
}

/// A blocklist entry: offers matching `offer` under `scope` are skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub scope: BlockScope,
    pub offer: Offer,
}

impl Block {
    fn blocks(&self, other: &Offer) -> bool {
        match self.scope {
            BlockScope::Offer => *other == self.offer,
            BlockScope::TypeInRegion => {
                other.instance_type == self.offer.instance_type && other.region == self.offer.region
            }
            BlockScope::Region => other.region == self.offer.region,
        }
    }
}

/// The failover search: ordered offers, the current position, what has been
/// ruled out, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Search {
    pub offers: Vec<Offer>,
    pub cursor: usize,
    pub blocked: Vec<Block>,
    pub history: Vec<Failure>,
}

impl Search {
    /// Start at the first offer (`None` if there are no offers).
    pub fn start(offers: Vec<Offer>) -> Option<Search> {
        Search {
            offers,
            cursor: 0,
            blocked: Vec::new(),
            history: Vec::new(),
        }
        .seek(0)
    }

    pub fn offer(&self) -> &Offer {
        &self.offers[self.cursor]
    }

    /// Record the current offer's failure, block per the error's scope, and
    /// move to the next unblocked offer. `Err(history)` when exhausted.
    pub fn fail_and_advance(mut self, error: ProviderError) -> Result<Search, Vec<Failure>> {
        let offer = self.offer().clone();
        self.blocked.push(Block {
            scope: error.block_scope(),
            offer: offer.clone(),
        });
        self.history.push(Failure {
            offer: offer.to_string(),
            error,
        });
        let history = self.history.clone();
        let from = self.cursor + 1;
        self.seek(from).ok_or(history)
    }

    fn seek(mut self, from: usize) -> Option<Search> {
        let next = (from..self.offers.len())
            .find(|&i| !self.blocked.iter().any(|b| b.blocks(&self.offers[i])))?;
        self.cursor = next;
        Some(self)
    }
}

/// A launched, SSH-verified VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadyVm {
    pub name: String,
    pub provider: String,
    pub offer: Offer,
    /// Concrete zone the instance runs in (when the provider reports one).
    pub zone: Option<String>,
    pub instance_id: String,
    /// Resolved endpoint: `user` is always set here.
    pub endpoint: SshEndpoint,
    pub private_ip: Option<String>,
    pub ssh_key_path: PathBuf,
    pub ssh_config_path: Option<PathBuf>,
    /// An already-running instance was adopted rather than created.
    pub reused: bool,
}

impl ReadyVm {
    pub fn user(&self) -> &str {
        self.endpoint.user.as_deref().unwrap_or("ubuntu")
    }

    pub fn ssh_options(&self) -> SshOptions {
        SshOptions::new(self.ssh_key_path.clone()).port(self.endpoint.port)
    }

    pub fn location(&self) -> Location {
        Location {
            region: self.offer.region.clone(),
            zone: self.zone.clone().or_else(|| self.offer.zone.clone()),
        }
    }

    /// Rebuild from an `UP` row (for commands acting on an existing VM).
    pub fn from_record(record: &VmRecord) -> Option<ReadyVm> {
        if record.state != VmState::Up {
            return None;
        }
        let region = record.region.clone()?;
        Some(ReadyVm {
            name: record.name.clone(),
            provider: record.provider.clone()?,
            offer: Offer::new(record.instance_type.clone().unwrap_or_default(), region),
            zone: record.zone.clone().filter(|z| !z.is_empty()),
            instance_id: record.instance_id.clone()?,
            endpoint: SshEndpoint {
                host: record.public_ip.clone()?,
                port: record.ssh_port.unwrap_or(22),
                user: record.ssh_user.clone(),
            },
            private_ip: record.private_ip.clone(),
            ssh_key_path: PathBuf::from(record.ssh_key_path.clone()?),
            ssh_config_path: record.ssh_config_path.clone().map(PathBuf::from),
            reused: true,
        })
    }
}

/// What to do once a compensation teardown completes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AfterTeardown {
    /// Block the failed offer and continue the search.
    NextOffer,
    /// Stop with this reason (fatal error, cancellation).
    Abort(String),
    /// Leftovers from an earlier run are gone: plan from scratch.
    Replan,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LaunchState {
    /// Compare the local record against the cloud before doing anything.
    Reconciling,
    Planning,
    Preparing {
        search: Search,
    },
    Creating {
        search: Search,
        prepared: serde_json::Value,
    },
    Booting {
        search: Search,
        instance_id: String,
    },
    Connecting {
        search: Option<Search>,
        vm: ReadyVm,
    },
    Compensating {
        search: Option<Search>,
        teardown: Teardown,
        failure: Option<ProviderError>,
        then: AfterTeardown,
    },
    /// `--retry-until-up`: wait, then re-plan.
    WaitingToRetry {
        history: Vec<Failure>,
    },
    Ready(ReadyVm),
    Exhausted {
        history: Vec<Failure>,
    },
    Aborted {
        reason: String,
        history: Vec<Failure>,
    },
    Leaked {
        location: Location,
        reason: String,
    },
}

impl LaunchState {
    /// One human-readable explanation for a terminal failure state (`None`
    /// for `Ready` and in-flight states).
    pub fn failure_summary(&self) -> Option<String> {
        match self {
            LaunchState::Exhausted { history } => Some(format!(
                "provisioning failed for every offer:{}",
                History(history)
            )),
            LaunchState::Aborted { reason, history } if history.is_empty() => Some(reason.clone()),
            LaunchState::Aborted { reason, history } => {
                Some(format!("{reason}\nearlier attempts:{}", History(history)))
            }
            LaunchState::Leaked { location, reason } => Some(format!(
                "could not confirm teardown at {location} ({reason}); resources may still be \
                 running — check the cloud console, then run `gpuprov down <name>`"
            )),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LaunchState::Ready(_)
                | LaunchState::Exhausted { .. }
                | LaunchState::Aborted { .. }
                | LaunchState::Leaked { .. }
        )
    }
}

/// Human-readable failover report.
pub struct History<'a>(pub &'a [Failure]);

impl fmt::Display for History<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for failure in self.0 {
            write!(f, "\n  - {}: {}", failure.offer, failure.error)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SSH reachability (injectable for tests)
// ---------------------------------------------------------------------------

/// Checks that a VM accepts our SSH login.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn probe(
        &self,
        endpoint: &SshEndpoint,
        user: &str,
        key: &std::path::Path,
    ) -> Result<(), String>;
}

/// The real thing: TCP banner, then `ssh … true` in batch mode.
pub struct SshConnector;

#[async_trait]
impl Connector for SshConnector {
    async fn probe(
        &self,
        endpoint: &SshEndpoint,
        user: &str,
        key: &std::path::Path,
    ) -> Result<(), String> {
        let opts = SshOptions::new(key.to_path_buf()).port(endpoint.port);
        wait::probe(&endpoint.host, user, &opts).await
    }
}

// ---------------------------------------------------------------------------
// Machine
// ---------------------------------------------------------------------------

/// Everything a launch needs that does not change while it runs.
pub struct LaunchMachine<'a> {
    pub provider: &'a dyn CloudProvider,
    pub db: &'a Db,
    pub paths: &'a Paths,
    pub connector: &'a dyn Connector,
    pub spec: InstanceSpec,
    pub request: ResourceRequest,
    pub required_features: Vec<Feature>,
    /// Stored on the `vms` row (re-read by `kubeconfig`).
    pub spec_json: String,
    pub ssh_key_path: PathBuf,
    pub retry_until_up: bool,
    pub ssh_timeout: Duration,
}

#[async_trait]
impl Machine for LaunchMachine<'_> {
    type State = LaunchState;
    const KIND: &'static str = "vm";

    fn is_terminal(&self, s: &LaunchState) -> bool {
        s.is_terminal()
    }

    fn label(&self, s: &LaunchState) -> String {
        match s {
            LaunchState::Reconciling => "reconciling local record with the cloud".into(),
            LaunchState::Planning => format!("planning on {}", self.provider.name()),
            LaunchState::Preparing { search } => format!("preparing {}", search.offer()),
            LaunchState::Creating { search, .. } => format!("creating {}", search.offer()),
            LaunchState::Booting {
                search,
                instance_id,
            } => format!("booting {instance_id} ({})", search.offer()),
            LaunchState::Connecting { vm, .. } => format!(
                "waiting for SSH {}@{}:{}",
                vm.user(),
                vm.endpoint.host,
                vm.endpoint.port
            ),
            LaunchState::Compensating { teardown, then, .. } => format!(
                "tearing down at {} ({:?}) then {}",
                teardown.location,
                teardown.phase,
                match then {
                    AfterTeardown::NextOffer => "next offer",
                    AfterTeardown::Abort(_) => "abort",
                    AfterTeardown::Replan => "re-plan",
                }
            ),
            LaunchState::WaitingToRetry { .. } => format!(
                "all offers failed; retrying in {}s (--retry-until-up)",
                RETRY_UNTIL_UP_GAP.as_secs()
            ),
            LaunchState::Ready(vm) => format!(
                "UP: {} {} ({})",
                vm.instance_id,
                vm.endpoint.host,
                vm.location()
            ),
            LaunchState::Exhausted { .. } => "FAILED: no offer could be provisioned".into(),
            LaunchState::Aborted { reason, .. } => format!("FAILED: {reason}"),
            LaunchState::Leaked { location, .. } => format!("LEAKED at {location}"),
        }
    }

    fn detail(&self, s: &LaunchState) -> Option<String> {
        match s {
            LaunchState::Compensating {
                failure: Some(e), ..
            } => Some(e.to_string()),
            LaunchState::Exhausted { history } | LaunchState::WaitingToRetry { history } => {
                Some(History(history).to_string())
            }
            LaunchState::Aborted { reason, history } => {
                Some(format!("{reason}{}", History(history)))
            }
            LaunchState::Leaked { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }

    async fn step(&self, s: &LaunchState, cx: &StepCx) -> Step<LaunchState> {
        match s {
            LaunchState::Reconciling => self.reconcile().await,
            LaunchState::Planning => self.plan().await,
            LaunchState::Preparing { search } => self.prepare(search).await,
            LaunchState::Creating { search, prepared } => self.create(search, prepared).await,
            LaunchState::Booting {
                search,
                instance_id,
            } => self.boot(search, instance_id, cx).await,
            LaunchState::Connecting { search, vm } => self.connect(search, vm, cx).await,
            LaunchState::Compensating {
                search,
                teardown,
                failure,
                then,
            } => self.compensate(search, teardown, failure, then, cx).await,
            LaunchState::WaitingToRetry { .. } => {
                if cx.in_state >= RETRY_UNTIL_UP_GAP {
                    Step::Next(LaunchState::Planning)
                } else {
                    Step::Poll {
                        after: RETRY_UNTIL_UP_GAP - cx.in_state,
                        note: "retry-until-up gap".into(),
                    }
                }
            }
            terminal => unreachable!("terminal state {terminal:?} is never stepped"),
        }
    }

    fn retry_policy(&self, s: &LaunchState) -> RetryPolicy {
        match s {
            // Teardown gets more patience than forward progress: giving up
            // there means a leak.
            LaunchState::Compensating { .. } => RetryPolicy {
                max_attempts: 8,
                ..RetryPolicy::default()
            },
            _ => RetryPolicy::default(),
        }
    }

    fn on_retries_exhausted(&self, s: &LaunchState, last_error: String) -> LaunchState {
        let error = ProviderError::failed(last_error.clone());
        match s {
            LaunchState::Reconciling | LaunchState::Planning => self.abort(last_error, Vec::new()),
            LaunchState::Preparing { search }
            | LaunchState::Creating { search, .. }
            | LaunchState::Booting { search, .. } => self.compensation(search.clone(), error),
            LaunchState::Connecting { search, vm } => match search {
                Some(search) if !vm.reused => self.compensation(search.clone(), error),
                _ => self.abort(last_error, Vec::new()),
            },
            LaunchState::Compensating { teardown, .. } => self.leaked(teardown, last_error),
            other => other.clone(),
        }
    }

    /// Ctrl-C mid-attempt tears down what this run created; Ctrl-C while
    /// planning (nothing created yet) just stops.
    fn on_cancel(&self, s: &LaunchState) -> Option<LaunchState> {
        let abort = |search: &Search| LaunchState::Compensating {
            teardown: Teardown::new(search.offer().location()),
            search: Some(search.clone()),
            failure: None,
            then: AfterTeardown::Abort("cancelled by user".into()),
        };
        match s {
            LaunchState::Preparing { search }
            | LaunchState::Creating { search, .. }
            | LaunchState::Booting { search, .. } => Some(abort(search)),
            LaunchState::Connecting {
                search: Some(search),
                vm,
            } if !vm.reused => Some(LaunchState::Compensating {
                teardown: Teardown::new(vm.location()),
                search: Some(search.clone()),
                failure: None,
                then: AfterTeardown::Abort("cancelled by user".into()),
            }),
            LaunchState::Compensating { .. } => Some(s.clone()),
            _ => None,
        }
    }
}

impl LaunchMachine<'_> {
    fn vm_name(&self) -> &str {
        &self.spec.vm_name
    }

    // --- Reconciling ---------------------------------------------------------

    async fn reconcile(&self) -> Step<LaunchState> {
        // Reject an unsupported ask before anything else — including before
        // reusing a running VM that could not honor it.
        if let Some(rejected) = self.check_features() {
            return Step::Next(rejected);
        }
        let record = match self.db.get(self.vm_name()) {
            Ok(r) => r,
            Err(e) => {
                return Step::Retry {
                    error: e.to_string(),
                }
            }
        };
        let Some(record) = record else {
            return Step::Next(LaunchState::Planning);
        };
        let owned = matches!(
            record.state,
            VmState::Up | VmState::Provisioning | VmState::Leaked
        );
        let provider = record.provider.as_deref().unwrap_or("aws");
        if owned && provider != self.provider.name() {
            return Step::Next(self.reject(format!(
                "VM '{}' is {} on {provider}; run `gpuprov down {}` before provisioning it on {}",
                self.vm_name(),
                record.state,
                self.vm_name(),
                self.provider.name()
            )));
        }
        let (Some(region), true) = (record.region.clone(), owned) else {
            return Step::Next(LaunchState::Planning);
        };
        let location = Location {
            region,
            zone: record.zone.clone().filter(|z| !z.is_empty()),
        };
        let observed = match self.provider.observe(&location, self.vm_name()).await {
            Ok(o) => o,
            Err(e) if e.kind == ErrorKind::Transient => return Step::Retry { error: e.msg },
            Err(e) if e.kind.is_fatal() => {
                return Step::Next(self.abort(e.to_string(), Vec::new()))
            }
            Err(e) => return Step::Retry { error: e.msg },
        };

        // LEAKED: clean up before anything else.
        if record.state == VmState::Leaked {
            tracing::warn!(
                "'{}' has leftover resources at {location}; cleaning up first",
                self.vm_name()
            );
            return Step::Next(LaunchState::Compensating {
                search: None,
                teardown: Teardown::new(location),
                failure: None,
                then: AfterTeardown::Replan,
            });
        }

        let ready: Vec<&Observed> = observed.iter().filter(|o| o.is_ready()).collect();
        if let [only] = ready.as_slice() {
            tracing::info!(
                "'{}' already has running instance {} at {location}; verifying it",
                self.vm_name(),
                only.instance_id
            );
            let offer = Offer::new(
                record.instance_type.clone().unwrap_or_default(),
                location.region.clone(),
            )
            .with_zone(location.zone.clone());
            return Step::Next(LaunchState::Connecting {
                search: None,
                vm: self.ready_vm(&offer, only, true),
            });
        }
        if observed.is_empty() {
            if record.state == VmState::Up {
                tracing::warn!(
                    "record says '{}' is UP but nothing runs at {location}; re-provisioning",
                    self.vm_name()
                );
            }
            return Step::Next(LaunchState::Planning);
        }
        // Stale PROVISIONING (its process died — we hold the lease) or a
        // confusing multi-instance state: clean slate.
        tracing::warn!(
            "'{}' has {} instance(s) in an unknown state at {location}; tearing down before \
             re-planning",
            self.vm_name(),
            observed.len()
        );
        Step::Next(LaunchState::Compensating {
            search: None,
            teardown: Teardown::new(location),
            failure: None,
            then: AfterTeardown::Replan,
        })
    }

    // --- Planning --------------------------------------------------------------

    async fn plan(&self) -> Step<LaunchState> {
        if let Some(rejected) = self.check_features() {
            return Step::Next(rejected);
        }
        match self.provider.plan(&self.request).await {
            Ok(offers) => {
                let count = offers.len();
                let preview: Vec<String> = offers.iter().take(5).map(|o| o.to_string()).collect();
                tracing::info!(
                    "{count} offer(s): {}{}",
                    preview.join(", "),
                    if count > preview.len() {
                        format!(", … +{} more", count - preview.len())
                    } else {
                        String::new()
                    }
                );
                match Search::start(offers) {
                    Some(search) => Step::Next(LaunchState::Preparing { search }),
                    None => Step::Next(self.exhausted(Vec::new())),
                }
            }
            Err(e) if e.kind == ErrorKind::Transient => Step::Retry { error: e.msg },
            Err(e) => Step::Next(self.abort(e.to_string(), Vec::new())),
        }
    }

    // --- Attempt phases ----------------------------------------------------------

    async fn prepare(&self, search: &Search) -> Step<LaunchState> {
        let offer = search.offer();
        // Record the attempt before creating anything, so the row points at
        // where resources may appear.
        if let Err(e) = self.db.upsert_provisioning(
            self.vm_name(),
            self.provider.name(),
            &offer.region,
            offer.zone.as_deref(),
            &offer.instance_type,
            &self.spec_json,
        ) {
            return Step::Retry {
                error: e.to_string(),
            };
        }
        match self.provider.prepare(offer, &self.spec).await {
            Ok(prepared) => Step::Next(LaunchState::Creating {
                search: search.clone(),
                prepared,
            }),
            Err(e) => self.attempt_failed(search, e),
        }
    }

    async fn create(&self, search: &Search, prepared: &serde_json::Value) -> Step<LaunchState> {
        let offer = search.offer();
        // Observe first: an interrupted run may already have created it.
        let existing = match self
            .provider
            .observe(&offer.location(), self.vm_name())
            .await
        {
            Ok(o) => o,
            Err(e) => return self.attempt_failed(search, e),
        };
        match existing.as_slice() {
            [] => {}
            [one] => {
                tracing::info!("adopting existing instance {}", one.instance_id);
                return Step::Next(LaunchState::Booting {
                    search: search.clone(),
                    instance_id: one.instance_id.clone(),
                });
            }
            many => {
                return self.attempt_failed(
                    search,
                    ProviderError::failed(format!(
                        "{} live instances carry this VM's identity; refusing to guess",
                        many.len()
                    )),
                )
            }
        }
        match self.provider.create(offer, prepared, &self.spec).await {
            Ok(instance_id) => Step::Next(LaunchState::Booting {
                search: search.clone(),
                instance_id,
            }),
            Err(e) => self.attempt_failed(search, e),
        }
    }

    async fn boot(&self, search: &Search, instance_id: &str, cx: &StepCx) -> Step<LaunchState> {
        let offer = search.offer();
        let caps = self.provider.capabilities();
        let observed = match self
            .provider
            .observe(&offer.location(), self.vm_name())
            .await
        {
            Ok(o) => o,
            Err(e) => return self.attempt_failed(search, e),
        };
        let Some(instance) = observed.iter().find(|o| o.instance_id == instance_id) else {
            if cx.in_state < VISIBILITY_GRACE {
                return Step::Poll {
                    after: caps.poll_interval,
                    note: format!("{instance_id} not visible yet"),
                };
            }
            return self.attempt_failed(
                search,
                ProviderError::failed(format!("instance {instance_id} disappeared while booting")),
            );
        };
        if instance.is_ready() {
            return Step::Next(LaunchState::Connecting {
                search: Some(search.clone()),
                vm: self.ready_vm(offer, instance, false),
            });
        }
        if let InstanceStatus::Failed(reason) = &instance.status {
            return self.attempt_failed(
                search,
                ProviderError::failed(format!("instance {instance_id} failed: {reason}")),
            );
        }
        if matches!(
            instance.status,
            InstanceStatus::Stopping | InstanceStatus::Stopped | InstanceStatus::Terminating
        ) {
            return self.attempt_failed(
                search,
                ProviderError::capacity(format!(
                    "instance {instance_id} went {:?} while booting (preempted?)",
                    instance.status
                )),
            );
        }
        if cx.in_state >= caps.boot_timeout {
            // A launch that never becomes ready is treated as no capacity here.
            return self.attempt_failed(
                search,
                ProviderError::capacity(format!(
                    "instance {instance_id} not ready after {}s",
                    caps.boot_timeout.as_secs()
                )),
            );
        }
        Step::Poll {
            after: caps.poll_interval,
            note: format!("{instance_id} is {:?}", instance.status),
        }
    }

    async fn connect(
        &self,
        search: &Option<Search>,
        vm: &ReadyVm,
        cx: &StepCx,
    ) -> Step<LaunchState> {
        match self
            .connector
            .probe(&vm.endpoint, vm.user(), &vm.ssh_key_path)
            .await
        {
            Ok(()) => match self.persist_up(vm) {
                Ok(vm) => Step::Next(LaunchState::Ready(vm)),
                Err(e) => Step::Retry { error: e },
            },
            Err(last) if cx.in_state >= self.ssh_timeout => {
                let msg = format!(
                    "SSH to {}@{}:{} not ready after {}s (last: {last})",
                    vm.user(),
                    vm.endpoint.host,
                    vm.endpoint.port,
                    self.ssh_timeout.as_secs()
                );
                match search {
                    // Never tear down a VM we adopted: report instead.
                    Some(search) if !vm.reused => {
                        Step::Next(self.compensation(search.clone(), ProviderError::failed(msg)))
                    }
                    _ => Step::Next(self.abort(msg, Vec::new())),
                }
            }
            Err(last) => Step::Poll {
                after: SSH_POLL,
                note: last,
            },
        }
    }

    // --- Compensation --------------------------------------------------------------

    async fn compensate(
        &self,
        search: &Option<Search>,
        teardown: &Teardown,
        failure: &Option<ProviderError>,
        then: &AfterTeardown,
        cx: &StepCx,
    ) -> Step<LaunchState> {
        let next_teardown = |td: Teardown| LaunchState::Compensating {
            search: search.clone(),
            teardown: td,
            failure: failure.clone(),
            then: then.clone(),
        };
        match step_teardown(self.provider, self.vm_name(), teardown, cx).await {
            TeardownStep::Next(td) => Step::Next(next_teardown(td)),
            TeardownStep::Poll(note) => Step::Poll {
                after: self.provider.capabilities().poll_interval,
                note,
            },
            TeardownStep::Retry(error) => Step::Retry { error },
            TeardownStep::TimedOut(reason) => Step::Next(self.leaked(teardown, reason)),
            TeardownStep::Done => match then {
                AfterTeardown::Replan => Step::Next(LaunchState::Planning),
                AfterTeardown::Abort(reason) => Step::Next(
                    self.abort(
                        reason.clone(),
                        search
                            .as_ref()
                            .map(|s| s.history.clone())
                            .unwrap_or_default(),
                    ),
                ),
                AfterTeardown::NextOffer => {
                    let (Some(search), Some(error)) = (search.clone(), failure.clone()) else {
                        return Step::Next(LaunchState::Planning);
                    };
                    match search.fail_and_advance(error) {
                        Ok(search) => Step::Next(LaunchState::Preparing { search }),
                        Err(history) => Step::Next(self.exhausted(history)),
                    }
                }
            },
        }
    }

    // --- Transitions shared by several states ---------------------------------------

    /// Classify an attempt failure:
    /// transient → retry the step; nothing created → next offer directly;
    /// fatal → compensate then abort; otherwise compensate then next offer.
    fn attempt_failed(&self, search: &Search, error: ProviderError) -> Step<LaunchState> {
        tracing::warn!("[{}] attempt failed: {error}", search.offer());
        match error.kind {
            ErrorKind::Transient => Step::Retry { error: error.msg },
            ErrorKind::Unavailable => match search.clone().fail_and_advance(error) {
                Ok(search) => Step::Next(LaunchState::Preparing { search }),
                Err(history) => Step::Next(self.exhausted(history)),
            },
            kind if kind.is_fatal() => Step::Next(LaunchState::Compensating {
                teardown: Teardown::new(search.offer().location()),
                search: Some(search.clone()),
                failure: Some(error.clone()),
                then: AfterTeardown::Abort(error.to_string()),
            }),
            _ => Step::Next(self.compensation(search.clone(), error)),
        }
    }

    fn check_features(&self) -> Option<LaunchState> {
        self.provider
            .capabilities()
            .check(self.provider.name(), &self.required_features)
            .err()
            .map(|e| self.reject(e.msg))
    }

    /// The request itself is refused: unlike [`Self::abort`], the `vms` row
    /// (possibly a healthy VM) is left untouched.
    fn reject(&self, reason: String) -> LaunchState {
        LaunchState::Aborted {
            reason,
            history: Vec::new(),
        }
    }

    fn compensation(&self, search: Search, error: ProviderError) -> LaunchState {
        LaunchState::Compensating {
            teardown: Teardown::new(search.offer().location()),
            search: Some(search),
            failure: Some(error),
            then: AfterTeardown::NextOffer,
        }
    }

    fn exhausted(&self, history: Vec<Failure>) -> LaunchState {
        if self.retry_until_up {
            return LaunchState::WaitingToRetry { history };
        }
        self.project_state(VmState::Failed);
        LaunchState::Exhausted { history }
    }

    fn abort(&self, reason: String, history: Vec<Failure>) -> LaunchState {
        self.project_state(VmState::Failed);
        LaunchState::Aborted { reason, history }
    }

    fn leaked(&self, teardown: &Teardown, reason: String) -> LaunchState {
        let at = &teardown.location;
        if let Err(e) = self.db.mark_leaked(
            self.vm_name(),
            self.provider.name(),
            &at.region,
            at.zone.as_deref(),
        ) {
            tracing::error!(
                "could not record LEAKED state for '{}': {e}",
                self.vm_name()
            );
        }
        LaunchState::Leaked {
            location: at.clone(),
            reason,
        }
    }

    fn project_state(&self, state: VmState) {
        // Only an existing row is updated; a never-provisioned name has none.
        if let Err(e) = self.db.set_state(self.vm_name(), state) {
            tracing::error!("could not record {state} for '{}': {e}", self.vm_name());
        }
    }

    fn ready_vm(&self, offer: &Offer, observed: &Observed, reused: bool) -> ReadyVm {
        let mut endpoint = observed
            .endpoint
            .clone()
            .expect("ready instances have an endpoint");
        endpoint.user = Some(endpoint.user.unwrap_or_else(|| self.spec.ssh_user.clone()));
        ReadyVm {
            name: self.vm_name().to_string(),
            provider: self.provider.name().to_string(),
            offer: offer.clone(),
            zone: observed.zone.clone().or_else(|| offer.zone.clone()),
            instance_id: observed.instance_id.clone(),
            endpoint,
            private_ip: observed.private_ip.clone(),
            ssh_key_path: self.ssh_key_path.clone(),
            ssh_config_path: None,
            reused,
        }
    }

    /// Projection on success: per-VM SSH config entry + `UP` row.
    fn persist_up(&self, vm: &ReadyVm) -> Result<ReadyVm, String> {
        let ssh_config_path = config_file::write_vm_host_entry(
            &self.paths.ssh_dir(),
            &vm.name,
            &vm.endpoint.host,
            vm.endpoint.port,
            vm.user(),
            &vm.ssh_key_path,
        )
        .map_err(|e| format!("writing SSH config: {e}"))?;
        // `mark_up` only updates: make sure a row exists even when this run
        // never passed through `Preparing` (adopted VM, or a resume that
        // started past it).
        {
            let existing = self.db.get(&vm.name).map_err(|e| e.to_string())?;
            if existing.is_none() {
                self.db
                    .upsert_provisioning(
                        &vm.name,
                        &vm.provider,
                        &vm.offer.region,
                        vm.zone.as_deref(),
                        &vm.offer.instance_type,
                        &self.spec_json,
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        self.db
            .mark_up(
                &vm.name,
                &UpFields {
                    region: vm.offer.region.clone(),
                    zone: vm.zone.clone().unwrap_or_default(),
                    instance_id: vm.instance_id.clone(),
                    public_ip: vm.endpoint.host.clone(),
                    private_ip: vm.private_ip.clone(),
                    ssh_user: vm.user().to_string(),
                    ssh_port: vm.endpoint.port,
                    ssh_key_path: vm.ssh_key_path.display().to_string(),
                    ssh_config_path: ssh_config_path.display().to_string(),
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(ReadyVm {
            ssh_config_path: Some(ssh_config_path),
            ..vm.clone()
        })
    }
}

#[cfg(test)]
mod tests;
