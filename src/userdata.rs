//! Cloud-init user data: creates the login user with passwordless sudo and
//! injects the generated SSH public key.
//!
//! Delivering the key through cloud-init avoids cloud key-pair APIs entirely
//! (EC2 key-pair limits, `describe_key_pairs` permissions). Used by clouds
//! that boot with cloud-init (AWS, GCP); others take the key via their API
//! (`InstanceSpec::ssh_public_key`).
//!
//! A workload script (the k3s installer) can be appended with
//! [`with_boot_script`] so it runs during boot; whether it ran is verified
//! afterwards over SSH by the cluster machine (`cluster::k3s`), which falls
//! back to pushing the same script over SSH. Clouds without cloud-init only
//! ever take that SSH route.

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

/// Append a boot script to a document from [`build_cloud_init`]: written to
/// `path` (root-only) and run once by `runcmd` with `env` set.
pub fn with_boot_script(
    cloud_init: &str,
    path: &str,
    script: &str,
    env: &[(&str, &str)],
) -> String {
    // A `content: |` literal block: every non-empty line indented past the key.
    let indented = script
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
    let env_args: String = env
        .iter()
        .map(|(key, value)| format!("\"{key}={value}\", "))
        .collect();
    format!(
        "{cloud_init}write_files:
  - path: {path}
    permissions: '0700'
    owner: root:root
    content: |
{indented}
runcmd:
  - [env, {env_args}bash, {path}]
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_script_embeds_verbatim_and_stays_valid_yaml() {
        let script = "#!/usr/bin/env bash\nset -e\n\nif true; then\n  echo \"hi: there\"\nfi\n";
        let doc = with_boot_script(
            &build_cloud_init("ubuntu", "ssh-ed25519 AAA test"),
            "/opt/x.sh",
            script,
            &[("ORIGIN", "boot")],
        );
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap();
        assert!(parsed.get("users").is_some(), "user + key survive");
        let file = &parsed["write_files"][0];
        assert_eq!(file["path"], "/opt/x.sh");
        assert_eq!(file["content"].as_str().unwrap(), script);
        let cmd: Vec<&str> = parsed["runcmd"][0]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(cmd, ["env", "ORIGIN=boot", "bash", "/opt/x.sh"]);
    }

    #[test]
    fn contains_user_and_key_and_is_valid_yaml() {
        let doc = build_cloud_init("ubuntu", "ssh-ed25519 AAAAC3Nza test\n");
        assert!(doc.starts_with("#cloud-config"));
        assert!(doc.contains("name: ubuntu"));
        assert!(doc.contains("- ssh-ed25519 AAAAC3Nza test"));
        assert!(doc.contains("NOPASSWD:ALL"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap();
        assert!(parsed.get("users").is_some());
    }
}
