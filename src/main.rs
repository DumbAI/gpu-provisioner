//! `gpuprov` — mini-SkyPilot CLI: provision an AWS VM (blocking, with region
//! failover) and SSH into it.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use gpu_provisioner::config::AppConfig;
use gpu_provisioner::db::{Db, VmState};
use gpu_provisioner::orchestrator;
use gpu_provisioner::provider::aws::AwsProvider;
use gpu_provisioner::provider::CloudProvider;
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
    /// GPU shorthand from the built-in catalog, e.g. A10G:1, T4:1, H100:8
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
        ports: args.ports,
        ssh_user: args.ssh_user,
    });

    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let provider = AwsProvider::new(&config.aws).await;
    let outcome =
        orchestrator::launch(&provider, config, &db, paths, &spec, args.retry_until_up).await?;

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
    Ok(())
}

/// SSH using the DB record: interactive session, or run a one-shot command
/// whose exit code is propagated.
async fn ssh(args: SshArgs, paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let record = db
        .get(&args.name)?
        .with_context(|| format!("no VM named '{}' — see `gpuprov list`", args.name))?;
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
    let ip = record
        .public_ip
        .context("record has no public IP; re-run `gpuprov provision`")?;
    let user = record.ssh_user.unwrap_or_else(|| "ubuntu".to_string());
    let key = record
        .ssh_key_path
        .map(PathBuf::from)
        .context("record has no SSH key path; re-run `gpuprov provision`")?;
    let opts = SshOptions::new(key).control_dir(Some(paths.sockets_dir()));

    if args.command.is_empty() {
        // Replaces this process with ssh; only returns on spawn failure.
        let err = runner::exec_interactive(&user, &ip, &opts);
        bail!("failed to exec ssh: {err}");
    }
    let code = runner::run_command(&user, &ip, &opts, &args.command).await?;
    std::process::exit(code);
}

fn list(paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let records = db.list()?;
    if records.is_empty() {
        println!("No VMs yet. Provision one with: gpuprov provision --name <name> --instance-type t3.micro --region us-west-2");
        return Ok(());
    }
    println!(
        "{:<20} {:<13} {:<12} {:<12} {:<21} {:<16}",
        "NAME", "STATE", "REGION", "ZONE", "INSTANCE_ID", "PUBLIC_IP"
    );
    for r in records {
        println!(
            "{:<20} {:<13} {:<12} {:<12} {:<21} {:<16}",
            r.name,
            r.state.as_str(),
            r.region.as_deref().unwrap_or("-"),
            r.zone.as_deref().unwrap_or("-"),
            r.instance_id.as_deref().unwrap_or("-"),
            r.public_ip.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

async fn down(args: DownArgs, config: &AppConfig, paths: &Paths) -> Result<()> {
    let db = Db::open(&paths.db_path()).context("cannot open state database")?;
    let record = db
        .get(&args.name)?
        .with_context(|| format!("no VM named '{}' — see `gpuprov list`", args.name))?;

    match record.region.as_deref() {
        Some(region) => {
            println!("Terminating VM '{}' in {region} ...", args.name);
            let provider = AwsProvider::new(&config.aws).await;
            provider.terminate(region, &args.name).await?;
        }
        None => println!(
            "VM '{}' has no region recorded (never provisioned); removing local state only.",
            args.name
        ),
    }
    db.set_state(&args.name, VmState::Terminated)?;
    config_file::remove_vm_host_entry(&paths.ssh_dir(), &args.name)?;
    println!("VM '{}' terminated.", args.name);
    Ok(())
}
