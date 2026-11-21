//! The state store: everything gpuprov remembers between steps and runs.
//!
//! Two concerns, two table groups:
//!
//! - `vms` — the user-facing **projection**, one row per VM (`list`, `ssh`,
//!   `down` read it). Lifecycle:
//!   `PROVISIONING` → `UP` → `TERMINATED`, `FAILED` on exhausted failover,
//!   `LEAKED` when teardown could not confirm cloud resources are gone.
//! - `machines` + `machine_events` — the state-machine **journal** (source of
//!   truth for in-flight operations, see `machine/`): the latest serialized
//!   state per key, a single-writer lease, and an append-only transition log.
//!
//! [`StateStore`] is the interface; two backends implement it with identical
//! semantics: [`SqliteStore`] (a local file — the CLI's store, and the
//! default for tests) and [`PgStore`] (Postgres, in its own `gpuprov` schema —
//! for embedders that run on hosts without durable disks, or several workers
//! sharing work).
//!
//! # The lease
//!
//! One writer drives a key at a time. A lease is held by a [`LeaseOwner`]
//! (one per `Executor` run: host, pid and a random token), taken atomically
//! in a single statement, kept alive by heartbeats, and considered abandoned
//! once its heartbeat is [`LEASE_STALE_SECS`] old by the **database's** clock.
//! Writes to the journal ([`StateStore::save_machine`]) and heartbeats are
//! **fenced**: they succeed only while the caller still owns the lease, and
//! otherwise fail with [`StoreError::LeaseLost`] — a run that stalled past the
//! staleness window and was taken over never writes again.

use std::fmt;

use async_trait::async_trait;

pub mod migrate;
mod postgres;
mod sqlite;

pub use postgres::{PgOptions, PgStore};
pub use sqlite::SqliteStore;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Provisioning,
    Up,
    Failed,
    Terminated,
    /// Teardown gave up while cloud resources may still exist; the next
    /// `provision`/`down` for the name must clean up first.
    Leaked,
}

impl VmState {
    pub fn as_str(&self) -> &'static str {
        match self {
            VmState::Provisioning => "PROVISIONING",
            VmState::Up => "UP",
            VmState::Failed => "FAILED",
            VmState::Terminated => "TERMINATED",
            VmState::Leaked => "LEAKED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PROVISIONING" => Some(VmState::Provisioning),
            "UP" => Some(VmState::Up),
            "FAILED" => Some(VmState::Failed),
            "TERMINATED" => Some(VmState::Terminated),
            "LEAKED" => Some(VmState::Leaked),
            _ => None,
        }
    }
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VmRecord {
    pub name: String,
    pub state: VmState,
    /// Canonical provider id (e.g. "aws") — which adapter owns this VM.
    pub provider: Option<String>,
    pub region: Option<String>,
    pub instance_type: Option<String>,
    pub zone: Option<String>,
    pub instance_id: Option<String>,
    pub public_ip: Option<String>,
    pub private_ip: Option<String>,
    pub ssh_user: Option<String>,
    /// `None` on rows written before the column existed (port 22).
    pub ssh_port: Option<u16>,
    /// The private key the VM was provisioned with. Embedders that supply
    /// the key (`ssh::keys::materialize_keypair`) should write it to the same
    /// path on every host, so this stays valid wherever the record is read.
    pub ssh_key_path: Option<String>,
    pub ssh_config_path: Option<String>,
    pub spec_json: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
}

/// Fields recorded when a VM reaches `UP`.
#[derive(Debug, Clone)]
pub struct UpFields {
    pub region: String,
    pub zone: String,
    pub instance_id: String,
    pub public_ip: String,
    pub private_ip: Option<String>,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub ssh_key_path: String,
    pub ssh_config_path: String,
}

/// A held lease is considered abandoned once its heartbeat is this old (the
/// owner crashed, was killed, or stalled), measured by the store's clock.
pub const LEASE_STALE_SECS: i64 = 60;

/// One journal row (`machines`).
#[derive(Debug, Clone, PartialEq)]
pub struct MachineRow {
    pub kind: String,
    pub state_json: String,
    pub label: String,
    pub terminal: bool,
}

/// One transition to journal ([`StateStore::save_machine`]).
#[derive(Debug, Clone, Copy)]
pub struct JournalEntry<'a> {
    pub kind: &'a str,
    pub state_json: &'a str,
    pub label: &'a str,
    pub terminal: bool,
    pub detail: Option<&'a str>,
}

/// One transition-log entry (`machine_events`).
#[derive(Debug, Clone, PartialEq)]
pub struct MachineEvent {
    /// Unix seconds.
    pub ts: i64,
    pub kind: String,
    pub label: String,
    pub detail: Option<String>,
}

/// Who holds a lease: `host:pid:token`, with a fresh random token per value.
///
/// One owner per `Executor` run rather than per process: an embedder runs
/// many operations in one process (a worker may even retry an operation
/// while a stalled earlier attempt is still alive), and those must exclude
/// and fence each other exactly as two hosts do. Host and pid are there for
/// the humans reading a `Busy` error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOwner(String);

