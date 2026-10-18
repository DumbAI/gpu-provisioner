//! The shared, failure-tolerant core: a durable executor for explicit state
//! machines. Every operation that touches a cloud (`vm::launch`,
//! `vm::teardown`, `cluster::k3s`) is a [`Machine`] — a serializable state
//! enum plus a `step` function — and this module supplies everything that
//! must be right exactly once:
//!
//! - **write-ahead journaling**: each transition is persisted (SQLite
//!   `machines` + `machine_events`) before the next step's side effects run,
//!   so a crash or Ctrl-C resumes from the last durable state;
//! - **retry budgets with exponential backoff** for transient failures, with
//!   the machine naming the failure state when the budget runs out;
//! - **polling** (`Step::Poll`) that consumes no budget, with the elapsed
//!   time-in-state exposed so machines enforce their own deadlines;
//! - **cancellation at step boundaries only** — an in-flight cloud call is
//!   never dropped (that would lose the handle it returns); the machine maps
//!   the cancelled state to a compensating one via `on_cancel`;
//! - a **single-writer lease** per key so two processes never drive the same
//!   VM concurrently.
//!
//! Design rationale and diagrams: `doc/STATE_MACHINE_DESIGN.md` §4.2.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::db::{Db, LeaseError, LEASE_STALE_SECS};

/// Lease refresh cadence during a step — well inside the staleness window.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(LEASE_STALE_SECS as u64 / 3);

/// Outcome of one `step`.
#[derive(Debug)]
pub enum Step<S> {
    /// Transition to a new state (persisted before the next step runs).
    Next(S),
    /// Not done yet (e.g. instance still booting): re-run the same state
    /// after `after`. Consumes no retry budget.
    Poll { after: Duration, note: String },
    /// Transient failure: re-run the same state after backoff, consuming one
    /// unit of the state's retry budget.
    Retry { error: String },
}

/// Exponential backoff budget for consecutive `Retry`s within one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    /// 5 attempts, 2s → 4s → 8s → 16s (capped at 30s): ~30s of patience for
    /// throttling / 5xx, after which the error is treated as real.
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Backoff before retry number `attempt` (1-based).
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// Per-step context from the executor.
#[derive(Debug, Clone, Copy)]
pub struct StepCx {
    /// Time since this process entered the current state. Resets on resume,
    /// which gives a recovered operation a fresh — still bounded — window.
    pub in_state: Duration,
    /// Consecutive `Retry`s so far in the current state.
    pub retries: u32,
}

/// A durable state machine driven by [`Executor`].
#[async_trait]
pub trait Machine: Send + Sync {
    type State: Serialize + DeserializeOwned + Clone + Debug + Send + Sync;

    /// Journal discriminator (`"vm"`, `"teardown"`, `"k3s"`), so a row is
    /// never deserialized as the wrong machine.
    const KIND: &'static str;

    fn is_terminal(&self, state: &Self::State) -> bool;

    /// Short human-readable label for progress output and the event log.
    fn label(&self, state: &Self::State) -> String;

    /// Extra context recorded with a transition into `state` (e.g. the error
    /// that caused it).
    fn detail(&self, _state: &Self::State) -> Option<String> {
        None
    }

    /// Advance from `state`. Must be idempotent: after a crash the same state
    /// may be stepped again even though its side effect already happened.
    async fn step(&self, state: &Self::State, cx: &StepCx) -> Step<Self::State>;

    fn retry_policy(&self, _state: &Self::State) -> RetryPolicy {
        RetryPolicy::default()
    }

    /// The state to move to when `state`'s retry budget is spent.
    fn on_retries_exhausted(&self, state: &Self::State, last_error: String) -> Self::State;

    /// The state to move to when the user cancels while in `state`; `None`
    /// stops right away, leaving the journal resumable.
    fn on_cancel(&self, _state: &Self::State) -> Option<Self::State> {
        None
    }
}

/// Cooperative cancellation shared between the signal handler and executors.
#[derive(Clone, Default)]
pub struct Cancel {
    inner: Arc<(AtomicBool, Notify)>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.0.store(true, Ordering::SeqCst);
        self.inner.1.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.0.load(Ordering::SeqCst)
    }

    /// Sleep for `duration`, waking early on cancellation.
    async fn sleep(&self, duration: Duration) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.1.notified();
        tokio::select! {
            _ = tokio::time::sleep(duration) => {}
            _ = notified => {}
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error("state db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("cannot (de)serialize machine state: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("interrupted; resume by re-running the same command")]
    Interrupted,
}

/// Drives one machine instance (keyed by VM name) to a terminal state.
pub struct Executor<'a, M: Machine> {
    machine: &'a M,
    db: &'a Db,
    key: String,
    cancel: Cancel,
}

impl<'a, M: Machine> Executor<'a, M> {
    pub fn new(machine: &'a M, db: &'a Db, key: impl Into<String>, cancel: Cancel) -> Self {
        Executor {
            machine,
            db,
            key: key.into(),
            cancel,
        }
    }

