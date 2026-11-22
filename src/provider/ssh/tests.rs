use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

use super::*;
use crate::provider::ErrorKind;

fn host(gpus: Option<&str>) -> SshHostConfig {
    SshHostConfig {
        host: "10.0.0.5".into(),
        user: "ryan".into(),
        port: 2222,
        identity_file: "~/.ssh/id_ed25519".into(),
        gpus: gpus.map(str::to_string),
        keep_k3s: false,
    }
}

fn config() -> SshConfig {
    SshConfig {
        hosts: BTreeMap::from([
            ("spark".to_string(), host(Some("GB10:1"))),
            ("lab".to_string(), host(None)),
        ]),
    }
}

fn request(ask: ResourceAsk, regions: &[&str]) -> ResourceRequest {
    ResourceRequest {
        ask,
        regions: regions.iter().map(|r| r.to_string()).collect(),
        zone: None,
        spot: false,
    }
}

fn gpus(s: &str) -> ResourceAsk {
    ResourceAsk::Gpus(s.parse().unwrap())
}

fn regions(offers: &[Offer]) -> Vec<(&str, &str)> {
    offers
        .iter()
        .map(|o| (o.region.as_str(), o.instance_type.as_str()))
        .collect()
}

#[test]
fn plans_the_hosts_that_fit_the_ask() {
    let c = config();
    let o = plan_hosts(&c, &request(gpus("GB10:1"), &[])).unwrap();
    assert_eq!(regions(&o), [("spark", "GB10:1")]);
    // A host by id, or by its type.
    let o = plan_hosts(&c, &request(ResourceAsk::InstanceType("lab".into()), &[])).unwrap();
    assert_eq!(regions(&o), [("lab", "cpu")]);
    let o = plan_hosts(&c, &request(ResourceAsk::InstanceType("cpu".into()), &[])).unwrap();
    assert_eq!(regions(&o), [("lab", "cpu")]);
    // --region names hosts.
    let o = plan_hosts(
        &c,
        &request(ResourceAsk::InstanceType("GB10:1".into()), &["spark"]),
    )
    .unwrap();
    assert_eq!(regions(&o), [("spark", "GB10:1")]);
}

#[test]
fn refuses_what_no_host_offers() {
    let c = config();
    for (req, says) in [
        (request(gpus("H100:1"), &[]), "no ssh host offers H100"),
        (request(gpus("GB10:2"), &[]), "no ssh host offers GB10"),
        (request(gpus("GB10:1"), &["nope"]), "no ssh host 'nope'"),
        (request(gpus("GB10:1"), &["lab"]), "no ssh host offers"),
    ] {
        let err = plan_hosts(&c, &req).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest, "{err}");
        assert!(err.to_string().contains(says), "{err}");
    }
    let err = plan_hosts(&SshConfig::default(), &request(gpus("GB10:1"), &[])).unwrap_err();
    assert!(err.to_string().contains("no ssh hosts configured"), "{err}");
    let mut zoned = request(gpus("GB10:1"), &[]);
    zoned.zone = Some("a".into());
    assert!(plan_hosts(&c, &zoned).is_err());
}

