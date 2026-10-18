//! Local SSH identity: a single ed25519 keypair generated once and reused for
//! every VM (ed25519: small, fast, and the modern default).

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

#[cfg(test)]
mod tests {
    use super::*;

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
