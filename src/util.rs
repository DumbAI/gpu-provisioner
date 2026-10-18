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

    /// Per-VM kubeconfig files (`<base>/kube/<name>.yaml`) for k3s clusters.
    pub fn kube_dir(&self) -> PathBuf {
        self.base.join("kube")
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

    /// Create the base/keys/ssh/sockets/kube directories with owner-only permissions.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [
            self.base.clone(),
            self.keys_dir(),
            self.ssh_dir(),
            self.sockets_dir(),
            self.kube_dir(),
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

/// `YYYY-MM-DD HH:MM:SS` (UTC) for a unix timestamp — enough for log-style
/// output without a date-time dependency (Howard Hinnant's civil-from-days).
pub fn format_unix_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod format_tests {
    use super::format_unix_utc;

    #[test]
    fn formats_known_instants() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_unix_utc(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(format_unix_utc(1_792_359_127), "2026-10-18 21:32:07");
    }
}
