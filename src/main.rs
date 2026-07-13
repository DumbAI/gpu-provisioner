//! `gpuprov` — mini-SkyPilot CLI: provision an AWS VM (blocking, with region
//! failover) and SSH into it.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use gpu_provisioner::config::{AppConfig, DEFAULT_SSH_USER};
use gpu_provisioner::db::{Db, VmRecord, VmState};
use gpu_provisioner::kube;
use gpu_provisioner::orchestrator;
use gpu_provisioner::provider;
use gpu_provisioner::spec::{CliOverrides, VmSpec};
use gpu_provisioner::ssh::config_file;
use gpu_provisioner::ssh::options::SshOptions;
use gpu_provisioner::ssh::runner;
use gpu_provisioner::util::Paths;

#[derive(Parser)]
#[command(
    name = "gpuprov",
    version,
    about = "Provision cloud VMs (AWS) and SSH into them — a mini-SkyPilot"
)]
struct Cli {
    /// Config file (default: ~/.gpu-provisioner/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Provision a VM: blocks until it is SSH-ready (region failover included)
    Provision(ProvisionArgs),
    /// SSH into a VM (interactive), or run a command on it
    Ssh(SshArgs),
    /// List VM records from the local database
    List,
    /// Fetch/refresh the local kubeconfig for a k3s VM (provisioned with --k8s)
    Kubeconfig(KubeconfigArgs),
    /// Terminate a VM and clean up its cloud resources and local state
    Down(DownArgs),
}

#[derive(Args)]
struct ProvisionArgs {
    /// VM name (required here or in the spec file)
    #[arg(long)]
    name: Option<String>,
    /// YAML spec file; flags below override its fields
    #[arg(long, value_name = "FILE")]
    spec: Option<PathBuf>,
    /// EC2 instance type, e.g. t3.micro / g5.xlarge
    #[arg(long, conflicts_with = "gpus")]
    instance_type: Option<String>,
    /// GPU shorthand from the built-in catalog, e.g. A10G:1, T4:1, L40S:1, H100:8
    #[arg(long)]
    gpus: Option<String>,
    /// Candidate region in failover order (repeatable)
    #[arg(long = "region", value_name = "REGION")]
    regions: Vec<String>,
    /// Pin to one availability zone (e.g. us-west-2a)
    #[arg(long)]
    zone: Option<String>,
    /// AMI override (default: resolved per region via SSM public parameters)
    #[arg(long)]
    ami: Option<String>,
    /// Root disk size in GB (default from config, else 100)
    #[arg(long, value_name = "GB")]
    disk: Option<i32>,
    /// Request a spot instance
    #[arg(long)]
    spot: bool,
    /// Install a single-node k3s cluster and open the Kubernetes API + ingress
    /// ports (6443/80/443); writes a local kubeconfig when the VM is up
    #[arg(long = "k8s")]
    kubernetes: bool,
    /// Extra ingress port to open besides SSH (repeatable)
    #[arg(long = "port", value_name = "PORT")]
    ports: Vec<u16>,
    /// SSH login user (default from config, else "ubuntu")
    #[arg(long)]
    ssh_user: Option<String>,
    /// Keep retrying full failover rounds (30s apart) until the VM is up
    #[arg(long)]
    retry_until_up: bool,
}

#[derive(Args)]
struct SshArgs {
    /// VM name (see `gpuprov list`)
    name: String,
    /// Command to run remotely (after `--`); interactive shell if omitted
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct KubeconfigArgs {
    /// VM name (see `gpuprov list`)
    name: String,
}

#[derive(Args)]
struct DownArgs {
    /// VM name
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let paths = Paths::resolve().context("cannot resolve gpuprov home directory")?;
    paths
        .ensure_dirs()
        .context("cannot create state directories")?;
    let config = AppConfig::load(cli.config.as_deref(), &paths.default_config_path())?;

