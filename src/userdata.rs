//! Cloud-init user data: creates the login user with passwordless sudo and
//! injects the generated SSH public key.
//!
//! Delivering the key through cloud-init avoids cloud key-pair APIs entirely
//! (EC2 key-pair limits, `describe_key_pairs` permissions). Used by clouds
//! that boot with cloud-init (AWS, GCP); others take the key via their API
//! (`InstanceSpec::ssh_public_key`).
//!
//! [`render`] produces the document a cloud-init cloud should boot with:
//! the login user, plus the spec's `boot_script` (the k3s installer) as a
//! file run once by `runcmd`. Clouds without cloud-init deliver the boot
//! script their own way (`BootScript::to_shell`) or not at all.

use crate::provider::InstanceSpec;

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

/// Where cloud-init writes the boot script before running it.
pub const BOOT_SCRIPT_PATH: &str = "/opt/gpuprov/boot.sh";

/// The cloud-init user data for `spec`: its login document, plus its boot
/// script when one is set.
pub fn render(spec: &InstanceSpec) -> String {
    match &spec.boot_script {
        None => spec.user_data.clone(),
        Some(boot) => {
            let env: Vec<(&str, &str)> = boot
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            with_boot_script(&spec.user_data, BOOT_SCRIPT_PATH, &boot.script, &env)
        }
    }
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
    fn render_adds_the_boot_script_only_when_set() {
        let mut spec = InstanceSpec {
            vm_name: "vm1".into(),
            image_id: None,
            disk_gb: 100,
            spot: false,
            open_ports: Vec::new(),
            user_data: build_cloud_init("ubuntu", "ssh-ed25519 AAA test"),
            ssh_public_key: "ssh-ed25519 AAA test".into(),
            ssh_user: "ubuntu".into(),
            boot_script: None,
        };
        assert_eq!(render(&spec), spec.user_data);
        spec.boot_script = Some(crate::provider::BootScript {
            script: "#!/usr/bin/env bash\necho hi\n".into(),
            env: vec![("K".into(), "V".into())],
        });
        let parsed: serde_yaml::Value = serde_yaml::from_str(&render(&spec)).unwrap();
        assert_eq!(parsed["write_files"][0]["path"], BOOT_SCRIPT_PATH);
        assert_eq!(parsed["runcmd"][0][1], "K=V");
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
