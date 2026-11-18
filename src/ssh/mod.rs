//! SSH subsystem: key management, readiness probing, command execution, and
//! the user-facing `~/.ssh/config` integration.

pub mod config_file;
pub mod keys;
pub mod options;
pub mod runner;
pub mod wait;

use std::path::PathBuf;

use crate::config::DEFAULT_SSH_USER;
use crate::db::VmRecord;
use options::SshOptions;

/// How to reach a recorded VM over SSH: its address, login and options
/// (key, port).
#[derive(Debug, Clone)]
pub struct Connection {
    pub host: String,
    pub user: String,
    pub opts: SshOptions,
}

/// A record that cannot be reached: it was never fully provisioned.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("VM '{0}' has no public IP recorded; re-run `gpuprov provision`")]
    NoAddress(String),
    #[error("VM '{0}' has no SSH key path recorded; re-run `gpuprov provision`")]
    NoKey(String),
}

impl Connection {
    pub fn for_record(record: &VmRecord) -> Result<Self, ConnectionError> {
        let host = record
            .public_ip
            .clone()
            .ok_or_else(|| ConnectionError::NoAddress(record.name.clone()))?;
        let key = record
            .ssh_key_path
            .clone()
            .map(PathBuf::from)
            .ok_or_else(|| ConnectionError::NoKey(record.name.clone()))?;
        Ok(Connection {
            host,
            user: record
                .ssh_user
                .clone()
                .unwrap_or_else(|| DEFAULT_SSH_USER.to_string()),
            opts: SshOptions::new(key).port(record.ssh_port.unwrap_or(22)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::VmState;

    fn record() -> VmRecord {
        VmRecord {
            name: "dev".into(),
            state: VmState::Up,
            provider: None,
            region: None,
            instance_type: None,
            zone: None,
            instance_id: None,
            public_ip: Some("203.0.113.7".into()),
            private_ip: None,
            ssh_user: None,
            ssh_port: Some(2222),
            ssh_key_path: Some("/keys/id".into()),
            ssh_config_path: None,
            spec_json: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_record_names_its_connection() {
        let c = Connection::for_record(&record()).unwrap();
        assert_eq!(c.host, "203.0.113.7");
        assert_eq!(c.user, DEFAULT_SSH_USER);
        let mut r = record();
        r.ssh_user = Some("root".into());
        assert_eq!(Connection::for_record(&r).unwrap().user, "root");
    }

    #[test]
    fn an_unprovisioned_record_has_none() {
        let mut r = record();
        r.public_ip = None;
        assert!(matches!(
            Connection::for_record(&r),
            Err(ConnectionError::NoAddress(_))
        ));
        let mut r = record();
        r.ssh_key_path = None;
        assert!(matches!(
            Connection::for_record(&r),
            Err(ConnectionError::NoKey(_))
        ));
    }
}
