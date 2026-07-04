//! Cloud-init user data: injects the generated SSH public key and creates the
//! login user with passwordless sudo.
//!
//! Modeled on SkyPilot's `aws-ray.yml.j2` UserData block, which exists
//! precisely to avoid the EC2 key-pair APIs (key limits, `describe_key_pairs`
//! permissions) — the key travels in via cloud-init instead.

/// Build the `#cloud-config` document. `public_key_openssh` is a single-line
/// OpenSSH public key (`ssh-ed25519 AAAA...`).
pub fn build_cloud_init(ssh_user: &str, public_key_openssh: &str) -> String {
    format!(
        "#cloud-config
users:
  - name: {user}
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - {key}
",
        user = ssh_user,
        key = public_key_openssh.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_user_and_key() {
        let doc = build_cloud_init("ubuntu", "ssh-ed25519 AAAAC3Nza test\n");
        assert!(doc.starts_with("#cloud-config"));
        assert!(doc.contains("name: ubuntu"));
        assert!(doc.contains("- ssh-ed25519 AAAAC3Nza test"));
        assert!(doc.contains("NOPASSWD:ALL"));
        // Must stay valid YAML.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap();
        assert!(parsed.get("users").is_some());
    }
}
