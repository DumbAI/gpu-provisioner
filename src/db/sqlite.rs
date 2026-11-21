//! The SQLite backend: one local file (`~/.gpu-provisioner/state.db`), the
//! CLI's store and the default for tests.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::migrate::{Snapshot, SnapshotEvent, SnapshotMachine};
use super::{
    JournalEntry, LeaseOwner, MachineEvent, MachineRow, StateStore, StoreError, UpFields, VmRecord,
    VmState, LEASE_STALE_SECS,
};
use crate::util::now_unix;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vms (
  name TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  provider TEXT,
  region TEXT,
  instance_type TEXT,
  zone TEXT,
  instance_id TEXT,
  public_ip TEXT,
  private_ip TEXT,
  ssh_user TEXT,
  ssh_key_path TEXT,
  ssh_config_path TEXT,
  spec_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS machines (
  key TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  state_json TEXT NOT NULL,
  label TEXT NOT NULL,
  terminal INTEGER NOT NULL,
  owner_pid INTEGER,
  heartbeat INTEGER,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS machine_events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  key TEXT NOT NULL,
  kind TEXT NOT NULL,
  ts INTEGER NOT NULL,
  label TEXT NOT NULL,
  detail TEXT
);
";

/// How long a statement waits for another process's write lock (several
/// `gpuprov` processes may share the file) before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Create the tables and apply additive migrations (older DBs lack columns
/// that were added later; `ALTER TABLE ADD COLUMN` is idempotent modulo the
/// duplicate-column error, which we swallow).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    add_column(conn, "vms", "instance_type TEXT")?;
    add_column(conn, "vms", "ssh_port INTEGER")?;
    // Rows written before the provider column existed were all AWS-provisioned.
    if add_column(conn, "vms", "provider TEXT")? {
        conn.execute("UPDATE vms SET provider = 'aws' WHERE provider IS NULL", [])?;
    }
    // The lease owner (`LeaseOwner`); `owner_pid` is no longer written.
    add_column(conn, "machines", "owner TEXT")?;
    Ok(())
}

/// Additive migration; returns true when the column was newly added.
fn add_column(conn: &Connection, table: &str, column_def: &str) -> rusqlite::Result<bool> {
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column_def}"), []) {
        Ok(_) => Ok(true),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(false),
        Err(e) => Err(e),
    }
}

const RECORD_COLUMNS: &str = "name, state, region, zone, instance_id, public_ip, private_ip,
    ssh_user, ssh_key_path, ssh_config_path, spec_json,
    created_at, updated_at, instance_type, provider, ssh_port";

