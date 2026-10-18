//! Local SQLite state store.
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

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::util::now_unix;

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

#[derive(Debug, Clone)]
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
    pub ssh_key_path: Option<String>,
    pub ssh_config_path: Option<String>,
    pub spec_json: String,
    pub created_at: i64,
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

/// A held lease is considered abandoned once its heartbeat is this old
/// (the owner crashed or was killed).
pub const LEASE_STALE_SECS: i64 = 60;

/// One journal row (`machines`).
#[derive(Debug, Clone)]
pub struct MachineRow {
    pub kind: String,
    pub state_json: String,
    pub label: String,
    pub terminal: bool,
}

/// One transition-log entry (`machine_events`).
#[derive(Debug, Clone)]
pub struct MachineEvent {
    pub ts: i64,
    pub kind: String,
    pub label: String,
    pub detail: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error(
        "'{key}' is being operated on by another gpuprov process (pid {pid}); wait for it to \
         finish or kill it"
    )]
    Busy { key: String, pid: i64 },
    #[error("state db error: {0}")]
    Db(#[from] rusqlite::Error),
}

/// Create the table and apply additive migrations (older DBs lack columns
/// that were added later; `ALTER TABLE ADD COLUMN` is idempotent modulo the
/// duplicate-column error, which we swallow).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    add_column(conn, "instance_type TEXT")?;
    add_column(conn, "ssh_port INTEGER")?;
    // Rows written before the provider column existed were all AWS-provisioned.
    if add_column(conn, "provider TEXT")? {
        conn.execute("UPDATE vms SET provider = 'aws' WHERE provider IS NULL", [])?;
    }
    Ok(())
}

/// Additive migration; returns true when the column was newly added.
fn add_column(conn: &Connection, column_def: &str) -> rusqlite::Result<bool> {
    match conn.execute(&format!("ALTER TABLE vms ADD COLUMN {column_def}"), []) {
        Ok(_) => Ok(true),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(false),
        Err(e) => Err(e),
    }
}

