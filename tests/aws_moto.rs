//! Integration tests: the AWS adapter, driven by the real launch and
//! teardown state machines, against [moto](https://docs.getmoto.org/) — a
//! maintained emulator of the EC2 and SSM APIs, run in server mode.
//!
//! The EC2 Query/XML protocol is large; rather than hand-writing a stub of
//! it (as `tests/stubs/` does for the JSON clouds), these tests point the
//! real AWS SDK clients at moto through `AwsConfig::endpoint_url`. Every call
//! the adapter makes is a real EC2/SSM API call, answered by moto:
//!
//! | adapter step | EC2 / SSM actions (API reference) |
//! |---|---|
//! | prepare | [DescribeInstanceTypes](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeInstanceTypes.html), [DescribeInstanceTypeOfferings](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeInstanceTypeOfferings.html), [DescribeImages](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeImages.html), [DescribeVpcs](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeVpcs.html), [DescribeSubnets](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeSubnets.html), [DescribeRouteTables](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeRouteTables.html), [CreateSecurityGroup](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_CreateSecurityGroup.html), [DescribeSecurityGroups](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeSecurityGroups.html), [AuthorizeSecurityGroupIngress](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_AuthorizeSecurityGroupIngress.html) |
//! | create | [RunInstances](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_RunInstances.html) |
//! | observe | [DescribeInstances](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeInstances.html) (tag + state filters) |
//! | delete / cleanup | [TerminateInstances](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_TerminateInstances.html), [DeleteSecurityGroup](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DeleteSecurityGroup.html) |
//!
//! Two moto gaps are bridged through moto's own EC2 API, never by faking the
//! adapter:
//!
//! - **AMI**: the public SSM parameters (`/aws/service/canonical/…`,
//!   `/aws/service/deeplearning/…`) are reserved names moto neither ships nor
//!   lets you create, so the VM spec names one of moto's default images
//!   explicitly (`ami`), exactly as a user's `--image` would.
//! - **Public subnets**: moto's default VPC main route table has only the
//!   `local` route; the adapter (correctly) only launches into subnets with an
//!   internet-gateway route, so the test attaches an internet gateway and adds
//!   `0.0.0.0/0` to the main route table, as a real default VPC has.
//!
//! Scenarios (mirroring `tests/stubs/harness.rs`, reusing its `Run` and the
//! `VendorStub` contract): lifecycle with a leak check, reuse, and an
//! interrupt that lands while the instance exists (the connector "presses
//! Ctrl-C" during the SSH wait). Capacity failover is not run: moto cannot
//! return `InsufficientInstanceCapacity`, and faking it below the SDK would
//! test the fake, not AWS; that path is covered by the error-classification
//! unit tests and the launch-machine tests.
//!
//! Run against a local moto (CI does the same, as a service container):
//!
//! ```text
//! docker run -d --rm -p 127.0.0.1:5000:5000 --name gpuprov-moto motoserver/moto:5.2.3
//! GPUPROV_MOTO_ENDPOINT=http://127.0.0.1:5000 cargo test --test aws_moto -- --ignored
//! ```
//!
//! Without `GPUPROV_MOTO_ENDPOINT` the tests skip with a notice, unless
//! `GPUPROV_E2E_REQUIRE_MOTO=1` (set in CI), which turns a missing emulator
//! into a failure.

mod stubs {
    // The shared scenario harness; this crate only uses part of it.
    #[allow(dead_code, unused_imports, unused_macros)]
    #[path = "harness.rs"]
    pub mod harness;
}

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use aws_sdk_ec2::types::Filter;
use gpu_provisioner::config::{AppConfig, AwsConfig};
use gpu_provisioner::db::VmState;
use gpu_provisioner::machine::Cancel;
use gpu_provisioner::provider::SshEndpoint;
use gpu_provisioner::vm::launch::{Connector, LaunchState, ReadyVm};
use gpu_provisioner::vm::teardown::TeardownState;

use stubs::harness::{Ask, Run, VendorStub};

const REGION: &str = "us-east-1";
const INSTANCE_TYPE: &str = "t3.micro";

/// The moto endpoint, or `None` (test skipped with a notice).
fn endpoint() -> Option<String> {
    match std::env::var("GPUPROV_MOTO_ENDPOINT") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            assert!(
                std::env::var_os("GPUPROV_E2E_REQUIRE_MOTO").is_none(),
                "GPUPROV_E2E_REQUIRE_MOTO is set but GPUPROV_MOTO_ENDPOINT is not"
            );
            eprintln!("SKIP: set GPUPROV_MOTO_ENDPOINT to a running moto server");
            None
        }
    }
}

async fn ec2(endpoint: &str) -> aws_sdk_ec2::Client {
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .endpoint_url(endpoint)
        .credentials_provider(aws_credential_types::Credentials::from_keys(
            "test", "test", None,
        ))
        .load()
        .await;
    aws_sdk_ec2::Client::new(&conf)
}

fn filter(name: &str, value: &str) -> Filter {
    Filter::builder().name(name).values(value).build()
}

