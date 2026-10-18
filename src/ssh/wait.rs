//! SSH readiness probe: a cheap raw TCP banner probe first (sshd
//! sends `SSH-2.0-...` on connect), then a real SSH login probe, because sshd
//! accepts TCP connections before logins are permitted ("System is booting
//! up...").
//!
//! This module probes once; the polling loop and its deadline belong to the
//! launch state machine's `Connecting` state (`vm::launch`).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use super::options::{ssh_target_args, SshOptions};

/// Overall SSH-readiness budget: 10 minutes covers slow first boots.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const BANNER_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One readiness probe of `user@host` (port from `opts`): TCP banner, then a
/// real login. `Err` carries the reason, for progress output.
pub async fn probe(host: &str, user: &str, opts: &SshOptions) -> Result<(), String> {
    if !banner_ok(host, opts.port).await {
        return Err(format!(
            "tcp port {} not accepting SSH connections yet",
            opts.port
        ));
    }
    ssh_probe(host, user, opts).await
}

/// Raw-socket probe: connect and check the `SSH` protocol banner.
async fn banner_ok(ip: &str, port: u16) -> bool {
    let connect = tokio::time::timeout(BANNER_PROBE_TIMEOUT, TcpStream::connect((ip, port))).await;
    let Ok(Ok(mut stream)) = connect else {
        return false;
    };
    let mut buf = [0u8; 100];
    match tokio::time::timeout(BANNER_PROBE_TIMEOUT, stream.read(&mut buf)).await {
        Ok(Ok(n)) if n >= 3 => buf[..n].starts_with(b"SSH"),
        _ => false,
    }
}

/// Real login probe: `ssh <opts> user@ip true` in batch mode.
async fn ssh_probe(ip: &str, user: &str, opts: &SshOptions) -> Result<(), String> {
    let probe_opts = opts
        .clone()
        .batch_mode(true)
        .connect_timeout_secs(SSH_PROBE_TIMEOUT.as_secs() as u32)
        // No ControlMaster during boot: a master socket established against a
        // half-booted sshd would be reused by later commands.
        .control_dir(None);
    let mut args = ssh_target_args(user, ip, &probe_opts);
    args.push("true".to_string());

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(SSH_PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Ok(Err(e)) => Err(format!("failed to run ssh: {e}")),
        Err(_) => Err(format!(
            "ssh probe timed out after {}s",
            SSH_PROBE_TIMEOUT.as_secs()
        )),
    }
}
