//! Execute commands on a VM over SSH — the mini analog of SkyPilot's
//! `SSHCommandRunner`. Uses the system OpenSSH binary: it gives us a real
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

/// Replace the current process with an interactive `ssh user@host` session
/// (full TTY passthrough). Only returns on spawn failure.
#[cfg(unix)]
pub fn exec_interactive(user: &str, host: &str, opts: &SshOptions) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let args = ssh_target_args(user, host, opts);
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