    /// The journaled state for this key if it belongs to this machine kind
    /// and is still in flight (i.e. should be resumed rather than restarted).
    pub fn resumable(&self) -> Result<Option<M::State>, ExecError> {
        match self.db.load_machine(&self.key)? {
            Some(row) if row.kind == M::KIND && !row.terminal => {
                Ok(Some(serde_json::from_str(&row.state_json)?))
            }
            _ => Ok(None),
        }
    }

    /// Resume an in-flight run for this key if there is one, else start from
    /// `initial`; run to a terminal state and return it.
    pub async fn run(&self, initial: M::State) -> Result<M::State, ExecError> {
        self.db.acquire_lease(&self.key, M::KIND)?;
        let start = match self.resumable()? {
            Some(state) => {
                tracing::info!(
                    "[{}] resuming interrupted operation at: {}",
                    self.key,
                    self.machine.label(&state)
                );
                state
            }
            None => initial,
        };
        let result = self.drive(start).await;
        self.db.release_lease(&self.key)?;
        result
    }

    async fn drive(&self, mut state: M::State) -> Result<M::State, ExecError> {
        self.persist(&state)?;
        let mut entered = Instant::now();
        let mut retries: u32 = 0;
        let mut cancel_applied = false;

        loop {
            if self.machine.is_terminal(&state) {
                return Ok(state);
            }
            if self.cancel.is_cancelled() && !cancel_applied {
                cancel_applied = true;
                match self.machine.on_cancel(&state) {
                    Some(next) => {
                        tracing::warn!(
                            "[{}] cancelled — compensating: {}",
                            self.key,
                            self.machine.label(&next)
                        );
                        self.persist(&next)?;
                        state = next;
                        entered = Instant::now();
                        retries = 0;
                        continue;
                    }
                    None => return Err(ExecError::Interrupted),
                }
            }

            let cx = StepCx {
                in_state: entered.elapsed(),
                retries,
            };
            match self.step_with_heartbeat(&state, &cx).await? {
                Step::Next(next) => {
                    self.persist(&next)?;
                    state = next;
                    entered = Instant::now();
                    retries = 0;
                }
                Step::Poll { after, note } => {
                    tracing::debug!("[{}] {}: {note}", self.key, self.machine.label(&state));
                    self.db.heartbeat(&self.key)?;
                    self.cancel.sleep(after).await;
                }
                Step::Retry { error } => {
                    retries += 1;
                    let policy = self.machine.retry_policy(&state);
                    if retries >= policy.max_attempts {
                        let next = self.machine.on_retries_exhausted(&state, error);
                        self.persist(&next)?;
                        state = next;
                        entered = Instant::now();
                        retries = 0;
                    } else {
                        let backoff = policy.backoff(retries);
                        tracing::warn!(
                            "[{}] {} failed (attempt {retries}/{}), retrying in {}s: {error}",
                            self.key,
                            self.machine.label(&state),
                            policy.max_attempts,
                            backoff.as_secs()
                        );
                        self.db.heartbeat(&self.key)?;
                        self.cancel.sleep(backoff).await;
                    }
                }
            }
        }
    }

    /// Run one step while refreshing the lease: a single step can legitimately
    /// take minutes (the k3s installer, a cloud operation wait), longer than
    /// the lease's staleness window.
    async fn step_with_heartbeat(
        &self,
        state: &M::State,
        cx: &StepCx,
    ) -> Result<Step<M::State>, ExecError> {
        let step = self.machine.step(state, cx);
        tokio::pin!(step);
        loop {
            tokio::select! {
                outcome = &mut step => return Ok(outcome),
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => self.db.heartbeat(&self.key)?,
            }
        }
    }

