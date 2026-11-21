//! SSH identity: one keypair for every VM.
//!
//! Two ways to get it:
//!
//! - [`ensure_keypair`] — the CLI's: an ed25519 key generated once under the
//!   local keys directory and reused (ed25519: small, fast, the modern
//!   default).
//! - [`materialize_keypair`] — for embedders that own the key (e.g. a worker
//!   reading it from a secret, on hosts whose disk does not survive a
//!   deploy): the caller's private key is written to a directory the caller
//!   picks, because `ssh` needs a file. VM records store that path
//!   (`ssh_key_path`), so pass the **same directory on every host** and
//!   records stay usable wherever they are read.

use std::path::{Path, PathBuf};

use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::util::set_permissions;

pub const PRIVATE_KEY_FILE: &str = "gpuprov_ed25519";
pub const PUBLIC_KEY_FILE: &str = "gpuprov_ed25519.pub";

#[derive(Debug, Clone)]
pub struct KeyPair {
    pub private_path: PathBuf,
    pub public_path: PathBuf,
    /// Single-line OpenSSH public key (`ssh-ed25519 AAAA...`).
    pub public_key_openssh: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("ssh key i/o error at '{path}': {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("ssh key error: {0}")]
    Key(#[from] ssh_key::Error),
    #[error("the supplied ssh private key is passphrase-protected; supply it unencrypted")]
    Encrypted,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> KeyError + '_ {
    move |source| KeyError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Ensure the keypair exists under `keys_dir` (created by the caller with
/// 0700). Generates on first use; loads and re-derives the public key
/// afterwards.
pub fn ensure_keypair(keys_dir: &Path) -> Result<KeyPair, KeyError> {
    let private_path = keys_dir.join(PRIVATE_KEY_FILE);
    let public_path = keys_dir.join(PUBLIC_KEY_FILE);

    let public_key_openssh = if private_path.exists() {
        let pem = std::fs::read_to_string(&private_path).map_err(io_err(&private_path))?;
        let key = PrivateKey::from_openssh(&pem)?;
        let public = key.public_key().to_openssh()?;
        // Heal a missing/stale public key file.
        if !public_path.exists() {
            std::fs::write(&public_path, format!("{public}\n")).map_err(io_err(&public_path))?;
            set_permissions(&public_path, 0o644).map_err(io_err(&public_path))?;
        }
        public
    } else {
        let key = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519)?;
        let pem = key.to_openssh(LineEnding::LF)?;
        std::fs::write(&private_path, pem.as_bytes()).map_err(io_err(&private_path))?;
        set_permissions(&private_path, 0o600).map_err(io_err(&private_path))?;
        let public = key.public_key().to_openssh()?;
        std::fs::write(&public_path, format!("{public}\n")).map_err(io_err(&public_path))?;
        set_permissions(&public_path, 0o644).map_err(io_err(&public_path))?;
        tracing::info!(
            "generated new ed25519 keypair at {}",
            private_path.display()
        );
        public
    };

    Ok(KeyPair {
        private_path,
        public_path,
        public_key_openssh,
    })
}

/// Write the caller's private key (OpenSSH format, `-----BEGIN OPENSSH
/// PRIVATE KEY-----`, unencrypted) into `keys_dir` (created 0700 if absent)
/// as the same two files [`ensure_keypair`] uses — private 0600, public 0644
/// — and return the pair. Idempotent: an unchanged file is not rewritten, and
/// a replaced one is swapped in atomically (write + rename), so concurrent
/// runs on one host never see a partial key.
pub fn materialize_keypair(
    private_key_openssh: &str,
    keys_dir: &Path,
) -> Result<KeyPair, KeyError> {
    let key = PrivateKey::from_openssh(private_key_openssh.trim())?;
    if key.is_encrypted() {
        return Err(KeyError::Encrypted);
    }
    let pem = key.to_openssh(LineEnding::LF)?;
    let public = key.public_key().to_openssh()?;

    std::fs::create_dir_all(keys_dir).map_err(io_err(keys_dir))?;
    set_permissions(keys_dir, 0o700).map_err(io_err(keys_dir))?;
    let private_path = keys_dir.join(PRIVATE_KEY_FILE);
    let public_path = keys_dir.join(PUBLIC_KEY_FILE);
    write_if_changed(&private_path, pem.as_bytes(), 0o600)?;
    write_if_changed(&public_path, format!("{public}\n").as_bytes(), 0o644)?;
    Ok(KeyPair {
        private_path,
        public_path,
        public_key_openssh: public,
    })
}

/// Atomically replace `path` with `contents` (mode `mode` from creation)
/// unless it already holds exactly that.
fn write_if_changed(path: &Path, contents: &[u8], mode: u32) -> Result<(), KeyError> {
    if std::fs::read(path).ok().as_deref() == Some(contents) {
        return set_permissions(path, mode).map_err(io_err(path));
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&tmp).map_err(io_err(&tmp))?;
        file.write_all(contents).map_err(io_err(&tmp))?;
        file.sync_all().map_err(io_err(&tmp))?;
    }
    set_permissions(&tmp, mode).map_err(io_err(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io_err(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_key() -> String {
        PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519)
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap()
            .to_string()
    }

    #[test]
    fn materializes_a_supplied_key_privately_and_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let keys_dir = dir.path().join("keys");
        let pem = fresh_key();
        let pair = materialize_keypair(&pem, &keys_dir).unwrap();
        assert_eq!(pair.private_path, keys_dir.join(PRIVATE_KEY_FILE));
        assert!(pair.public_key_openssh.starts_with("ssh-ed25519 "));
        let written = std::fs::read_to_string(&pair.private_path).unwrap();
        assert_eq!(
            PrivateKey::from_openssh(&written)
                .unwrap()
                .public_key()
                .to_openssh()
                .unwrap(),
            pair.public_key_openssh
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&pair.private_path), 0o600);
            assert_eq!(mode(&keys_dir), 0o700);
        }
        // Same key again: same pair; the CLI's loader reads it back too.
        let again = materialize_keypair(&pem, &keys_dir).unwrap();
        assert_eq!(again.public_key_openssh, pair.public_key_openssh);
        let loaded = ensure_keypair(&keys_dir).unwrap();
        assert_eq!(loaded.public_key_openssh, pair.public_key_openssh);
        // A rotated key replaces the files.
        let rotated = materialize_keypair(&fresh_key(), &keys_dir).unwrap();
        assert_ne!(rotated.public_key_openssh, pair.public_key_openssh);
        let public = std::fs::read_to_string(&rotated.public_path).unwrap();
        assert_eq!(public.trim(), rotated.public_key_openssh);
    }

    #[test]
    fn rejects_what_is_not_an_openssh_private_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            materialize_keypair("ssh-ed25519 AAAA not-a-private-key", dir.path()),
            Err(KeyError::Key(_))
        ));
        assert!(!dir.path().join(PRIVATE_KEY_FILE).exists());
    }

    #[test]
    fn generates_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_keypair(dir.path()).unwrap();
        assert!(first.public_key_openssh.starts_with("ssh-ed25519 "));
        assert!(first.private_path.exists());
        assert!(first.public_path.exists());

        let second = ensure_keypair(dir.path()).unwrap();
        assert_eq!(first.public_key_openssh, second.public_key_openssh);
    }

    #[test]
    fn heals_missing_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure_keypair(dir.path()).unwrap();
        std::fs::remove_file(&first.public_path).unwrap();
        let second = ensure_keypair(dir.path()).unwrap();
        assert!(second.public_path.exists());
        assert_eq!(first.public_key_openssh, second.public_key_openssh);
    }
}
