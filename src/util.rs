//! Filesystem layout and small shared helpers.

use std::io;
use std::path::{Path, PathBuf};

/// All local state lives under a single base directory:
/// `$GPUPROV_HOME` if set, else `~/.gpu-provisioner`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    pub fn resolve() -> io::Result<Self> {
        let base = match std::env::var_os("GPUPROV_HOME") {
            Some(p) => PathBuf::from(p),
            None => dirs::home_dir()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory")
                })?
                .join(".gpu-provisioner"),
        };
        Ok(Self { base })
    }

    pub fn keys_dir(&self) -> PathBuf {
        self.base.join("keys")
    }

    pub fn ssh_dir(&self) -> PathBuf {
        self.base.join("ssh")
    }

    pub fn sockets_dir(&self) -> PathBuf {
        self.base.join("sockets")
    }

    pub fn db_path(&self) -> PathBuf {
        self.base.join("state.db")
    }

    pub fn default_config_path(&self) -> PathBuf {
        self.base.join("config.toml")
    }

    /// The user's `~/.ssh/config`, where we maintain an `Include` directive.
    pub fn user_ssh_config_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".ssh").join("config"))
    }

    /// Create the base/keys/ssh/sockets directories with owner-only permissions.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [
            self.base.clone(),
            self.keys_dir(),
            self.ssh_dir(),
            self.sockets_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
            set_permissions(&dir, 0o700)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn set_permissions(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