/// The SQLite handle. The connection sits behind a mutex so `Db` is `Sync`
/// and can be shared by state machines whose step futures must be `Send`;
/// every method is a short, non-awaiting critical section.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if needed) the database. The parent directory must exist.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
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

    /// Record the start of a provisioning attempt (new VM or a new failover
    /// offer). Clears any stale instance fields from prior attempts.
    pub fn upsert_provisioning(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
        instance_type: &str,
        spec_json: &str,
    ) -> rusqlite::Result<()> {
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

    /// Record that `name`'s resources at `region`/`zone` could not be
    /// confirmed deleted (keeps the location so cleanup can find them).
    pub fn mark_leaked(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        zone: Option<&str>,
    ) -> rusqlite::Result<()> {
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

    pub fn mark_up(&self, name: &str, up: &UpFields) -> rusqlite::Result<()> {
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

    pub fn set_state(&self, name: &str, state: VmState) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE vms SET state = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, state.as_str(), now_unix()],
        )?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> rusqlite::Result<Option<VmRecord>> {
        self.conn()
            .query_row(
                "SELECT name, state, region, zone, instance_id, public_ip, private_ip,
                        ssh_user, ssh_key_path, ssh_config_path, spec_json,
                        created_at, updated_at, instance_type, provider, ssh_port
                 FROM vms WHERE name = ?1",
                params![name],
                record_from_row,
            )
            .optional()
    }

    pub fn list(&self) -> rusqlite::Result<Vec<VmRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, state, region, zone, instance_id, public_ip, private_ip,
                    ssh_user, ssh_key_path, ssh_config_path, spec_json,
                    created_at, updated_at, instance_type, provider, ssh_port
             FROM vms ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], record_from_row)?;
        rows.collect()
    }

    // --- state-machine journal ---------------------------------------------

    /// Take the single-writer lease on `key` for this process. Fails when a
    /// different, live (recent heartbeat) process holds it.
    pub fn acquire_lease(&self, key: &str, kind: &str) -> Result<(), LeaseError> {
        let conn = self.conn();
        let me = std::process::id() as i64;
        let now = now_unix();
        let holder: Option<(Option<i64>, Option<i64>, bool)> = conn
            .query_row(
                "SELECT owner_pid, heartbeat, terminal FROM machines WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)),
            )
            .optional()?;
        if let Some((Some(pid), Some(beat), false)) = holder {
            if pid != me && now - beat < LEASE_STALE_SECS {
                return Err(LeaseError::Busy {
                    key: key.to_string(),
                    pid,
                });
            }
        }
        conn.execute(
            "INSERT INTO machines (key, kind, state_json, label, terminal, owner_pid, heartbeat,
                                   updated_at)
             VALUES (?1, ?2, 'null', 'leased', 0, ?3, ?4, ?4)
             ON CONFLICT(key) DO UPDATE SET owner_pid = ?3, heartbeat = ?4",
            params![key, kind, me, now],
        )?;
        Ok(())
    }

    /// Drop this process's lease on `key` (no-op if someone else holds it).
    pub fn release_lease(&self, key: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE machines SET owner_pid = NULL WHERE key = ?1 AND owner_pid = ?2",
            params![key, std::process::id() as i64],
        )?;
        Ok(())
    }

    /// Refresh the lease heartbeat while a long poll is in progress.
    pub fn heartbeat(&self, key: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE machines SET heartbeat = ?2 WHERE key = ?1",
            params![key, now_unix()],
        )?;
        Ok(())
    }

    /// Persist a machine's state (write-ahead: called before the next step's
    /// side effects) and append the transition to the event log.
    pub fn save_machine(
        &self,
        key: &str,
        kind: &str,
        state_json: &str,
        label: &str,
        terminal: bool,
        detail: Option<&str>,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let now = now_unix();
        tx.execute(
            "INSERT INTO machines (key, kind, state_json, label, terminal, heartbeat, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(key) DO UPDATE SET kind = ?2, state_json = ?3, label = ?4,
               terminal = ?5, heartbeat = ?6, updated_at = ?6",
            params![key, kind, state_json, label, terminal as i64, now],
        )?;
        tx.execute(
            "INSERT INTO machine_events (key, kind, ts, label, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, kind, now, label, detail],
        )?;
        tx.commit()
    }

    pub fn load_machine(&self, key: &str) -> rusqlite::Result<Option<MachineRow>> {
        self.conn()
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
            .optional()
    }

    /// The most recent `limit` transitions for `key`, oldest first.
    pub fn machine_events(&self, key: &str, limit: usize) -> rusqlite::Result<Vec<MachineEvent>> {
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
        rows.collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn up_fields() -> UpFields {
        UpFields {
            region: "us-west-2".into(),
            zone: "us-west-2a".into(),
            instance_id: "i-0123".into(),
            public_ip: "1.2.3.4".into(),
            private_ip: Some("10.0.0.1".into()),
            ssh_user: "ubuntu".into(),
            ssh_port: 2222,
            ssh_key_path: "/keys/k".into(),
            ssh_config_path: "/ssh/vm".into(),
        }
    }

    #[test]
    fn lifecycle_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("vm1", "aws", "us-west-2", None, "g6e.8xlarge", "{}")
            .unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.provider.as_deref(), Some("aws"));
        assert_eq!(rec.region.as_deref(), Some("us-west-2"));
        assert_eq!(rec.instance_type.as_deref(), Some("g6e.8xlarge"));
        assert!(rec.public_ip.is_none());

        db.mark_up("vm1", &up_fields()).unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Up);
        assert_eq!(rec.public_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(rec.instance_id.as_deref(), Some("i-0123"));
        assert_eq!(rec.ssh_port, Some(2222));

        db.set_state("vm1", VmState::Terminated).unwrap();
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Terminated);
    }

    #[test]
    fn reprovision_clears_stale_instance_fields() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("vm1", "aws", "us-west-2", None, "t3.micro", "{}")
            .unwrap();
        db.mark_up("vm1", &up_fields()).unwrap();
        // Failover to a new region resets instance fields.
        db.upsert_provisioning("vm1", "aws", "us-east-1", None, "t3.micro", "{}")
            .unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.region.as_deref(), Some("us-east-1"));
        assert!(rec.public_ip.is_none());
        assert!(rec.instance_id.is_none());
    }

    #[test]
    fn list_returns_all() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("a", "aws", "us-west-2", None, "t3.micro", "{}")
            .unwrap();
        // Any provider id round-trips — the schema is cloud-agnostic.
        db.upsert_provisioning(
            "b",
            "gcp",
            "us-central1",
            Some("us-central1-a"),
            "a2-highgpu-1g",
            "{}",
        )
        .unwrap();
        let records = db.list().unwrap();
        let names: Vec<&str> = records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(records[0].provider.as_deref(), Some("aws"));
        assert_eq!(records[1].provider.as_deref(), Some("gcp"));
        assert_eq!(records[1].instance_type.as_deref(), Some("a2-highgpu-1g"));
    }

    #[test]
    fn leaked_keeps_location_for_cleanup() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning(
            "vm1",
            "gcp",
            "us-central1",
            Some("us-central1-a"),
            "g2",
            "{}",
        )
        .unwrap();
        db.mark_leaked("vm1", "gcp", "us-central1", Some("us-central1-a"))
            .unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Leaked);
        assert_eq!(rec.zone.as_deref(), Some("us-central1-a"));
        assert_eq!(rec.provider.as_deref(), Some("gcp"));
    }

    #[test]
    fn journal_saves_latest_state_and_appends_events() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.load_machine("vm1").unwrap().is_none());
        db.save_machine("vm1", "vm", r#""A""#, "a", false, None)
            .unwrap();
        db.save_machine("vm1", "vm", r#""B""#, "b", true, Some("why"))
            .unwrap();
        let row = db.load_machine("vm1").unwrap().unwrap();
        assert_eq!(row.state_json, r#""B""#);
        assert!(row.terminal);
        let events = db.machine_events("vm1", 10).unwrap();
        let labels: Vec<&str> = events.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b"]);
        assert_eq!(events[1].detail.as_deref(), Some("why"));
        // `limit` keeps the newest, still oldest-first.
        assert_eq!(db.machine_events("vm1", 1).unwrap()[0].label, "b");
    }

    #[test]
    fn lease_blocks_other_live_owner_but_not_self_or_stale() {
        let db = Db::open_in_memory().unwrap();
        db.acquire_lease("vm1", "vm").unwrap();
        db.acquire_lease("vm1", "vm").unwrap(); // re-entrant for the same pid
                                                // Simulate another live process holding it.
        db.conn()
            .execute(
                "UPDATE machines SET owner_pid = 999999999, heartbeat = ?1 WHERE key = 'vm1'",
                params![now_unix()],
            )
            .unwrap();
        assert!(matches!(
            db.acquire_lease("vm1", "vm"),
            Err(LeaseError::Busy { pid: 999999999, .. })
        ));
        // A stale heartbeat means the owner died: the lease is taken over.
        db.conn()
            .execute(
                "UPDATE machines SET heartbeat = ?1 WHERE key = 'vm1'",
                params![now_unix() - LEASE_STALE_SECS - 1],
            )
            .unwrap();
        db.acquire_lease("vm1", "vm").unwrap();
        db.release_lease("vm1").unwrap();
    }

    #[test]
    fn get_missing_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get("nope").unwrap().is_none());
    }

    /// A v1 database (no `instance_type`/`provider` columns) is migrated on
    /// open; pre-existing rows get `provider = 'aws'` backfilled (v1 only ever
    /// provisioned AWS).
    #[test]
    fn migrates_v1_schema() {
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
        let db = Db::open(&path).unwrap();
        let rec = db.get("old").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Up);
        assert!(rec.instance_type.is_none());
        assert!(rec.ssh_port.is_none());
        assert_eq!(rec.provider.as_deref(), Some("aws"));
        // New writes populate the migrated columns.
        db.upsert_provisioning("old", "aws", "us-east-2", None, "g6e.8xlarge", "{}")
            .unwrap();
        assert_eq!(
            db.get("old").unwrap().unwrap().instance_type.as_deref(),
            Some("g6e.8xlarge")
        );
        // Re-opening is idempotent and does not clobber providers.
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.get("old").unwrap().unwrap().provider.as_deref(),
            Some("aws")
        );
    }
}
