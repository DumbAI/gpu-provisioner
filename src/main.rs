//! `gpuprov` CLI: provision a GPU VM on AWS, GCP or Prime
//! Intellect (or a local Docker container) with failover, SSH into it, and
//! optionally turn it into a single-node k3s cluster.
//!
//! Every cloud-touching command runs a durable state machine (`machine`,
//! `vm`, `cluster`): Ctrl-C compensates or leaves a resumable journal, and
//! re-running the same command resumes it.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use gpu_provisioner::cluster::k3s::{K3sMachine, K3sState};
use gpu_provisioner::config::{AppConfig, DEFAULT_SSH_USER};
use gpu_provisioner::db::{Db, VmRecord, VmState};
use gpu_provisioner::kube::{self, AccessMode, KubeAccess};
use gpu_provisioner::machine::{Cancel, Executor};
use gpu_provisioner::planner;
use gpu_provisioner::provider;
use gpu_provisioner::spec::{CliOverrides, VmSpec};
use gpu_provisioner::ssh::options::SshOptions;
use gpu_provisioner::ssh::{config_file, keys, runner, wait};
use gpu_provisioner::util::{self, Paths};
use gpu_provisioner::vm::launch::{LaunchMachine, LaunchState, ReadyVm, SshConnector};
use gpu_provisioner::vm::teardown::{TeardownMachine, TeardownState};

#[derive(Parser)]
#[command(
    name = "gpuprov",
    version,
    about = "Provision GPU VMs across clouds (AWS, GCP, Lambda, Hyperstack, Massed Compute, Verda, RunPod, Prime Intellect) with failover, durable \
             state machines and single-node k3s clusters"
)]
struct Cli {
    /// Config file (default: ~/.gpu-provisioner/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

// Built once per process; boxing the large variant would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Provision a VM: blocks until it is SSH-ready (failover included);
    /// re-running resumes an interrupted provision
    Provision(ProvisionArgs),
    /// SSH into a VM (interactive), or run a command on it
    Ssh(SshArgs),
    /// List VM records from the local database
    List,
    /// Show a VM's record and its recent state-machine transitions
    Status(NameArg),
    /// Fetch/refresh the local kubeconfig for a k3s VM (provisioned with --k8s)
    Kubeconfig(NameArg),
    /// Forward the k3s API of a tunnel-mode cluster to 127.0.0.1:16443
    Tunnel(NameArg),
    /// Terminate a VM and clean up its cloud resources and local state
    Down(NameArg),
}