    match cli.command {
        Command::Provision(args) => provision(args, &config, &paths).await,
        Command::Ssh(args) => ssh(args, &paths).await,
        Command::List => list(&paths),
        Command::Kubeconfig(args) => kubeconfig(args, &paths).await,
        Command::Down(args) => down(args, &config, &paths).await,
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

/// The long-running, blocking provisioning operation. Ends with a `UP` record
/// in the local DB (IP + SSH config) or a non-zero exit.
async fn provision(args: ProvisionArgs, config: &AppConfig, paths: &Paths) -> Result<()> {
    let base = match &args.spec {
        Some(path) => VmSpec::from_yaml_file(path)?,
        None => VmSpec::default(),
    };
    let spec = base.merged_with(CliOverrides {
        name: args.name,
        instance_type: args.instance_type,
        gpus: args.gpus,
        regions: args.regions,
        zone: args.zone,
        ami: args.ami,
        disk_gb: args.disk,
        spot: args.spot,
        kubernetes: args.kubernetes,
        ports: args.ports,
        ssh_user: args.ssh_user,
    });

    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let cloud = provider::build(provider::DEFAULT_PROVIDER, config).await?;
    let outcome =
        orchestrator::launch(&*cloud, config, &db, paths, &spec, args.retry_until_up).await?;

    println!();
    println!(
        "VM '{}' is UP{}",
        outcome.name,
        if outcome.reused {
            " (reused existing instance)"
        } else {
            ""
        }
    );
    println!("  Public IP:   {}", outcome.public_ip);
    println!(
        "  Instance:    {}  ({} / {})",
        outcome.instance_id, outcome.region, outcome.zone
    );
    println!("  Login:       {}@{}", outcome.ssh_user, outcome.public_ip);
    println!(
        "  Connect:     gpuprov ssh {}    (or plain: ssh {})",
        outcome.name, outcome.name
    );
    println!("  SSH config:  {}", outcome.ssh_config_path.display());
    println!("  Record:      {}", paths.db_path().display());

    if spec.kubernetes {
        finalize_kubernetes(&outcome, paths).await?;
    }
    Ok(())
}

/// After the VM is SSH-ready, wait for the k3s API and write a local kubeconfig.
async fn finalize_kubernetes(outcome: &orchestrator::LaunchOutcome, paths: &Paths) -> Result<()> {
    println!();
    println!(
        "Waiting for k3s to come up (first boot installs it; this can take a few minutes) ..."
    );
    let setup = kube::setup(
        &outcome.name,
        &outcome.public_ip,
        &outcome.ssh_user,
        &outcome.ssh_key_path,
        paths,
        kube::DEFAULT_READY_TIMEOUT,
        outcome.reused,
    )
    .await?;
    print_kube_instructions(&setup);
    Ok(())
}

/// Print how to talk to the freshly-provisioned cluster.
fn print_kube_instructions(setup: &kube::KubeSetup) {
    println!();
    println!("Kubernetes cluster '{}' is ready.", setup.context);
    println!("  API:         {}", setup.api_endpoint);
    println!("  Kubeconfig:  {}", setup.kubeconfig_path.display());
    println!();
    println!("  export KUBECONFIG={}", setup.kubeconfig_path.display());
    println!("  kubectl get nodes");
    println!("  kubectl apply -f examples/nginx.yaml      # deploy a container app");
    println!("  kubectl apply -f examples/gpu-test.yaml   # GPU smoke test (GPU nodes)");
    println!();
    println!(
        "  NOTE: the API (6443) and ingress (80/443) are open to 0.0.0.0/0; access is gated by"
    );
    println!("        the client cert inside the kubeconfig — treat that file as a secret.");
}

/// SSH using the DB record: interactive session, or run a one-shot command
/// whose exit code is propagated.
async fn ssh(args: SshArgs, paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let record = load_record(&db, &args.name)?;
    if record.state != VmState::Up {
        bail!(
            "VM '{}' is {}, not UP{}",
            args.name,
            record.state,
            if record.state == VmState::Failed {
                " — re-run `gpuprov provision`"
            } else {
                ""
            }
        );
    }
    let (ip, user, key) = connection(&record)?;
    let opts = SshOptions::new(key).control_dir(Some(paths.sockets_dir()));

    if args.command.is_empty() {
        // Replaces this process with ssh; only returns on spawn failure.
        let err = runner::exec_interactive(&user, &ip, &opts);
        bail!("failed to exec ssh: {err}");
    }
    let code = runner::run_command(&user, &ip, &opts, &args.command).await?;
    std::process::exit(code);
}

/// Look up a VM record by name, with a CLI-friendly not-found message.
fn load_record(db: &Db, name: &str) -> Result<VmRecord> {
    db.get(name)?
        .with_context(|| format!("no VM named '{name}' — see `gpuprov list`"))
}

/// Extract the SSH connection tuple `(public_ip, user, key_path)` from a record.
fn connection(record: &VmRecord) -> Result<(String, String, PathBuf)> {
    let ip = record
        .public_ip
        .clone()
        .context("record has no public IP; re-run `gpuprov provision`")?;
    let user = record
        .ssh_user
        .clone()
        .unwrap_or_else(|| DEFAULT_SSH_USER.to_string());
    let key = record
        .ssh_key_path
        .clone()
        .map(PathBuf::from)
        .context("record has no SSH key path; re-run `gpuprov provision`")?;
    Ok((ip, user, key))
}

/// Fetch/refresh the local kubeconfig for a k3s VM (idempotent).
async fn kubeconfig(args: KubeconfigArgs, paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let record = load_record(&db, &args.name)?;
    if record.state != VmState::Up {
        bail!("VM '{}' is {}, not UP", args.name, record.state);
    }
    let spec: VmSpec = serde_json::from_str(&record.spec_json)
        .with_context(|| format!("cannot parse stored spec for VM '{}'", args.name))?;
    if !spec.kubernetes {
        bail!(
            "VM '{}' was not provisioned with --k8s; there is no cluster to connect to",
            args.name
        );
    }
    let (ip, user, key) = connection(&record)?;
    // The instance already exists (cloud-init won't re-run), so fail fast if
    // k3s is somehow absent rather than waiting out the full timeout.
    let setup = kube::setup(
        &args.name,
        &ip,
        &user,
        &key,
        paths,
        kube::DEFAULT_READY_TIMEOUT,
        true,
    )
    .await?;
    print_kube_instructions(&setup);
    Ok(())
}

fn list(paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let records = db.list()?;
    if records.is_empty() {
        println!("No VMs yet. Provision one with: gpuprov provision --name <name> --instance-type t3.micro --region us-west-2");
        return Ok(());
    }
    println!(
        "{:<20} {:<13} {:<9} {:<12} {:<12} {:<15} {:<21} {:<16}",
        "NAME", "STATE", "PROVIDER", "REGION", "ZONE", "TYPE", "INSTANCE_ID", "PUBLIC_IP"
    );
    for r in records {
        println!(
            "{:<20} {:<13} {:<9} {:<12} {:<12} {:<15} {:<21} {:<16}",
            r.name,
            r.state.as_str(),
            r.provider.as_deref().unwrap_or("-"),
            r.region.as_deref().unwrap_or("-"),
            r.zone.as_deref().unwrap_or("-"),
            r.instance_type.as_deref().unwrap_or("-"),
            r.instance_id.as_deref().unwrap_or("-"),
            r.public_ip.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

async fn down(args: DownArgs, config: &AppConfig, paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let record = load_record(&db, &args.name)?;

    match record.region.as_deref() {
        Some(region) => {
            // Dispatch teardown to the adapter that provisioned the VM
            // (pre-provider-column records were all the default cloud).
            let provider_name = record
                .provider
                .as_deref()
                .unwrap_or(provider::DEFAULT_PROVIDER);
            println!(
                "Terminating VM '{}' in {region} ({provider_name}) ...",
                args.name
            );
            let cloud = provider::build(provider_name, config)
                .await
                .with_context(|| {
                    format!(
                        "VM '{}' was provisioned by '{provider_name}', unsupported by this build",
                        args.name
                    )
                })?;
            cloud.terminate(region, &args.name).await?;
        }
        None => println!(
            "VM '{}' has no region recorded (never provisioned); removing local state only.",
            args.name
        ),
    }
    db.set_state(&args.name, VmState::Terminated)?;
    config_file::remove_vm_host_entry(&paths.ssh_dir(), &args.name)?;
    kube::remove_kubeconfig(paths, &args.name)?;
    println!("VM '{}' terminated.", args.name);
    Ok(())
}
