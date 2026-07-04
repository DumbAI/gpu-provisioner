//! Wait for a VM's sshd to become ready — a port of SkyPilot's
//! `wait_for_ssh` (`sky/provision/provisioner.py`): a cheap raw TCP banner
//! probe first (sshd sends `SSH-2.0-...` on connect), then a real SSH login
//! probe, because sshd accepts TCP connections before logins are permitted
//! ("System is booting up...").

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use super::options::{ssh_target_args, SshOptions};

/// Overall timeout, matching SkyPilot's 10-minute maximum.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const BANNER_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const SSH_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("SSH to {user}@{ip} not ready after {secs}s (last: {last})")]
    Timeout {
        ip: String,
        user: String,
        secs: u64,
        last: String,
    },
}

/// Block until `user@ip` accepts an SSH login, or time out.
pub async fn wait_for_ssh(
    ip: &str,
    user: &str,
    opts: &SshOptions,
    overall_timeout: Duration,
) -> Result<(), WaitError> {
    let start = Instant::now();
    let mut attempts: u32 = 0;
    loop {
        let last = if banner_ok(ip, opts.port).await {
            match ssh_probe(ip, user, opts).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
            }
        } else {
            format!("tcp port {} not accepting SSH connections yet", opts.port)
        };
        if start.elapsed() >= overall_timeout {
            return Err(WaitError::Timeout {
                ip: ip.to_string(),
                user: user.to_string(),
                secs: overall_timeout.as_secs(),
                last,
            });
        }
        attempts += 1;
        if attempts.is_multiple_of(15) {
            tracing::info!(
                "still waiting for SSH on {ip} ({}s elapsed): {last}",
                start.elapsed().as_secs()
            );
        } else {
            tracing::debug!("waiting for SSH on {ip}: {last}");
        }
        tokio::time::sleep(RETRY_INTERVAL).await;
    }
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