#[derive(Args)]
struct ProvisionArgs {
    /// VM name (required here or in the spec file)
    #[arg(long)]
    name: Option<String>,
    /// YAML spec file; flags below override its fields
    #[arg(long, value_name = "FILE")]
    spec: Option<PathBuf>,
    /// Cloud: aws (default), gcp, massedcompute, hyperstack, lambda, verda,
    /// primeintellect, runpod, docker
    #[arg(long)]
    cloud: Option<String>,
    /// Exact instance type, e.g. g5.xlarge (AWS), g2-standard-8 (GCP),
    /// hyperstack/n3-H100x1 (Prime Intellect), "NVIDIA H100 80GB HBM3" (RunPod), 1H100.80S.22V (Verda), gpu_1x_a6000 (Lambda),
    /// container (docker)
    #[arg(long, conflicts_with = "gpus")]
    instance_type: Option<String>,
    /// GPU shorthand resolved by the cloud's catalog, e.g. A10G:1, L4:1, H100:8
    #[arg(long)]
    gpus: Option<String>,
    /// Candidate region in failover order (repeatable)
    #[arg(long = "region", value_name = "REGION")]
    regions: Vec<String>,
    /// Pin to one zone / data center (e.g. us-west-2a)
    #[arg(long)]
    zone: Option<String>,
    /// Machine-image override (AMI on AWS, image URL on GCP)
    #[arg(long, alias = "ami")]
    image: Option<String>,
    /// Root disk size in GB (default from config, else 100)
    #[arg(long, value_name = "GB")]
    disk: Option<i32>,
    /// Request spot / preemptible capacity
    #[arg(long)]
    spot: bool,
    /// Require a single-node k3s cluster (the default, unless disabled in the
    /// config; fails on clouds that cannot host k3s)
    #[arg(long = "k8s", conflicts_with = "no_kubernetes")]
    kubernetes: bool,
    /// Plain VM: skip the k3s cluster
    #[arg(long = "no-k8s")]
    no_kubernetes: bool,
    /// Extra ingress port to open besides SSH (repeatable)
    #[arg(long = "port", value_name = "PORT")]
    ports: Vec<u16>,
    /// SSH login user (default from config, else "ubuntu"; some clouds dictate it)
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
struct NameArg {
    /// VM name (see `gpuprov list`)
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
        Command::Status(args) => status(&args.name, &paths),
        Command::Kubeconfig(args) => kubeconfig(&args.name, &config, &paths).await,
        Command::Tunnel(args) => tunnel(&args.name, &paths),
        Command::Down(args) => down(&args.name, &config, &paths).await,
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

/// First Ctrl-C: cancel cooperatively (machines compensate at the next step
/// boundary). Second Ctrl-C: exit now, leaving the journal resumable.
fn install_ctrl_c(cancel: &Cancel) {
    let cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "\ninterrupt: finishing the current step, then cleaning up (Ctrl-C again to exit \
                 immediately; re-run the command to resume)"
            );
            cancel.cancel();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
}

fn open_db(paths: &Paths) -> Result<Db> {
    Db::open(&paths.db_path()).context("cannot open state database")
}

/// The long-running, blocking provisioning operation. Ends with an `UP`
/// record (and, unless k3s is off or unsupported, a validated cluster) or a
/// non-zero exit.
async fn provision(args: ProvisionArgs, config: &AppConfig, paths: &Paths) -> Result<()> {
    let base = match &args.spec {
        Some(path) => VmSpec::from_yaml_file(path)?,
        None => VmSpec::default(),
    };
    let mut spec = base.merged_with(CliOverrides {
        name: args.name,
        cloud: args.cloud,
        instance_type: args.instance_type,
        gpus: args.gpus,
        regions: args.regions,
        zone: args.zone,
        ami: args.image,
        disk_gb: args.disk,
        spot: args.spot,
        kubernetes: match (args.kubernetes, args.no_kubernetes) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        ports: args.ports,
        ssh_user: args.ssh_user,
    });

    let cloud_id = spec.cloud_id();
    let cloud = provider::build(&cloud_id, config).await?;
    let db = open_db(paths)?;
    let keypair = keys::ensure_keypair(&paths.keys_dir())?;
    let plan = planner::prepare(
        &spec,
        &config.defaults,
        config.default_regions(&cloud_id),
        &keypair.public_key_openssh,
    )?;
    let vm_name = plan.spec.vm_name.clone();
    let kubernetes = plan.kubernetes_for(&cloud.capabilities());
    if plan.kubernetes && !kubernetes {
        eprintln!(
            "note: {cloud_id} cannot host k3s; provisioning a plain VM (pass --no-k8s to silence)"
        );
    }
    // Record the resolved choice: `gpuprov kubeconfig` reads it back.
    spec.kubernetes = Some(kubernetes);
    let launch = LaunchMachine {
        provider: &*cloud,
        db: &db,
        paths,
        connector: &SshConnector,
        spec: plan.spec,
        request: plan.request,
        required_features: plan.required_features,
        spec_json: serde_json::to_string(&spec)?,
        ssh_key_path: keypair.private_path.clone(),
        retry_until_up: args.retry_until_up,
        ssh_timeout: wait::DEFAULT_TIMEOUT,
    };
    let cancel = Cancel::new();
    install_ctrl_c(&cancel);

    if kubernetes {
        let machine = K3sMachine::new(launch, paths);
        let end = Executor::new(&machine, &db, &vm_name, cancel)
            .run(K3sState::start())
            .await?;
        match end {
            K3sState::Ready { vm, access } => {
                print_vm(&vm, paths)?;
                print_kube(&vm, &access);
                Ok(())
            }
            K3sState::Failed { vm, reason } => {
                if let Some(vm) = vm {
                    print_vm(&vm, paths)?;
                    bail!(
                        "the VM is up but the cluster is not: {reason}\n  retry: re-run this \
                         command (the installer is idempotent)\n  remove: gpuprov down {}",
                        vm.name
                    );
                }
                bail!("{reason}")
            }
            other => bail!("unexpected end state: {other:?}"),
        }
    } else {
        let end = Executor::new(&launch, &db, &vm_name, cancel)
            .run(LaunchState::Reconciling)
            .await?;
        match end {
            LaunchState::Ready(vm) => print_vm(&vm, paths),
            failed => bail!(
                "{}",
                failed
                    .failure_summary()
                    .unwrap_or_else(|| format!("unexpected end state: {failed:?}"))
                    .replace("<name>", &vm_name)
            ),
        }
    }
}

fn print_vm(vm: &ReadyVm, paths: &Paths) -> Result<()> {
    if let Some(user_ssh_config) = Paths::user_ssh_config_path() {
        config_file::ensure_include(&user_ssh_config, &paths.ssh_dir())?;
    }
    println!();
    println!(
        "VM '{}' is UP{}",
        vm.name,
        if vm.reused {
            " (reused existing instance)"
        } else {
            ""
        }
    );
    println!(
        "  Endpoint:    {}@{}:{}",
        vm.user(),
        vm.endpoint.host,
        vm.endpoint.port
    );
    println!(
        "  Instance:    {}  ({} {} / {})",
        vm.instance_id,
        vm.provider,
        vm.offer.instance_type,
        vm.location()
    );
    println!(
        "  Connect:     gpuprov ssh {}    (or plain: ssh {})",
        vm.name, vm.name
    );
    if let Some(path) = &vm.ssh_config_path {
        println!("  SSH config:  {}", path.display());
    }
    println!("  Record:      {}", paths.db_path().display());
    Ok(())
}

fn print_kube(vm: &ReadyVm, access: &KubeAccess) {
    println!();
    println!("Kubernetes cluster '{}' is ready.", access.context);
    println!("  API:         {}", access.api_endpoint);
    println!("  Kubeconfig:  {}", access.kubeconfig_path.display());
    println!();
    if access.mode == AccessMode::SshTunnel {
        println!("  This cloud exposes SSH only; keep a tunnel open in another terminal:");
        println!("  gpuprov tunnel {}", vm.name);
    }
    println!("  export KUBECONFIG={}", access.kubeconfig_path.display());
    println!("  kubectl get nodes");
    println!("  kubectl apply -f examples/nginx.yaml      # deploy a container app");
    println!("  kubectl apply -f examples/gpu-test.yaml   # GPU smoke test (GPU nodes)");
    if access.mode == AccessMode::Direct {
        println!();
        println!(
            "  NOTE: the API (6443) and ingress (80/443) are open to 0.0.0.0/0; access is gated \
             by"
        );
        println!("        the client cert inside the kubeconfig — treat that file as a secret.");
    }
}

/// SSH using the DB record: interactive session, or run a one-shot command
/// whose exit code is propagated.
async fn ssh(args: SshArgs, paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
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
    let (ip, user, opts) = connection(&record)?;
    let opts = opts.control_dir(Some(paths.sockets_dir()));

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

/// The SSH connection for a record: `(host, user, options-with-key-and-port)`.
fn connection(record: &VmRecord) -> Result<(String, String, SshOptions)> {
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
    Ok((
        ip,
        user,
        SshOptions::new(key).port(record.ssh_port.unwrap_or(22)),
    ))
}

/// Re-run the cluster machine from `Installing` for an existing VM: the
/// installer is idempotent, so this both refreshes the kubeconfig and repairs
/// a half-installed node.
async fn kubeconfig(name: &str, config: &AppConfig, paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
    let record = load_record(&db, name)?;
    let spec: VmSpec = serde_json::from_str(&record.spec_json)
        .with_context(|| format!("cannot parse stored spec for VM '{name}'"))?;
    if spec.kubernetes != Some(true) {
        bail!("VM '{name}' is a plain VM (no k3s); re-run `gpuprov provision --k8s ...` to add it");
    }
    let vm = ReadyVm::from_record(&record)
        .with_context(|| format!("VM '{name}' is {}, not UP", record.state))?;
    let cloud = provider::build(&vm.provider, config).await?;
    let keypair = keys::ensure_keypair(&paths.keys_dir())?;
    let plan = planner::prepare(
        &spec,
        &config.defaults,
        config.default_regions(&vm.provider),
        &keypair.public_key_openssh,
    )?;
    let launch = LaunchMachine {
        provider: &*cloud,
        db: &db,
        paths,
        connector: &SshConnector,
        spec: plan.spec,
        request: plan.request,
        required_features: plan.required_features,
        spec_json: record.spec_json.clone(),
        ssh_key_path: keypair.private_path.clone(),
        retry_until_up: false,
        ssh_timeout: wait::DEFAULT_TIMEOUT,
    };
    let machine = K3sMachine::new(launch, paths);
    let cancel = Cancel::new();
    install_ctrl_c(&cancel);
    match Executor::new(&machine, &db, name, cancel)
        .run(K3sState::for_existing(vm))
        .await?
    {
        K3sState::Ready { vm, access } => {
            print_kube(&vm, &access);
            Ok(())
        }
        K3sState::Failed { reason, .. } => bail!("{reason}"),
        other => bail!("unexpected end state: {other:?}"),
    }
}

/// `ssh -N -L 16443:127.0.0.1:6443` for clusters on clouds without inbound
/// ports (the kubeconfig points at 127.0.0.1:16443).
fn tunnel(name: &str, paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
    let record = load_record(&db, name)?;
    if record.state != VmState::Up {
        bail!("VM '{name}' is {}, not UP", record.state);
    }
    let (ip, user, opts) = connection(&record)?;
    eprintln!(
        "forwarding 127.0.0.1:{} → {name}:{} (Ctrl-C to stop)",
        kube::TUNNEL_LOCAL_PORT,
        kube::API_PORT
    );
    let err = runner::exec_forward(&user, &ip, &opts, kube::TUNNEL_LOCAL_PORT, kube::API_PORT);
    bail!("failed to exec ssh: {err}")
}

fn list(paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
    let records = db.list()?;
    if records.is_empty() {
        println!(
            "No VMs yet. Provision one with: gpuprov provision --name <name> --instance-type \
             t3.micro --region us-west-2"
        );
        return Ok(());
    }
    println!(
        "{:<20} {:<13} {:<15} {:<14} {:<16} {:<22} {:<38} {:<16}",
        "NAME", "STATE", "PROVIDER", "REGION", "ZONE", "TYPE", "INSTANCE_ID", "ENDPOINT"
    );
    for r in records {
        let endpoint = match (&r.public_ip, r.ssh_port) {
            (Some(ip), Some(port)) if port != 22 => format!("{ip}:{port}"),
            (Some(ip), _) => ip.clone(),
            _ => "-".to_string(),
        };
        println!(
            "{:<20} {:<13} {:<15} {:<14} {:<16} {:<22} {:<38} {:<16}",
            r.name,
            r.state.as_str(),
            r.provider.as_deref().unwrap_or("-"),
            r.region.as_deref().unwrap_or("-"),
            r.zone.as_deref().filter(|z| !z.is_empty()).unwrap_or("-"),
            r.instance_type.as_deref().unwrap_or("-"),
            r.instance_id.as_deref().unwrap_or("-"),
            endpoint,
        );
    }
    Ok(())
}

fn status(name: &str, paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
    let record = load_record(&db, name)?;
    println!(
        "{name}: {} on {} {}{}",
        record.state,
        record.provider.as_deref().unwrap_or("-"),
        record.region.as_deref().unwrap_or("-"),
        record
            .zone
            .as_deref()
            .filter(|z| !z.is_empty())
            .map(|z| format!("/{z}"))
            .unwrap_or_default()
    );
    if let Some(row) = db.load_machine(name)? {
        println!(
            "last operation ({}): {}{}",
            row.kind,
            row.label,
            if row.terminal {
                ""
            } else {
                "  [in flight — re-run the command to resume]"
            }
        );
    }
    println!("recent transitions:");
    for event in db.machine_events(name, 30)? {
        println!(
            "  {}  [{}] {}",
            util::format_unix_utc(event.ts),
            event.kind,
            event.label
        );
        if let Some(detail) = event.detail {
            for line in detail.lines().filter(|l| !l.trim().is_empty()) {
                println!("        {}", line.trim());
            }
        }
    }
    Ok(())
}

async fn down(name: &str, config: &AppConfig, paths: &Paths) -> Result<()> {
    let db = open_db(paths)?;
    let record = load_record(&db, name)?;
    kube::remove_kubeconfig(paths, name)?;

    let Some(start) = TeardownState::for_record(&record) else {
        println!("VM '{name}' has no region recorded (never provisioned); removing local state.");
        db.set_state(name, VmState::Terminated)?;
        config_file::remove_vm_host_entry(&paths.ssh_dir(), name)?;
        return Ok(());
    };
    // Dispatch teardown to the adapter that provisioned the VM (rows from
    // before the provider column were all AWS).
    let provider_name = record
        .provider
        .clone()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.to_string());
    let cloud = provider::build(&provider_name, config)
        .await
        .with_context(|| format!("VM '{name}' was provisioned by '{provider_name}'"))?;
    if let TeardownState::Running(td) = &start {
        println!(
            "Terminating VM '{name}' at {} ({provider_name}) ...",
            td.location
        );
    }
    let machine = TeardownMachine {
        provider: &*cloud,
        db: &db,
        paths,
        vm_name: name.to_string(),
    };
    let cancel = Cancel::new();
    install_ctrl_c(&cancel);
    match Executor::new(&machine, &db, name, cancel)
        .run(start)
        .await?
    {
        TeardownState::Done => {
            println!("VM '{name}' terminated.");
            Ok(())
        }
        TeardownState::Leaked { location, reason } => bail!(
            "could not confirm teardown at {location}: {reason}\nresources may still be running; \
             re-run `gpuprov down {name}` or check the cloud console"
        ),
        TeardownState::Running(_) => unreachable!("executor returns terminal states"),
    }
}
