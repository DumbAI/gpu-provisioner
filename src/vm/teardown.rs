//! VM teardown as a reusable sub-machine — the one implementation behind both
//! `gpuprov down` ([`TeardownMachine`]) and the launch machine's
//! compensation after a failed attempt (`vm::launch`).
//!
//! `Requesting → AwaitingGone → CleaningUp → done`, each phase level-triggered
//! (observe first), so re-running any phase after a crash is safe. Running out
//! of retries or time is a `Leaked` outcome — persisted, shown by `list`, and
//! resolved by the next `provision`/`down` — never a silent move-on.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::db::{Db, VmRecord, VmState};
use crate::machine::{Machine, Step, StepCx};
use crate::provider::{CloudProvider, InstanceStatus, Location, ProviderError};
use crate::ssh::config_file;
use crate::util::Paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeardownPhase {
    /// Observe what exists and request deletion.
    Requesting,
    /// Wait for the instances to disappear (re-requesting stragglers).
    AwaitingGone,
    /// Delete per-VM side resources (security group, firewall rule).
    CleaningUp,
}

/// Teardown progress for one VM at one location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Teardown {
    pub location: Location,
    pub phase: TeardownPhase,
}

impl Teardown {
    pub fn new(location: Location) -> Self {
        Teardown {
            location,
            phase: TeardownPhase::Requesting,
        }
    }
}

/// Result of advancing a [`Teardown`] by one step.
#[derive(Debug)]
pub enum TeardownStep {
    Next(Teardown),
    Done,
    Poll(String),
    Retry(String),
    /// The cloud never confirmed deletion within the provider's window.
    TimedOut(String),
}

/// Advance a teardown by one phase. Every provider error is retried (within
/// the caller's budget): giving up on teardown risks a leak, so even
/// "permanent-looking" errors get the full budget before `Leaked`.
pub async fn step_teardown(
    provider: &dyn CloudProvider,
    vm_name: &str,
    td: &Teardown,
    cx: &StepCx,
) -> TeardownStep {
    let at = &td.location;
    let retry = |e: ProviderError| TeardownStep::Retry(format!("teardown at {at}: {e}"));
    match td.phase {
        TeardownPhase::Requesting => {
            let observed = match provider.observe(at, vm_name).await {
                Ok(o) => o,
                Err(e) => return retry(e),
            };
            if observed.is_empty() {
                return TeardownStep::Next(Teardown {
                    location: at.clone(),
                    phase: TeardownPhase::CleaningUp,
                });
            }
            let ids: Vec<String> = observed.into_iter().map(|o| o.instance_id).collect();
            tracing::info!(
                "[{at}] deleting instance(s) of '{vm_name}': {}",
                ids.join(", ")
            );
            match provider.delete(at, vm_name, &ids).await {
                Ok(()) => TeardownStep::Next(Teardown {
                    location: at.clone(),
                    phase: TeardownPhase::AwaitingGone,
                }),
                Err(e) => retry(e),
            }
        }
        TeardownPhase::AwaitingGone => {
            let observed = match provider.observe(at, vm_name).await {
                Ok(o) => o,
                Err(e) => return retry(e),
            };
            if observed.is_empty() {
                return TeardownStep::Next(Teardown {
                    location: at.clone(),
                    phase: TeardownPhase::CleaningUp,
                });
            }
            let caps = provider.capabilities();
            if cx.in_state >= caps.delete_timeout {
                return TeardownStep::TimedOut(format!(
                    "{} instance(s) still present at {at} after {}s",
                    observed.len(),
                    caps.delete_timeout.as_secs()
                ));
            }
            // A delete request can be lost (or race a still-booting
            // instance): re-request anything not visibly going away.
            let stragglers: Vec<String> = observed
                .iter()
                .filter(|o| o.status != InstanceStatus::Terminating)
                .map(|o| o.instance_id.clone())
                .collect();
            if !stragglers.is_empty() {
                if let Err(e) = provider.delete(at, vm_name, &stragglers).await {
                    return retry(e);
                }
            }
            TeardownStep::Poll(format!("{} instance(s) still terminating", observed.len()))
        }
        TeardownPhase::CleaningUp => match provider.cleanup(at, vm_name).await {
            Ok(()) => TeardownStep::Done,
            Err(e) => retry(e),
        },
    }
}

