//! User-facing SSH config integration — the mini analog of SkyPilot's
//! `SSHConfigHelper`: one generated file per VM under our own directory, plus
//! a single `Include` directive at the top of `~/.ssh/config`, so plain
//! `ssh <vm-name>` works alongside `gpuprov ssh <vm-name>`.

use std::io;
use std::path::{Path, PathBuf};

use crate::util::set_permissions;

const AUTOGEN_COMMENT: &str = "# Added by gpuprov";

/// Render the per-VM Host block.
pub fn host_entry(name: &str, ip: &str, user: &str, key_path: &Path) -> String {
    format!(
        "{comment} (vm: {name})
Host {name}
  HostName {ip}
  User {user}
  IdentityFile {key}
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile=/dev/null
  GlobalKnownHostsFile=/dev/null
  Port 22
",
        comment = AUTOGEN_COMMENT,
        name = name,
        ip = ip,
        user = user,
        key = key_path.display(),
    )
}

/// Write (or overwrite) the per-VM config file and return its path.
pub fn write_vm_host_entry(
    ssh_dir: &Path,
    name: &str,
    ip: &str,
    user: &str,
    key_path: &Path,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(ssh_dir)?;
    set_permissions(ssh_dir, 0o700)?;
    let path = ssh_dir.join(name);
    std::fs::write(&path, host_entry(name, ip, user, key_path))?;
    set_permissions(&path, 0o644)?;
    Ok(path)
}

/// Remove the per-VM config file (no-op if absent).
pub fn remove_vm_host_entry(ssh_dir: &Path, name: &str) -> io::Result<()> {
    let path = ssh_dir.join(name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Ensure `Include <ssh_dir>/*` is present in the user's ssh config,
/// prepending it if missing. It must appear before any `Host` block to apply
/// globally, hence prepend. Idempotent.
pub fn ensure_include(user_ssh_config: &Path, ssh_dir: &Path) -> io::Result<()> {
    let include_line = format!("Include {}/*", ssh_dir.display());
    let existing = match std::fs::read_to_string(user_ssh_config) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.lines().any(|l| l.trim() == include_line) {
        return Ok(());
    }
    if let Some(parent) = user_ssh_config.parent() {
        std::fs::create_dir_all(parent)?;
        set_permissions(parent, 0o700)?;
    }
    let content = format!("{AUTOGEN_COMMENT}: per-VM SSH configs\n{include_line}\n\n{existing}");
    std::fs::write(user_ssh_config, content)?;
    set_permissions(user_ssh_config, 0o600)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_entry_fields() {
        let entry = host_entry("vm1", "1.2.3.4", "ubuntu", Path::new("/keys/id"));
        for expected in [
            "Host vm1",
            "HostName 1.2.3.4",
            "User ubuntu",
            "IdentityFile /keys/id",
            "StrictHostKeyChecking no",
            "Port 22",
        ] {
            assert!(entry.contains(expected), "missing '{expected}'");
        }
    }

    #[test]
    fn write_and_remove_entry() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join("ssh");
        let path =
            write_vm_host_entry(&ssh_dir, "vm1", "1.2.3.4", "ubuntu", Path::new("/k")).unwrap();
        assert!(path.exists());
        remove_vm_host_entry(&ssh_dir, "vm1").unwrap();
        assert!(!path.exists());
        // Removing again is fine.
        remove_vm_host_entry(&ssh_dir, "vm1").unwrap();
    }

    #[test]
    fn include_is_prepended_once_and_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".ssh").join("config");
        let ssh_dir = dir.path().join("gpuprov-ssh");

        // Fresh file.
        ensure_include(&cfg, &ssh_dir).unwrap();
        let c1 = std::fs::read_to_string(&cfg).unwrap();
        assert!(c1.starts_with(AUTOGEN_COMMENT));
        assert!(c1.contains(&format!("Include {}/*", ssh_dir.display())));

        // Idempotent.
        ensure_include(&cfg, &ssh_dir).unwrap();
        let c2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(c1, c2);

        // Existing content is preserved below the include.
        std::fs::write(&cfg, "Host existing\n  HostName 9.9.9.9\n").unwrap();
        ensure_include(&cfg, &ssh_dir).unwrap();
        let c3 = std::fs::read_to_string(&cfg).unwrap();
        assert!(c3.starts_with(AUTOGEN_COMMENT));
        assert!(c3.contains("Host existing"));
        assert!(c3.find("Include").unwrap() < c3.find("Host existing").unwrap());
    }
}
