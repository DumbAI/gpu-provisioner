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
    pub region: Option<String>,
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
  region TEXT,
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

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database. The parent directory must exist.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record the start of a provisioning attempt (new VM or a new failover
    /// candidate region). Clears any stale instance fields from prior attempts.
    pub fn upsert_provisioning(
        &self,
        name: &str,
        region: &str,
        spec_json: &str,
    ) -> rusqlite::Result<()> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO vms (name, state, region, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(name) DO UPDATE SET
               state = excluded.state,
               region = excluded.region,
               zone = NULL, instance_id = NULL, public_ip = NULL, private_ip = NULL,
               spec_json = excluded.spec_json,
               updated_at = excluded.updated_at",
            params![name, VmState::Provisioning.as_str(), region, spec_json, now],
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
                        created_at, updated_at
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
                    created_at, updated_at
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
        db.upsert_provisioning("vm1", "us-west-2", "{}").unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.region.as_deref(), Some("us-west-2"));
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
        db.upsert_provisioning("vm1", "us-west-2", "{}").unwrap();
        db.mark_up("vm1", &up_fields()).unwrap();
        // Failover to a new region resets instance fields.
        db.upsert_provisioning("vm1", "us-east-1", "{}").unwrap();
        let rec = db.get("vm1").unwrap().unwrap();
        assert_eq!(rec.state, VmState::Provisioning);
        assert_eq!(rec.region.as_deref(), Some("us-east-1"));
        assert!(rec.public_ip.is_none());
        assert!(rec.instance_id.is_none());
    }

    #[test]
    fn list_returns_all() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_provisioning("a", "us-west-2", "{}").unwrap();
        db.upsert_provisioning("b", "us-east-1", "{}").unwrap();
        let names: Vec<String> = db.list().unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn get_missing_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get("nope").unwrap().is_none());
    }
}