/// The SQLite store. The connection sits behind a mutex so the store is
/// `Sync`; every method is a short, non-awaiting critical section, so it
/// answers inline rather than on a blocking thread. Lease times use SQLite's
/// own clock (`unixepoch()`), as the Postgres backend uses the server's.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) the database. The parent directory must exist.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::with_connection(conn)
    }

    /// A private, empty database (tests, dry runs).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves the connection itself intact.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Everything, for copying into another store (`migrate`). `live_lease`
    /// is set on non-terminal machines whose lease is held and fresh
    /// (including leases written by older gpuprov versions, by pid).
    pub(crate) fn snapshot(&self) -> Result<Snapshot, StoreError> {
        let conn = self.conn();
        let vms = {
            let mut stmt = conn.prepare(&format!(
                "SELECT {RECORD_COLUMNS} FROM vms ORDER BY created_at, name"
            ))?;
            let rows = stmt.query_map([], record_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let machines = {
            let mut stmt = conn.prepare(
                "SELECT key, kind, state_json, label, terminal, updated_at,
                        CASE WHEN terminal = 0
                              AND (owner IS NOT NULL OR owner_pid IS NOT NULL)
                              AND heartbeat > unixepoch() - ?1
                             THEN COALESCE(owner, 'pid ' || owner_pid) END
                 FROM machines WHERE state_json != 'null' ORDER BY key",
            )?;
            let rows = stmt.query_map(params![LEASE_STALE_SECS], |r| {
                Ok(SnapshotMachine {
                    key: r.get(0)?,
                    row: MachineRow {
                        kind: r.get(1)?,
                        state_json: r.get(2)?,
                        label: r.get(3)?,
                        terminal: r.get::<_, i64>(4)? != 0,
                    },
                    updated_at: r.get(5)?,
                    live_lease: r.get(6)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let events = {
            let mut stmt = conn
                .prepare("SELECT key, ts, kind, label, detail FROM machine_events ORDER BY seq")?;
            let rows = stmt.query_map([], |r| {
                Ok(SnapshotEvent {
                    key: r.get(0)?,
                    event: MachineEvent {
                        ts: r.get(1)?,
                        kind: r.get(2)?,
                        label: r.get(3)?,
                        detail: r.get(4)?,
                    },
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Snapshot {
            vms,
            machines,
            events,
        })
    }

    /// Test support: age `key`'s heartbeat by `secs`.
    #[cfg(test)]
    pub(crate) fn backdate_heartbeat(&self, key: &str, secs: i64) {
        self.conn()
            .execute(
                "UPDATE machines SET heartbeat = unixepoch() - ?2 WHERE key = ?1",
                params![key, secs],
            )
            .unwrap();
    }

    /// Test support: raw SQL.
    #[cfg(test)]
    pub(crate) fn execute(&self, sql: &str) {
        self.conn().execute_batch(sql).unwrap();
    }
}

#[async_trait]
impl StateStore for SqliteStore {
    async fn upsert_provisioning(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
        instance_type: &str,
        spec_json: &str,
    ) -> Result<(), StoreError> {
        let now = now_unix();
        self.conn().execute(
            "INSERT INTO vms (name, state, provider, region, zone, instance_type, spec_json,
                              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(name) DO UPDATE SET
               state = excluded.state,
               provider = excluded.provider,
               region = excluded.region,
               zone = excluded.zone,
               instance_type = excluded.instance_type,
               instance_id = NULL, public_ip = NULL, private_ip = NULL,
               spec_json = excluded.spec_json,
               updated_at = excluded.updated_at",
            params![
                name,
                VmState::Provisioning.as_str(),
                provider,
                region,
                zone,
                instance_type,
                spec_json,
                now
            ],
        )?;
        Ok(())
    }

    async fn mark_leaked(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
    ) -> Result<(), StoreError> {
        let now = now_unix();
        self.conn().execute(
            "INSERT INTO vms (name, state, provider, region, zone, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?6, ?6)
             ON CONFLICT(name) DO UPDATE SET
               state = excluded.state, provider = excluded.provider,
               region = excluded.region, zone = excluded.zone,
               updated_at = excluded.updated_at",
            params![name, VmState::Leaked.as_str(), provider, region, zone, now],
        )?;
        Ok(())
    }

    async fn mark_up(&self, name: &str, up: &UpFields) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE vms SET
               state = ?2, region = ?3, zone = ?4, instance_id = ?5,
               public_ip = ?6, private_ip = ?7,
               ssh_user = ?8, ssh_key_path = ?9, ssh_config_path = ?10,
               updated_at = ?11, ssh_port = ?12
             WHERE name = ?1",
            params![
                name,
                VmState::Up.as_str(),
                up.region,
                up.zone,
                up.instance_id,
                up.public_ip,
                up.private_ip,
                up.ssh_user,
                up.ssh_key_path,
                up.ssh_config_path,
                now_unix(),
                up.ssh_port
            ],
        )?;
        Ok(())
    }

    async fn set_state(&self, name: &str, state: VmState) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE vms SET state = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, state.as_str(), now_unix()],
        )?;
        Ok(())
    }

    async fn get(&self, name: &str) -> Result<Option<VmRecord>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {RECORD_COLUMNS} FROM vms WHERE name = ?1"),
                params![name],
                record_from_row,
            )
            .optional()?)
    }

    async fn list(&self) -> Result<Vec<VmRecord>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM vms ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], record_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    async fn acquire_lease(
        &self,
        key: &str,
        kind: &str,
        owner: &LeaseOwner,
    ) -> Result<(), StoreError> {
        let conn = self.conn();
        let granted: Option<String> = conn
            .query_row(
                "INSERT INTO machines (key, kind, state_json, label, terminal, owner, heartbeat,
                                       updated_at)
                 VALUES (?1, ?2, 'null', 'leased', 0, ?3, unixepoch(), unixepoch())
                 ON CONFLICT(key) DO UPDATE SET owner = excluded.owner, owner_pid = NULL,
                                               heartbeat = excluded.heartbeat
                 WHERE machines.owner IS NULL
                    OR machines.owner = excluded.owner
                    OR machines.heartbeat IS NULL
                    OR machines.heartbeat <= unixepoch() - ?4
                    OR machines.terminal != 0
                 RETURNING owner",
                params![key, kind, owner.as_str(), LEASE_STALE_SECS],
                |r| r.get(0),
            )
            .optional()?;
        if granted.is_some() {
            return Ok(());
        }
        let holder: Option<String> = conn
            .query_row(
                "SELECT owner FROM machines WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Err(StoreError::Busy {
            key: key.to_string(),
            owner: holder.unwrap_or_else(|| "unknown".into()),
        })
    }

    async fn release_lease(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE machines SET owner = NULL WHERE key = ?1 AND owner = ?2",
            params![key, owner.as_str()],
        )?;
        Ok(())
    }

    async fn heartbeat(&self, key: &str, owner: &LeaseOwner) -> Result<(), StoreError> {
        let updated = self.conn().execute(
            "UPDATE machines SET heartbeat = unixepoch() WHERE key = ?1 AND owner = ?2",
            params![key, owner.as_str()],
        )?;
        if updated == 0 {
            return Err(StoreError::LeaseLost {
                key: key.to_string(),
            });
        }
        Ok(())
    }

    async fn save_machine(
        &self,
        key: &str,
        owner: &LeaseOwner,
        entry: JournalEntry<'_>,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = now_unix();
        let updated = tx.execute(
            "UPDATE machines SET kind = ?3, state_json = ?4, label = ?5, terminal = ?6,
                                 heartbeat = unixepoch(), updated_at = ?7
             WHERE key = ?1 AND owner = ?2",
            params![
                key,
                owner.as_str(),
                entry.kind,
                entry.state_json,
                entry.label,
                entry.terminal as i64,
                now
            ],
        )?;
        if updated == 0 {
            // Dropping `tx` rolls back (nothing was written anyway).
            return Err(StoreError::LeaseLost {
                key: key.to_string(),
            });
        }
        tx.execute(
            "INSERT INTO machine_events (key, kind, ts, label, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, entry.kind, now, entry.label, entry.detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    async fn load_machine(&self, key: &str) -> Result<Option<MachineRow>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT kind, state_json, label, terminal FROM machines
                 WHERE key = ?1 AND state_json != 'null'",
                params![key],
                |r| {
                    Ok(MachineRow {
                        kind: r.get(0)?,
                        state_json: r.get(1)?,
                        label: r.get(2)?,
                        terminal: r.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()?)
    }

    async fn machine_events(
        &self,
        key: &str,
        limit: usize,
    ) -> Result<Vec<MachineEvent>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT ts, kind, label, detail FROM
               (SELECT seq, ts, kind, label, detail FROM machine_events
                WHERE key = ?1 ORDER BY seq DESC LIMIT ?2)
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![key, limit as i64], |r| {
            Ok(MachineEvent {
                ts: r.get(0)?,
                kind: r.get(1)?,
                label: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<VmRecord> {
    let state_str: String = row.get(1)?;
    let state = VmState::parse(&state_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown vm state '{state_str}'"),
            )),
        )
    })?;
    Ok(VmRecord {
        name: row.get(0)?,
        state,
        region: row.get(2)?,
        zone: row.get(3)?,
        instance_id: row.get(4)?,
        public_ip: row.get(5)?,
        private_ip: row.get(6)?,
        ssh_user: row.get(7)?,
        ssh_key_path: row.get(8)?,
        ssh_config_path: row.get(9)?,
        spec_json: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        instance_type: row.get(13)?,
        provider: row.get(14)?,
        ssh_port: row.get(15)?,
    })
}

/// SQLite-only behavior; the shared contract is in `db::tests`.
#[cfg(test)]
mod tests {
    use super::*;

    /// A v1 database (no `instance_type`/`provider` columns) is migrated on
    /// open; pre-existing rows get `provider = 'aws'` backfilled (v1 only ever
    /// provisioned AWS).
    #[tokio::test]
    async fn migrates_v1_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE vms (
                   name TEXT PRIMARY KEY, state TEXT NOT NULL,
                   region TEXT, zone TEXT, instance_id TEXT,
                   public_ip TEXT, private_ip TEXT,
                   ssh_user TEXT, ssh_key_path TEXT, ssh_config_path TEXT,
                   spec_json TEXT NOT NULL,
                   created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 INSERT INTO vms (name, state, region, spec_json, created_at, updated_at)
                 VALUES ('old', 'UP', 'us-west-2', '{}', 1, 1);",
            )
            .unwrap();
        }
        let db = SqliteStore::open(&path).unwrap();
        let rec = db.get("old").await.unwrap().unwrap();
        assert_eq!(rec.state, VmState::Up);
        assert!(rec.instance_type.is_none());
        assert!(rec.ssh_port.is_none());
        assert_eq!(rec.provider.as_deref(), Some("aws"));
        // New writes populate the migrated columns.
        db.upsert_provisioning("old", "aws", "us-east-2", None, "g6e.8xlarge", "{}")
            .await
            .unwrap();
        let rec = db.get("old").await.unwrap().unwrap();
        assert_eq!(rec.instance_type.as_deref(), Some("g6e.8xlarge"));
        // Re-opening is idempotent and does not clobber providers.
        drop(db);
        let db = SqliteStore::open(&path).unwrap();
        let rec = db.get("old").await.unwrap().unwrap();
        assert_eq!(rec.provider.as_deref(), Some("aws"));
    }

    /// A journal written before leases had owners (pid only) still opens,
    /// and its lease is taken over.
    #[tokio::test]
    async fn pre_owner_journal_is_taken_over() {
        let db = SqliteStore::open_in_memory().unwrap();
        db.execute(
            "INSERT INTO machines (key, kind, state_json, label, terminal, owner_pid, heartbeat,
                                   updated_at)
             VALUES ('vm1', 'vm', '\"Planning\"', 'planning', 0, 4242, 0, 0)",
        );
        let me = LeaseOwner::new();
        db.acquire_lease("vm1", "vm", &me).await.unwrap();
        db.heartbeat("vm1", &me).await.unwrap();
    }
}
