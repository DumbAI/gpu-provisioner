//! Cloud-init user data: injects the generated SSH public key and creates the
//! login user with passwordless sudo, and — for the k3s workload — embeds a
//! boot-time install script.
//!
//! Modeled on SkyPilot's `aws-ray.yml.j2` UserData block, which exists
//! precisely to avoid the EC2 key-pair APIs (key limits, `describe_key_pairs`
//! permissions) — the key travels in via cloud-init instead.
//!
//! This module only knows *how* to render cloud-init, not *what* a given
//! workload installs: the k3s recipe is supplied by the caller (the planner
//! passes [`crate::kube::K3S_INSTALL_SCRIPT`]), keeping the provisioning core
//! orchestrator-agnostic.

/// What the VM should become at boot.
pub enum Workload<'a> {
    /// A bare VM: just the login user + SSH key (the original behavior).
    Bare,
    /// A single-node k3s cluster: additionally write and run `install_script`.
    K3s { install_script: &'a str },
}

/// Build the `#cloud-config` document. `public_key_openssh` is a single-line
/// OpenSSH public key (`ssh-ed25519 AAAA...`).
pub fn build_cloud_init(ssh_user: &str, public_key_openssh: &str, workload: &Workload) -> String {
    let base = format!(
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
    );

    match workload {
        Workload::Bare => base,
        Workload::K3s { install_script } => {
            // Embed the script as a `content: |` literal block scalar; every
            // non-empty line is indented past the `content:` key.
            let indented = install_script
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        String::new()
                    } else {
                        format!("      {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "{base}write_files:
  - path: /opt/gpuprov/install-k3s.sh
    permissions: '0755'
    owner: root:root
    content: |
{indented}
runcmd:
  - [bash, /opt/gpuprov/install-k3s.sh]
"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_contains_user_and_key() {
        let doc = build_cloud_init("ubuntu", "ssh-ed25519 AAAAC3Nza test\n", &Workload::Bare);
        assert!(doc.starts_with("#cloud-config"));
        assert!(doc.contains("name: ubuntu"));
        assert!(doc.contains("- ssh-ed25519 AAAAC3Nza test"));
        assert!(doc.contains("NOPASSWD:ALL"));
        // Bare has no k3s scaffolding.
        assert!(!doc.contains("write_files"));
        // Must stay valid YAML.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap();
        assert!(parsed.get("users").is_some());
    }

    #[test]
    fn k3s_embeds_script_preserving_indentation() {
        let script = "#!/usr/bin/env bash\nset -e\n\nif true; then\n  echo hi\nfi\n";
        let doc = build_cloud_init(
            "ubuntu",
            "ssh-ed25519 AAA test",
            &Workload::K3s {
                install_script: script,
            },
        );
        assert!(doc.starts_with("#cloud-config"));
        assert!(doc.contains("/opt/gpuprov/install-k3s.sh"));

        // Valid YAML, and the script round-trips as the file content with its
        // own internal indentation intact.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap();
        assert!(parsed.get("runcmd").is_some());
        let content = parsed["write_files"][0]["content"].as_str().unwrap();
        assert!(content.starts_with("#!/usr/bin/env bash"));
        assert!(content.contains("\n  echo hi\n"));
    }
}