/// A moto-backed "account". Tests share one moto server and run in
/// parallel, so nothing here resets moto; every check filters by VM name.
pub struct Moto {
    endpoint: String,
    ec2: aws_sdk_ec2::Client,
    ami: String,
    /// VM names the scenarios launched (leftover checks look these up).
    names: Mutex<Vec<String>>,
}

impl Moto {
    async fn connect(endpoint: String) -> Moto {
        let ec2 = ec2(&endpoint).await;
        ensure_public_default_vpc(&ec2).await;
        let ami = ec2
            .describe_images()
            .owners("amazon")
            .filters(filter("architecture", "x86_64"))
            .send()
            .await
            .expect("moto DescribeImages")
            .images()
            .first()
            .and_then(|i| i.image_id())
            .expect("moto ships default images")
            .to_string();
        Moto {
            endpoint,
            ec2,
            ami,
            names: Mutex::new(Vec::new()),
        }
    }

    fn track(&self, name: &str) {
        self.names.lock().unwrap().push(name.to_string());
    }

    /// All instances (any state) tagged for `name`.
    async fn instances(&self, name: &str) -> Vec<(String, String)> {
        let resp = self
            .ec2
            .describe_instances()
            .filters(filter("tag:gpuprov-vm-name", name))
            .send()
            .await
            .expect("moto DescribeInstances");
        resp.reservations()
            .iter()
            .flat_map(|r| r.instances())
            .map(|i| {
                (
                    i.instance_id().unwrap_or_default().to_string(),
                    i.state()
                        .and_then(|s| s.name())
                        .map(|n| n.as_str().to_string())
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    async fn security_groups(&self, name: &str) -> Vec<String> {
        self.ec2
            .describe_security_groups()
            .filters(filter("group-name", &format!("gpuprov-{name}")))
            .send()
            .await
            .expect("moto DescribeSecurityGroups")
            .security_groups()
            .iter()
            .filter_map(|g| g.group_id().map(str::to_string))
            .collect()
    }

    async fn leftovers_async(&self) -> Vec<String> {
        let names = self.names.lock().unwrap().clone();
        let mut left = Vec::new();
        for name in names {
            for (id, state) in self.instances(&name).await {
                if state != "terminated" {
                    left.push(format!("instance {id} ({state}) for {name}"));
                }
            }
            for sg in self.security_groups(&name).await {
                left.push(format!("security group {sg} for {name}"));
            }
        }
        left
    }
}

/// moto's default VPC has no internet gateway route; a real default VPC
/// does, and the adapter only launches into public subnets. Idempotent and
/// safe under parallel tests (duplicate-route races are ignored).
async fn ensure_public_default_vpc(ec2: &aws_sdk_ec2::Client) {
    let vpc = ec2
        .describe_vpcs()
        .filters(filter("isDefault", "true"))
        .send()
        .await
        .expect("moto DescribeVpcs")
        .vpcs()
        .first()
        .and_then(|v| v.vpc_id())
        .expect("moto has a default VPC")
        .to_string();
    let tables = ec2
        .describe_route_tables()
        .filters(filter("vpc-id", &vpc))
        .filters(filter("association.main", "true"))
        .send()
        .await
        .expect("moto DescribeRouteTables");
    let Some(main) = tables.route_tables().first() else {
        panic!("moto default VPC has no main route table");
    };
    let has_igw = main.routes().iter().any(|r| {
        r.gateway_id().is_some_and(|g| g.starts_with("igw-"))
            && r.destination_cidr_block() == Some("0.0.0.0/0")
    });
    if has_igw {
        return;
    }
    let table_id = main.route_table_id().unwrap_or_default().to_string();
    let igw = ec2
        .create_internet_gateway()
        .send()
        .await
        .expect("moto CreateInternetGateway")
        .internet_gateway()
        .and_then(|g| g.internet_gateway_id())
        .unwrap_or_default()
        .to_string();
    ec2.attach_internet_gateway()
        .internet_gateway_id(&igw)
        .vpc_id(&vpc)
        .send()
        .await
        .expect("moto AttachInternetGateway");
    // A parallel test may have added the route first; that is fine.
    let _ = ec2
        .create_route()
        .route_table_id(table_id)
        .destination_cidr_block("0.0.0.0/0")
        .gateway_id(igw)
        .send()
        .await;
}

#[async_trait]
impl VendorStub for Moto {
    const CLOUD: &'static str = "aws";

    async fn start() -> Self {
        let endpoint = endpoint().expect("callers check endpoint() first");
        Moto::connect(endpoint).await
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            aws: AwsConfig {
                access_key_id: Some("test".into()),
                secret_access_key: Some("test".into()),
                endpoint_url: Some(self.endpoint.clone()),
                ..AwsConfig::default()
            },
            ..AppConfig::default()
        }
    }

    fn ask(&self) -> Ask {
        Ask {
            instance_type: Some(INSTANCE_TYPE.into()),
            regions: vec![REGION.into()],
            ..Ask::default()
        }
    }

    fn fail_next_create_with_capacity(&self) {
        unreachable!("moto cannot return InsufficientInstanceCapacity (see module docs)")
    }

    fn hold_boot(&self, _hold: bool) {
        unreachable!("moto runs instances immediately (see the interrupt scenario)")
    }

    fn leftovers(&self) -> Vec<String> {
        // The trait is synchronous; scenarios here call `leftovers_async`.
        unreachable!("use Moto::leftovers_async")
    }

    fn creates(&self) -> usize {
        // Scenarios here count tagged instances in moto directly.
        unreachable!("use Moto::instances")
    }

    fn expected_login(&self) -> (String, u16) {
        // Our cloud-init creates the default `ubuntu` user; sshd on 22.
        ("ubuntu".into(), 22)
    }
}

/// A run whose spec names moto's AMI explicitly (see module docs). Names get
/// a per-process suffix: moto keeps terminated instances, so a re-run
/// against the same server must not see the previous run's VMs.
async fn run(moto: &Moto, prefix: &str) -> Run {
    let name = format!("{prefix}-{}", std::process::id());
    moto.track(&name);
    let mut run = Run::new(moto, &name).await;
    run.spec.ami = Some(moto.ami.clone());
    run
}

fn ready(run: &Run, end: LaunchState) -> ReadyVm {
    match end {
        LaunchState::Ready(vm) => vm,
        other => panic!("expected Ready, got {other:?}\njournal:\n{}", run.journal()),
    }
}

async fn assert_no_leftovers(moto: &Moto, run: &Run) {
    let left = moto.leftovers_async().await;
    assert!(
        left.is_empty(),
        "billable resources left behind: {left:?}\njournal:\n{}",
        run.journal()
    );
}

#[tokio::test]
#[ignore = "needs a moto server; see module docs"]
async fn lifecycle_leaves_nothing_behind() {
    let Some(endpoint) = endpoint() else { return };
    let moto = Moto::connect(endpoint).await;
    let run = run(&moto, "moto-life").await;
    let vm = ready(&run, run.launch(Cancel::new()).await);

    let (user, port) = moto.expected_login();
    assert_eq!(vm.user(), user);
    assert_eq!(vm.endpoint.port, port);
    let probed = run.connector.probed.lock().unwrap().clone();
    assert!(
        probed.contains(&(user, vm.endpoint.host.clone(), port)),
        "SSH not attempted at the reported endpoint: {probed:?}"
    );
    let instances = moto.instances(&run.name).await;
    assert_eq!(instances.len(), 1, "exactly one instance: {instances:?}");
    assert_eq!(moto.security_groups(&run.name).await.len(), 1);
    assert_eq!(run.db.get(&run.name).unwrap().unwrap().state, VmState::Up);

    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&moto, &run).await;
    assert_eq!(
        run.db.get(&run.name).unwrap().unwrap().state,
        VmState::Terminated
    );
}

#[tokio::test]
#[ignore = "needs a moto server; see module docs"]
async fn reprovision_reuses_the_running_instance() {
    let Some(endpoint) = endpoint() else { return };
    let moto = Moto::connect(endpoint).await;
    let run = run(&moto, "moto-reuse").await;
    let first = ready(&run, run.launch(Cancel::new()).await);
    let second = ready(&run, run.launch(Cancel::new()).await);
    assert!(second.reused, "second provision should reuse");
    assert_eq!(second.instance_id, first.instance_id);
    assert_eq!(
        moto.instances(&run.name).await.len(),
        1,
        "no second RunInstances"
    );
    assert_eq!(run.down().await, TeardownState::Done, "{}", run.journal());
    assert_no_leftovers(&moto, &run).await;
}

/// "Presses Ctrl-C" during the SSH wait — the instance exists in moto at
/// that point — and reports SSH as not ready yet.
struct CancelOnProbe {
    cancel: Cancel,
}

#[async_trait]
impl Connector for CancelOnProbe {
    async fn probe(&self, _: &SshEndpoint, _: &str, _: &Path) -> Result<(), String> {
        self.cancel.cancel();
        Err("connection refused".into())
    }
}

#[tokio::test]
#[ignore = "needs a moto server; see module docs"]
async fn interrupt_after_create_cleans_up() {
    let Some(endpoint) = endpoint() else { return };
    let moto = Moto::connect(endpoint).await;
    let run = run(&moto, "moto-int").await;
    let cancel = Cancel::new();
    let connector = CancelOnProbe {
        cancel: cancel.clone(),
    };
    let mut machine = run.machine();
    machine.connector = &connector;
    let end = gpu_provisioner::machine::Executor::new(&machine, &run.db, &run.name, cancel)
        .run(LaunchState::Reconciling)
        .await
        .expect("launch journal");
    match &end {
        LaunchState::Aborted { reason, .. } => assert!(reason.contains("cancelled"), "{reason}"),
        other => panic!(
            "expected a cancelled launch, got {other:?}\njournal:\n{}",
            run.journal()
        ),
    }
    // The instance really existed and was torn down.
    let instances = moto.instances(&run.name).await;
    assert_eq!(
        instances.len(),
        1,
        "one instance was created: {instances:?}"
    );
    assert_no_leftovers(&moto, &run).await;
}
