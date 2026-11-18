//! Execute commands on a VM over SSH, using the system OpenSSH binary: it gives us a real
//! interactive TTY, ControlMaster multiplexing, and the user's ProxyCommand
//! ecosystem for free.

use std::process::Stdio;

use super::options::{ssh_target_args, SshOptions};

/// Run a command remotely with inherited stdio (output streams live).
/// Returns the remote exit code (255 = ssh-level failure, per ssh convention).
pub async fn run_command(
    user: &str,
    host: &str,
    opts: &SshOptions,
    command: &[String],
) -> std::io::Result<i32> {
    let mut args = ssh_target_args(user, host, opts);
    args.push(quote_command(command));
    let status = tokio::process::Command::new("ssh")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    Ok(status.code().unwrap_or(255))
}

/// Run a command remotely and capture its output (unlike [`run_command`],
/// which streams stdio to the parent). Used by callers that need to parse the
/// remote output — e.g. reading back a k3s kubeconfig. Batch mode, no TTY.
pub async fn run_command_capture(
    user: &str,
    host: &str,
    opts: &SshOptions,
    command: &[String],
) -> std::io::Result<std::process::Output> {
    let mut args = ssh_target_args(user, host, opts);
    args.push(quote_command(command));
    tokio::process::Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
}

/// Like [`run_command_capture`], with `input` written to the remote command's
/// stdin. The way to hand a remote command secret material: unlike an
/// argument, stdin never shows in the remote process list or in our logs.
pub async fn run_command_with_input(
    user: &str,
    host: &str,
    opts: &SshOptions,
    command: &[String],
    input: &[u8],
) -> std::io::Result<std::process::Output> {
    use tokio::io::AsyncWriteExt;
    let mut args = ssh_target_args(user, host, opts);
    args.push(quote_command(command));
    let mut child = tokio::process::Command::new("ssh")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // Write while the output pipes drain, so neither side blocks on a full
    // pipe; dropping stdin is the remote command's end of input.
    let write = async move {
        let result = stdin.write_all(input).await;
        drop(stdin);
        result
    };
    let (written, output) = tokio::join!(write, child.wait_with_output());
    let output = output?;
    // A command that exits without reading all of its input breaks the
    // pipe; its exit status is the better report.
    match written {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e),
        _ => Ok(output),
    }
}

/// Replace the current process with an interactive `ssh user@host` session
/// (full TTY passthrough). Only returns on spawn failure.
#[cfg(unix)]
pub fn exec_interactive(user: &str, host: &str, opts: &SshOptions) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let args = ssh_target_args(user, host, opts);
    std::process::Command::new("ssh").args(&args).exec()
}

/// Replace this process with a foreground `ssh -N -L` port forward
/// (`127.0.0.1:<local_port>` → `<remote_host>:<remote_port>` as seen from
/// the VM: its loopback, or a cluster IP). Returns only on spawn failure.
pub fn exec_forward(
    user: &str,
    host: &str,
    opts: &SshOptions,
    local_port: u16,
    (remote_host, remote_port): (&str, u16),
) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut args = vec![
        "-N".to_string(),
        "-L".to_string(),
        format!("127.0.0.1:{local_port}:{remote_host}:{remote_port}"),
    ];
    args.extend(ssh_target_args(user, host, opts));
    std::process::Command::new("ssh").args(&args).exec()
}

/// Join command parts into a single remote command string, quoting each part
/// so argument boundaries survive the remote shell (shlex.quote semantics).
/// A single part is passed through as-is, so `gpuprov ssh vm -- 'a && b'`
/// behaves exactly like `ssh vm 'a && b'`.
pub fn quote_command(parts: &[String]) -> String {
    if parts.len() == 1 {
        return parts[0].clone();
    }
    parts
        .iter()
        .map(|p| sh_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_part_passes_through() {
        assert_eq!(
            quote_command(&v(&["echo hi && uname -a"])),
            "echo hi && uname -a"
        );
    }

    #[test]
    fn multiple_parts_are_quoted() {
        assert_eq!(quote_command(&v(&["ls", "-la", "/tmp"])), "ls -la /tmp");
        assert_eq!(
            quote_command(&v(&["echo", "hello world"])),
            "echo 'hello world'"
        );
        assert_eq!(quote_command(&v(&["echo", "it's"])), r"echo 'it'\''s'");
    }
}
