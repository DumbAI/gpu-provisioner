//! Copy a local SQLite state file into a Postgres store
//! (`gpuprov state migrate --to postgres://…`).
//!
//! - **Refuses** while any operation is in flight under a live lease (a
//!   running `gpuprov` process): its journal would keep moving after the
//!   copy. Stale leases (crashed processes) don't block; their operations
//!   are copied and resume from Postgres.
//! - **Never overwrites** and is **idempotent**: rows already in Postgres
//!   are kept, a key's events are copied only when Postgres has none for it,
//!   and everything is one transaction — a rerun copies only what is new.
//! - Leases are not copied.

use super::{MachineEvent, MachineRow, PgStore, SqliteStore, StoreError, VmRecord};

/// Everything in a store, in copy order.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub vms: Vec<VmRecord>,
    pub machines: Vec<SnapshotMachine>,
    /// Oldest first.
    pub events: Vec<SnapshotEvent>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotMachine {
    pub key: String,
    pub row: MachineRow,
    pub updated_at: i64,
    /// The holder, when the operation is in flight under a fresh lease.
    pub live_lease: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotEvent {
    pub key: String,
    pub event: MachineEvent,
}

/// What an import wrote, and what it found already there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub vms_copied: usize,
    pub vms_kept: usize,
    pub machines_copied: usize,
    pub machines_kept: usize,
    pub events_copied: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub counts: ImportCounts,
    /// Non-terminal operations that were copied: they resume on the next
    /// run against Postgres (and must not also be resumed from SQLite).
    pub in_flight: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(
        "refusing to migrate: operations in flight under a live lease: {}; let them finish \
         (or stop them) and re-run",
        .0.join(", ")
    )]
    Leased(Vec<String>),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Copy `from` into `to` (see the module docs for the rules).
pub async fn sqlite_to_postgres(
    from: &SqliteStore,
    to: &PgStore,
) -> Result<MigrationReport, MigrateError> {
    let snapshot = from.snapshot()?;
    let leased: Vec<String> = snapshot
        .machines
        .iter()
        .filter_map(|m| {
            m.live_lease
                .as_ref()
                .map(|owner| format!("'{}' ({owner})", m.key))
        })
        .collect();
    if !leased.is_empty() {
        return Err(MigrateError::Leased(leased));
    }
    let counts = to.import(&snapshot).await?;
    let in_flight = snapshot
        .machines
        .iter()
        .filter(|m| !m.row.terminal)
        .map(|m| m.key.clone())
        .collect();
    Ok(MigrationReport { counts, in_flight })
}