impl LeaseOwner {
    pub fn new() -> Self {
        use rand_core::RngCore;
        let mut token = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut token);
        let token: String = token.iter().map(|b| format!("{b:02x}")).collect();
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        LeaseOwner(format!("{host}:{}:{token}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LeaseOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LeaseOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "'{key}' is being operated on by another gpuprov run ({owner}); wait for it to finish \
         or stop it (a lease without a heartbeat for {}s is taken over)",
        LEASE_STALE_SECS
    )]
    Busy { key: String, owner: String },
    #[error(
        "lost the lease on '{key}': another run took it over after this one missed its \
         heartbeats; stopped without writing (the new owner carries the operation on)"
    )]
    LeaseLost { key: String },
    #[error("state db error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state db error: {0}")]
    Postgres(String),
    #[error("state db: {0}")]
    Invalid(String),
}

impl From<tokio_postgres::Error> for StoreError {
    fn from(e: tokio_postgres::Error) -> Self {
        // `Display` alone says only "db error"; the server's message is the
        // part worth reading.
        StoreError::Postgres(match e.as_db_error() {
            Some(db) => format!("{}: {}", db.severity(), db.message()),
            None => match std::error::Error::source(&e) {
                Some(source) => format!("{e}: {source}"),
                None => e.to_string(),
            },
        })
    }
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        match e {
            deadpool_postgres::PoolError::Backend(e) => e.into(),
            other => StoreError::Postgres(other.to_string()),
        }
    }
}

/// Everything the state machines, the executor and the CLI read or write.
///
/// Async because the executor is: a store round trip (Postgres over the
/// network) must not block a runtime thread. [`SqliteStore`] answers inline
/// (a short, local critical section).
///
/// Shared as a trait object (`&dyn StateStore`, or `Arc<dyn StateStore>`
/// when owned) rather than a concrete enum of backends: the machines need
/// only these operations, each backend keeps its connection and error
/// handling to itself, tests can wrap a store to inject faults, and the
/// virtual call is noise next to the round trip behind it. Both backends are
/// cheap to share (a mutexed connection, a connection pool).
#[async_trait]
pub trait StateStore: Send + Sync {
    // --- the `vms` projection ------------------------------------------------

    /// Record the start of a provisioning attempt (new VM or a new failover
    /// offer). Clears any stale instance fields from prior attempts.
    async fn upsert_provisioning(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
        instance_type: &str,
        spec_json: &str,
    ) -> Result<(), StoreError>;

    /// Record that `name`'s resources at `region`/`zone` could not be
    /// confirmed deleted (keeps the location so cleanup can find them).
    async fn mark_leaked(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
    ) -> Result<(), StoreError>;

    /// `UP` with its endpoint. Updates an existing row only.
    async fn mark_up(&self, name: &str, up: &UpFields) -> Result<(), StoreError>;

    /// Updates an existing row only.
    async fn set_state(&self, name: &str, state: VmState) -> Result<(), StoreError>;

    async fn get(&self, name: &str) -> Result<Option<VmRecord>, StoreError>;

    /// Every record, oldest first.
    async fn list(&self) -> Result<Vec<VmRecord>, StoreError>;

    // --- the journal and its lease -------------------------------------------

    /// Take the single-writer lease on `key` for `owner`, in one atomic
    /// statement: granted when the key is unleased, already `owner`'s, its
    /// heartbeat is stale, or its operation is terminal; otherwise
    /// [`StoreError::Busy`].
    async fn acquire_lease(
        &self,
        key: &str,
        kind: &str,
        owner: &LeaseOwner,
    ) -> Result<(), StoreError>;

    /// Drop `owner`'s lease on `key` (a no-op if someone else holds it).
    async fn release_lease(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError>;

    /// Refresh the lease while a long step or poll is in progress. Fenced:
    /// [`StoreError::LeaseLost`] when `owner` no longer holds it.
    async fn heartbeat(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError>;

    /// Persist a machine's state (write-ahead: called before the next step's
    /// side effects) and append the transition to the event log, atomically.
    /// Fenced: [`StoreError::LeaseLost`] (and nothing written) when `owner`
    /// no longer holds the lease.
    async fn save_machine(
        &self,
        key: &str,
        owner: &LeaseOwner,
        entry: JournalEntry<'_>,
    ) -> Result<(), StoreError>;

    async fn load_machine(&self, key: &str) -> Result<Option<MachineRow>, StoreError>;

    /// The most recent `limit` transitions for `key`, oldest first.
    async fn machine_events(
        &self,
        key: &str,
        limit: usize,
    ) -> Result<Vec<MachineEvent>, StoreError>;
}

/// Test support: journal a state for `key` as a run that then went away
/// (takes and releases a lease of its own).
#[cfg(test)]
pub(crate) async fn seed_machine(
    store: &dyn StateStore,
    key: &str,
    kind: &str,
    state_json: &str,
    label: &str,
    terminal: bool,
) {
    let owner = LeaseOwner::new();
    store.acquire_lease(key, kind, &owner).await.unwrap();
    store
        .save_machine(
            key,
            &owner,
            JournalEntry {
                kind,
                state_json,
                label,
                terminal,
                detail: None,
            },
        )
        .await
        .unwrap();
    store.release_lease(key, &owner).await.unwrap();
}