    fn persist(&self, state: &M::State) -> Result<(), ExecError> {
        let label = self.machine.label(state);
        let detail = self.machine.detail(state);
        tracing::info!("[{}] {label}", self.key);
        self.db.save_machine(
            &self.key,
            M::KIND,
            &serde_json::to_string(state)?,
            &label,
            self.machine.is_terminal(state),
            detail.as_deref(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde::Deserialize;

    use super::*;

    /// Toy machine: `Count(n)` → `Count(n+1)` until `Done`; scripted
    /// per-state outcomes let tests inject polls and transient failures.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Toy {
        Count(u32),
        Waiting,
        Done,
        GaveUp(String),
        Compensated,
    }

    #[derive(Default)]
    struct ToyMachine {
        /// States that fail transiently forever.
        flaky: Vec<u32>,
        /// Number of polls `Waiting` performs before finishing.
        polls: u32,
        /// Calls observed, for assertions.
        log: Mutex<Vec<String>>,
        /// Cancel this handle when reaching Count(n).
        cancel_at: Option<(u32, Cancel)>,
        /// Make `on_cancel` compensate (vs. stop).
        compensate_on_cancel: bool,
    }

    #[async_trait]
    impl Machine for ToyMachine {
        type State = Toy;
        const KIND: &'static str = "toy";

        fn is_terminal(&self, s: &Toy) -> bool {
            matches!(s, Toy::Done | Toy::GaveUp(_) | Toy::Compensated)
        }

        fn label(&self, s: &Toy) -> String {
            format!("{s:?}")
        }

        async fn step(&self, s: &Toy, cx: &StepCx) -> Step<Toy> {
            self.log.lock().unwrap().push(format!("{s:?}"));
            match s {
                Toy::Count(n) if self.flaky.contains(n) => Step::Retry {
                    error: format!("flaky {n}"),
                },
                Toy::Count(n) => {
                    if let Some((at, cancel)) = &self.cancel_at {
                        if at == n {
                            cancel.cancel();
                        }
                    }
                    if *n >= 2 {
                        Step::Next(Toy::Waiting)
                    } else {
                        Step::Next(Toy::Count(n + 1))
                    }
                }
                Toy::Waiting => {
                    let polled = self
                        .log
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|l| *l == "Waiting")
                        .count() as u32;
                    if polled > self.polls {
                        assert!(cx.in_state >= Duration::from_secs(self.polls as u64));
                        Step::Next(Toy::Done)
                    } else {
                        Step::Poll {
                            after: Duration::from_secs(1),
                            note: "not yet".into(),
                        }
                    }
                }
                _ => unreachable!("terminal"),
            }
        }

        fn on_retries_exhausted(&self, _s: &Toy, last_error: String) -> Toy {
            Toy::GaveUp(last_error)
        }

        fn on_cancel(&self, _s: &Toy) -> Option<Toy> {
            self.compensate_on_cancel.then_some(Toy::Compensated)
        }
    }

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn runs_to_terminal_and_journals_every_transition() {
        let machine = ToyMachine {
            polls: 2,
            ..Default::default()
        };
        let db = db();
        let end = Executor::new(&machine, &db, "k", Cancel::new())
            .run(Toy::Count(0))
            .await
            .unwrap();
        assert_eq!(end, Toy::Done);
        let labels: Vec<String> = db
            .machine_events("k", 100)
            .unwrap()
            .into_iter()
            .map(|e| e.label)
            .collect();
        // Polls are not transitions: Waiting is journaled once.
        assert_eq!(
            labels,
            ["Count(0)", "Count(1)", "Count(2)", "Waiting", "Done"]
        );
        assert!(db.load_machine("k").unwrap().unwrap().terminal);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_exhaustion_moves_to_machine_named_state() {
        let machine = ToyMachine {
            flaky: vec![1],
            ..Default::default()
        };
        let db = db();
        let end = Executor::new(&machine, &db, "k", Cancel::new())
            .run(Toy::Count(0))
            .await
            .unwrap();
        assert_eq!(end, Toy::GaveUp("flaky 1".into()));
        let attempts = machine
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| *l == "Count(1)")
            .count();
        assert_eq!(attempts as u32, RetryPolicy::default().max_attempts);
    }

    #[tokio::test(start_paused = true)]
    async fn resumes_from_journaled_non_terminal_state() {
        let db = db();
        // A previous process crashed in Count(2).
        db.save_machine("k", "toy", r#"{"Count":2}"#, "Count(2)", false, None)
            .unwrap();
        let machine = ToyMachine::default();
        let end = Executor::new(&machine, &db, "k", Cancel::new())
            .run(Toy::Count(0))
            .await
            .unwrap();
        assert_eq!(end, Toy::Done);
        // Never restarted from the initial state.
        assert!(!machine
            .log
            .lock()
            .unwrap()
            .contains(&"Count(0)".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_or_foreign_journal_rows_are_not_resumed() {
        let db = db();
        db.save_machine("k", "other-kind", r#"{"Count":2}"#, "x", false, None)
            .unwrap();
        let machine = ToyMachine::default();
        Executor::new(&machine, &db, "k", Cancel::new())
            .run(Toy::Count(0))
            .await
            .unwrap();
        assert_eq!(machine.log.lock().unwrap()[0], "Count(0)");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_maps_to_compensation_at_a_step_boundary() {
        let cancel = Cancel::new();
        let machine = ToyMachine {
            cancel_at: Some((1, cancel.clone())),
            compensate_on_cancel: true,
            ..Default::default()
        };
        let db = db();
        let end = Executor::new(&machine, &db, "k", cancel)
            .run(Toy::Count(0))
            .await
            .unwrap();
        // The in-flight step (Count(1)) completed; then cancellation applied.
        assert_eq!(end, Toy::Compensated);
        assert_eq!(*machine.log.lock().unwrap(), ["Count(0)", "Count(1)"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_without_compensation_leaves_journal_resumable() {
        let cancel = Cancel::new();
        let machine = ToyMachine {
            cancel_at: Some((1, cancel.clone())),
            ..Default::default()
        };
        let db = db();
        let exec = Executor::new(&machine, &db, "k", cancel);
        assert!(matches!(
            exec.run(Toy::Count(0)).await,
            Err(ExecError::Interrupted)
        ));
        assert_eq!(exec.resumable().unwrap(), Some(Toy::Count(2)));
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(1), Duration::from_secs(2));
        assert_eq!(p.backoff(2), Duration::from_secs(4));
        assert_eq!(p.backoff(4), Duration::from_secs(16));
        assert_eq!(p.backoff(10), Duration::from_secs(30));
    }
}