fn output(code: i32, stderr: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn script_outcomes_drive_failover() {
    let kind = |step, code, stderr| {
        classify(step, "spark", &output(code, stderr))
            .unwrap_err()
            .kind
    };
    assert!(classify(Step::Claim, "spark", &output(0, "")).is_ok());
    assert_eq!(
        kind(Step::Claim, EXIT_CLAIMED, "the host serves VM 'b'"),
        ErrorKind::Capacity
    );
    assert_eq!(
        kind(Step::Claim, EXIT_NO_SUDO, ""),
        ErrorKind::InvalidRequest
    );
    assert_eq!(
        kind(Step::Claim, EXIT_FOREIGN_K3S, ""),
        ErrorKind::InvalidRequest
    );
    assert_eq!(
        kind(
            Step::Claim,
            EXIT_SSH,
            "ryan@10.0.0.5: Permission denied (publickey)."
        ),
        ErrorKind::Credentials
    );
    // Unreachable: nothing was claimed, so another host may serve; once
    // claimed, reaching it again is worth retrying.
    assert_eq!(
        kind(Step::Claim, EXIT_SSH, "Connection timed out"),
        ErrorKind::Unavailable
    );
    assert_eq!(
        kind(Step::Observe, EXIT_SSH, "Connection timed out"),
        ErrorKind::Transient
    );
    assert_eq!(
        kind(Step::Release, EXIT_SSH, "No route to host"),
        ErrorKind::Transient
    );
    assert_eq!(kind(Step::Release, 1, "boom"), ErrorKind::Failed);
}

#[test]
fn embedded_scripts_parse_as_bash() {
    for (name, script) in [("claim", CLAIM), ("observe", OBSERVE), ("release", RELEASE)] {
        let out = std::process::Command::new("bash")
            .args(["-n", "-c", script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn home_is_expanded() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(expand_home("~/.ssh/k"), PathBuf::from(home).join(".ssh/k"));
    assert_eq!(expand_home("/k"), PathBuf::from("/k"));
}

// ------------------------------------------------- the scripts, run locally

/// Runs the scripts with this machine's bash, as if it were the host:
/// `HOME` and the state directory are temporary, and `sudo` / `systemctl`
/// are shims.
struct LocalShell {
    home: PathBuf,
    bin: PathBuf,
}

#[async_trait]
impl HostShell for LocalShell {
    async fn run(
        &self,
        _host: &SshHostConfig,
        script: &str,
        args: &[String],
    ) -> std::io::Result<Output> {
        use tokio::io::AsyncWriteExt;
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        let mut child = tokio::process::Command::new("bash")
            .arg("-s")
            .arg("--")
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await
    }
}

fn shim(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Host {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    provider: SshProvider,
}

/// A "host" whose user has passwordless sudo, or not.
fn local_host(sudo: bool) -> Host {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let bin = tmp.path().join("bin");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    if sudo {
        shim(&bin, "sudo", r#"[ "$1" = -n ] && shift; exec "$@""#);
    } else {
        shim(&bin, "sudo", "exit 1");
    }
    shim(&bin, "systemctl", "exit 0");
    let provider = SshProvider::with_shell(
        &config(),
        Box::new(LocalShell {
            home: home.clone(),
            bin,
        }),
        state.to_str().unwrap(),
    );
    Host {
        _tmp: tmp,
        home,
        state,
        provider,
    }
}

fn spec(vm: &str, key: &str) -> InstanceSpec {
    InstanceSpec {
        vm_name: vm.into(),
        image_id: None,
        disk_gb: 100,
        spot: false,
        open_ports: Vec::new(),
        user_data: String::new(),
        ssh_public_key: format!("{key}\n"),
        ssh_user: "ubuntu".into(),
        boot_script: None,
    }
}

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGpr gpuprov";
const MINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMine ryan@laptop";

fn spark() -> (Offer, Location) {
    let offer = Offer::new("GB10:1", "spark");
    let at = Location {
        region: "spark".into(),
        zone: None,
    };
    (offer, at)
}

fn authorized(h: &Host) -> String {
    std::fs::read_to_string(h.home.join(".ssh/authorized_keys")).unwrap_or_default()
}

#[tokio::test]
async fn a_host_is_claimed_served_and_given_back() {
    let h = local_host(true);
    let (offer, at) = spark();
    let p = &h.provider;
    std::fs::create_dir_all(h.home.join(".ssh")).unwrap();
    std::fs::write(h.home.join(".ssh/authorized_keys"), format!("{MINE}\n")).unwrap();

    assert!(p.observe(&at, "a").await.unwrap().is_empty(), "unclaimed");
    let id = p
        .create(&offer, &serde_json::Value::Null, &spec("a", KEY))
        .await
        .unwrap();
    assert_eq!(id, "spark");
    // Claiming again for the same VM converges (a retried create).
    p.create(&offer, &serde_json::Value::Null, &spec("a", KEY))
        .await
        .unwrap();
    assert_eq!(
        authorized(&h),
        format!("{MINE}\n{KEY}\n"),
        "gpuprov's key once"
    );

    let seen = p.observe(&at, "a").await.unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].is_ready());
    assert_eq!(
        seen[0].endpoint,
        Some(SshEndpoint {
            host: "10.0.0.5".into(),
            port: 2222,
            user: Some("ryan".into()),
        })
    );
    assert!(p.observe(&at, "b").await.unwrap().is_empty(), "not b's");

    // One VM per host: another is refused as capacity (try the next host).
    let err = p
        .create(&offer, &serde_json::Value::Null, &spec("b", KEY))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Capacity, "{err}");
    // Releasing for another VM touches nothing.
    p.delete(&at, "b", &[]).await.unwrap();
    assert_eq!(p.observe(&at, "a").await.unwrap().len(), 1);

    p.delete(&at, "a", &[]).await.unwrap();
    assert!(p.observe(&at, "a").await.unwrap().is_empty());
    assert!(!h.state.join("ssh-claim").exists());
    assert_eq!(
        authorized(&h),
        format!("{MINE}\n"),
        "only gpuprov's key went"
    );
    // Released twice: nothing to do.
    p.delete(&at, "a", &[]).await.unwrap();
    // Free again.
    p.create(&offer, &serde_json::Value::Null, &spec("b", KEY))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_key_that_was_already_authorized_stays() {
    let h = local_host(true);
    let (offer, at) = spark();
    std::fs::create_dir_all(h.home.join(".ssh")).unwrap();
    std::fs::write(h.home.join(".ssh/authorized_keys"), format!("{KEY}\n")).unwrap();
    h.provider
        .create(&offer, &serde_json::Value::Null, &spec("a", KEY))
        .await
        .unwrap();
    h.provider.delete(&at, "a", &[]).await.unwrap();
    assert_eq!(authorized(&h), format!("{KEY}\n"));
}

#[tokio::test]
async fn a_host_without_passwordless_sudo_is_refused_and_left_alone() {
    let h = local_host(false);
    let (offer, _) = spark();
    let err = h
        .provider
        .create(&offer, &serde_json::Value::Null, &spec("a", KEY))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest, "{err}");
    assert!(err.to_string().contains("passwordless sudo"), "{err}");
    assert!(!h.state.join("ssh-claim").exists());
    assert_eq!(authorized(&h), "");
}

#[tokio::test]
async fn an_unknown_host_is_an_invalid_request() {
    let h = local_host(true);
    let at = Location {
        region: "gone".into(),
        zone: None,
    };
    let err = h.provider.observe(&at, "a").await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest, "{err}");
}