// ---------------------------------------------------------------------------
// Standalone machine for `gpuprov down`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeardownState {
    Running(Teardown),
    Done,
    Leaked { location: Location, reason: String },
}

impl TeardownState {
    /// Start tearing down whatever a `vms` row points at; `None` when the
    /// row never got a location (nothing was ever created in a cloud).
    pub fn for_record(record: &VmRecord) -> Option<TeardownState> {
        let region = record.region.clone()?;
        Some(TeardownState::Running(Teardown::new(Location {
            region,
            zone: record.zone.clone().filter(|z| !z.is_empty()),
        })))
    }
}

/// `gpuprov down` as a durable machine: an interrupted `down` resumes, and a
/// failed one leaves the VM `LEAKED` (with its location) instead of lying.
pub struct TeardownMachine<'a> {
    pub provider: &'a dyn CloudProvider,
    pub db: &'a Db,
    pub paths: &'a Paths,
    pub vm_name: String,
}

#[async_trait]
impl Machine for TeardownMachine<'_> {
    type State = TeardownState;
    const KIND: &'static str = "teardown";

    fn is_terminal(&self, s: &TeardownState) -> bool {
        !matches!(s, TeardownState::Running(_))
    }

    fn label(&self, s: &TeardownState) -> String {
        match s {
            TeardownState::Running(td) => format!("teardown {:?} at {}", td.phase, td.location),
            TeardownState::Done => "terminated".to_string(),
            TeardownState::Leaked { location, .. } => format!("LEAKED at {location}"),
        }
    }

    fn detail(&self, s: &TeardownState) -> Option<String> {
        match s {
            TeardownState::Leaked { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }

    async fn step(&self, s: &TeardownState, cx: &StepCx) -> Step<TeardownState> {
        let TeardownState::Running(td) = s else {
            unreachable!("terminal states are never stepped")
        };
        match step_teardown(self.provider, &self.vm_name, td, cx).await {
            TeardownStep::Next(td) => Step::Next(TeardownState::Running(td)),
            TeardownStep::Poll(note) => Step::Poll {
                after: self.provider.capabilities().poll_interval,
                note,
            },
            TeardownStep::Retry(error) => Step::Retry { error },
            TeardownStep::TimedOut(reason) => self.leaked(td, reason),
            TeardownStep::Done => {
                let projected = self
                    .db
                    .set_state(&self.vm_name, VmState::Terminated)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        config_file::remove_vm_host_entry(&self.paths.ssh_dir(), &self.vm_name)
                            .map_err(|e| e.to_string())
                    });
                match projected {
                    Ok(()) => Step::Next(TeardownState::Done),
                    Err(error) => Step::Retry { error },
                }
            }
        }
    }

    fn on_retries_exhausted(&self, s: &TeardownState, last_error: String) -> TeardownState {
        let TeardownState::Running(td) = s else {
            return s.clone();
        };
        self.leaked_state(td, last_error)
    }

    /// Ctrl-C during `down` keeps tearing down (stopping halfway is how leaks
    /// happen); a second Ctrl-C exits and leaves the journal resumable.
    fn on_cancel(&self, s: &TeardownState) -> Option<TeardownState> {
        Some(s.clone())
    }
}

impl TeardownMachine<'_> {
    fn leaked_state(&self, td: &Teardown, reason: String) -> TeardownState {
        if let Err(e) = self.db.mark_leaked(
            &self.vm_name,
            self.provider.name(),
            &td.location.region,
            td.location.zone.as_deref(),
        ) {
            tracing::error!("could not record LEAKED state for '{}': {e}", self.vm_name);
        }
        TeardownState::Leaked {
            location: td.location.clone(),
            reason,
        }
    }

    fn leaked(&self, td: &Teardown, reason: String) -> Step<TeardownState> {
        Step::Next(self.leaked_state(td, reason))
    }
}
