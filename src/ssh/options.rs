//! Sane SSH options — a direct port of SkyPilot's `ssh_options_list`
//! (`sky/utils/command_runner.py`): no host-key prompts, no known_hosts
//! pollution (IPs get recycled across VMs), fast dead-connection detection,
//! and ControlMaster multiplexing for repeated commands.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SshOptions {
    pub private_key: PathBuf,
    pub port: u16,
    pub connect_timeout_secs: u32,
    /// `BatchMode=yes` — fail instead of prompting (used by probes).
    pub batch_mode: bool,
    /// Directory for ControlMaster sockets; `None` disables multiplexing.
    pub control_dir: Option<PathBuf>,
}

impl SshOptions {
    pub fn new(private_key: PathBuf) -> Self {
        Self {
            private_key,
            port: 22,
            connect_timeout_secs: 30,
            batch_mode: false,
            control_dir: None,
        }
    }

    pub fn batch_mode(mut self, on: bool) -> Self {
        self.batch_mode = on;
        self
    }

    pub fn connect_timeout_secs(mut self, secs: u32) -> Self {
        self.connect_timeout_secs = secs;
        self
    }

    pub fn control_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.control_dir = dir;
        self
    }

    /// Build the ssh argument list (everything before `user@host`).
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec!["-i".to_string(), self.private_key.display().to_string()];
        let mut opt = |k: &str, v: String| {
            args.push("-o".to_string());
            args.push(format!("{k}={v}"));
        };
        opt("Port", self.port.to_string());
        // Skip host-key verification: VM IPs are recycled, so pinning
        // fingerprints only produces HOST IDENTIFICATION CHANGED noise.
        opt("StrictHostKeyChecking", "no".to_string());
        opt("UserKnownHostsFile", "/dev/null".to_string());
        opt("LogLevel", "ERROR".to_string());
        // Only use our key; don't spray the agent's identities at the server.
        opt("IdentitiesOnly", "yes".to_string());
        opt("ExitOnForwardFailure", "yes".to_string());
        // Detect broken connections quickly instead of hanging.
        opt("ServerAliveInterval", "5".to_string());
        opt("ServerAliveCountMax", "3".to_string());
        opt("ConnectTimeout", self.connect_timeout_secs.to_string());
        if self.batch_mode {
            opt("BatchMode", "yes".to_string());
        }
        if let Some(dir) = &self.control_dir {
            opt("ControlMaster", "auto".to_string());
            opt("ControlPath", format!("{}/%C", dir.display()));
            opt("ControlPersist", "300s".to_string());
        }
        args
    }
}

/// Full argument list for `ssh` targeting `user@host` (command appended by
/// the caller).
pub fn ssh_target_args(user: &str, host: &str, opts: &SshOptions) -> Vec<String> {
    let mut args = opts.to_args();
    args.push(format!("{user}@{host}"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_sane_options() {
        let opts = SshOptions::new(PathBuf::from("/k/id"))
            .batch_mode(true)
            .control_dir(Some(PathBuf::from("/tmp/sock")));
        let args = ssh_target_args("ubuntu", "1.2.3.4", &opts).join(" ");
        for expected in [
            "-i /k/id",
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "IdentitiesOnly=yes",
            "ServerAliveInterval=5",
            "BatchMode=yes",
            "ControlMaster=auto",
            "ControlPath=/tmp/sock/%C",
            "ubuntu@1.2.3.4",
        ] {
            assert!(args.contains(expected), "missing '{expected}' in: {args}");
        }
    }

    #[test]
    fn control_master_disabled_by_default() {
        let args = SshOptions::new(PathBuf::from("/k/id")).to_args().join(" ");
        assert!(!args.contains("ControlMaster"));
        assert!(!args.contains("BatchMode"));
    }
}
