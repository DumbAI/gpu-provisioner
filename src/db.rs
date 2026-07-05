//! Local SQLite state store — the mini analog of SkyPilot's `global_user_state`.
//!
//! One row per VM. State transitions mirror SkyPilot's cluster lifecycle
//! (`add_or_update_cluster(ready=False)` → `ready=True`):
//! `PROVISIONING` → `UP` → `TERMINATED`, with `FAILED` on exhausted failover.

use std::fmt;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::util::now_unix;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Provisioning,
    Up,
    Failed,
    Terminated,
}

impl VmState {
    pub fn as_str(&self) -> &'static str {
        match self {
            VmState::Provisioning => "PROVISIONING",
            VmState::Up => "UP",
            VmState::Failed => "FAILED",
            VmState::Terminated => "TERMINATED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PROVISIONING" => Some(VmState::Provisioning),
            "UP" => Some(VmState::Up),
            "FAILED" => Some(VmState::Failed),
            "TERMINATED" => Some(VmState::Terminated),
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
";

/// Create the table and apply additive migrations (older DBs lack columns
/// that were added later; `ALTER TABLE ADD COLUMN` is idempotent modulo the
/// duplicate-column error, which we swallow).
fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    add_column(conn, "instance_type TEXT")?;
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

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database. The parent directory must exist.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        init_schema(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Record the start of a provisioning attempt (new VM or a new failover
    /// candidate region). Clears any stale instance fields from prior attempts.
    pub fn upsert_provisioning(
        &self,
        name: &str,
        provider: &str,
        region: &str,
        instance_type: &str,
        spec_json: &str,
    ) -> rusqlite::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO vms (name, state, provider, region, instance_type, spec_json,
                              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(name) DO UPDATE SET
               state = excluded.state,
               provider = excluded.provider,
               region = excluded.region,
               instance_type = excluded.instance_type,
               zone = NULL, instance_id = NULL, public_ip = NULL, private_ip = NULL,
               spec_json = excluded.spec_json,
               updated_at = excluded.updated_at",
            params![
                name,
                VmState::Provisioning.as_str(),
                provider,
                region,
                instance_type,
                spec_json,
                now
            ],
        )?;
        Ok(())
    }

    pub fn mark_up(&self, name: &str, up: &UpFields) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE vms SET
               state = ?2, region = ?3, zone = ?4, instance_id = ?5,
               public_ip = ?6, private_ip = ?7,
               ssh_user = ?8, ssh_key_path = ?9, ssh_config_path = ?10,
               updated_at = ?11
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
                now_unix()
            ],
        )?;
        Ok(())
    }

    pub fn set_state(&self, name: &str, state: VmState) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE vms SET state = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, state.as_str(), now_unix()],
        )?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> rusqlite::Result<Option<VmRecord>> {
        self.conn
            .query_row(
                "SELECT name, state, region, zone, instance_id, public_ip, private_ip,
                        ssh_user, ssh_key_path, ssh_config_path, spec_json,
                        created_at, updated_at, instance_type, provider
                 FROM vms WHERE name = ?1",
                params![name],
                record_from_row,
            )
            .optional()
    }

    pub fn list(&self) -> rusqlite::Result<Vec<VmRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, state, region, zone, instance_id, public_ip, private_ip,
                    ssh_user, ssh_key_path, ssh_config_path, spec_json,
                    created_at, updated_at, instance_type, provider
             FROM vms ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], record_from_row)?;
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
            ssh_key_path: "/keys/k".into(),
            ssh_config_path: "/ssh/vm".into(),
        }
    }

    #[test]
    fn lifecycle_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("vm1", "aws", "us-west-2", "g6e.8xlarge", "{}")
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

        db.set_state("vm1", VmState::Terminated).unwrap();
        assert_eq!(db.get("vm1").unwrap().unwrap().state, VmState::Terminated);
    }

    #[test]
    fn reprovision_clears_stale_instance_fields() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("vm1", "aws", "us-west-2", "t3.micro", "{}")
            .unwrap();
        db.mark_up("vm1", &up_fields()).unwrap();
        // Failover to a new region resets instance fields.
        db.upsert_provisioning("vm1", "aws", "us-east-1", "t3.micro", "{}")
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
        db.upsert_provisioning("a", "aws", "us-west-2", "t3.micro", "{}")
            .unwrap();
        // Any provider id round-trips — the schema is cloud-agnostic.
        db.upsert_provisioning("b", "gcp", "us-central1", "a2-highgpu-1g", "{}")
            .unwrap();
        let records = db.list().unwrap();
        let names: Vec<&str> = records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(records[0].provider.as_deref(), Some("aws"));
        assert_eq!(records[1].provider.as_deref(), Some("gcp"));
        assert_eq!(records[1].instance_type.as_deref(), Some("a2-highgpu-1g"));
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
        assert_eq!(rec.provider.as_deref(), Some("aws"));
        // New writes populate the migrated columns.
        db.upsert_provisioning("old", "aws", "us-east-2", "g6e.8xlarge", "{}")
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
