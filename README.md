# gpu-provisioner (`gpuprov`)

A mini-SkyPilot in Rust: provision a cloud VM (AWS) with region failover, then
SSH into it and run anything. A clean-room re-implementation of the core of
SkyPilot's provisioning pipeline — the stage machine, the failover engine, and
the cloud provisioner API — without the CLI/server split, API server, Ray, or
skylet.

```
$ gpuprov provision --name dev --gpus T4:1 --region us-west-2 --region us-east-1
...streamed progress: plan → provision (failover) → wait-ssh → persist...

VM 'dev' is UP
  Public IP:   54.190.xxx.xxx
  Instance:    i-0abc123...  (us-west-2 / us-west-2a)
  Login:       ubuntu@54.190.xxx.xxx
  Connect:     gpuprov ssh dev    (or plain: ssh dev)
  SSH config:  ~/.gpu-provisioner/ssh/dev
  Record:      ~/.gpu-provisioner/state.db

$ gpuprov ssh dev -- nvidia-smi
$ gpuprov ssh dev            # interactive shell
$ gpuprov list
$ gpuprov down dev
```

## How it works

`gpuprov provision` is one long-running, blocking operation:

1. **Plan** (`planner.rs` + the provider) — validate the spec, then let the
   cloud provider resolve it into ordered `{instance_type, region, zone}`
   candidates: a GPU ask like `L40S:1` expands to a ladder of instance types
   (cheapest first — same GPU, more CPU/RAM as you go up), tried in every
   candidate region before moving to the next type. GPU→type mapping and
   region/zone naming are provider-specific (`provider/aws/catalog.rs`).
2. **Provision with failover** (`failover.rs`) — per candidate region:
   bootstrap (live instance-type metadata: GPU/arch facts and the zones that
   actually offer the type; AMI via SSM public parameters + per-image root
   device/min-disk facts; default-VPC public subnets restricted to offering
   zones; per-VM security group) → find-or-create the EC2 instance (retrying
   across zones/subnets on capacity errors) → wait until running. On failure:
   classify the error, tear down partial resources, block the region, try the
   next one. Credential errors abort; failed teardown aborts loudly. Regions
   that don't sell the instance type are skipped cleanly before anything is
   created.
3. **Wait for SSH** (`ssh/wait.rs`) — TCP banner probe then a real SSH login
   probe, up to 10 minutes.
4. **Persist** — write the VM record (IP, instance id, SSH fields) to the
   local SQLite DB (`~/.gpu-provisioner/state.db`) and a per-VM SSH config
   included from `~/.ssh/config`, so plain `ssh <name>` works too.

The SSH key is a local ed25519 keypair generated on first use and injected via
cloud-init user data — no EC2 key-pair APIs, no IAM instance profile, IMDSv2
required, everything tagged `gpuprov-vm-name=<name>` / `gpuprov-managed=true`.

Layering (ports-and-adapters): `orchestrator` → `failover` → `CloudProvider`
trait (`provider/mod.rs`) → `provider/aws` (the only module touching the AWS
SDK). Adding a cloud = implementing the five-method trait.

## Install / build

```bash
cargo build --release          # binary at target/release/gpuprov
cargo test                     # unit tests (no cloud access needed)
```

Requirements: Rust 1.75+, OpenSSH client (`ssh`) on PATH.

## AWS credentials

Resolution order (config file wins):

1. Static keys in the config file
2. Named profile in the config file
3. Default AWS chain (`AWS_ACCESS_KEY_ID`/`AWS_PROFILE` env, `~/.aws/...`)

`~/.gpu-provisioner/config.toml` (or pass `--config <path>`):

```toml
[aws]
profile = "default"
# ...or explicit static credentials:
# access_key_id = "AKIA..."
# secret_access_key = "..."
# session_token = "..."       # optional

[defaults]
regions = ["us-west-2", "us-east-1"]   # failover order when --region is omitted
disk_gb = 100
ssh_user = "ubuntu"
```

The caller needs IAM permissions for: `ec2:Describe*`, `ec2:RunInstances`,
`ec2:TerminateInstances`, `ec2:CreateTags`, `ec2:CreateSecurityGroup`,
`ec2:DeleteSecurityGroup`, `ec2:AuthorizeSecurityGroupIngress`, and
`ssm:GetParameter` (public AMI parameters). The VM itself gets **no** IAM role.

## Spec file

Flags override the file (`--spec vm.yaml`):

```yaml
name: train-box
gpus: A10G:1            # or: instance_type: g5.xlarge
regions: [us-west-2, us-east-1]
# zone: us-west-2a      # optional pin
# ami: ami-0123...      # optional override
disk_gb: 200
spot: false
ports: [8888]           # extra ingress besides SSH
ssh_user: ubuntu
```

Default AMIs are resolved per region at provision time via SSM public
parameters, keyed on the instance type's live metadata (NVIDIA GPU? arch?):

| | x86_64 | arm64 |
|---|---|---|
| CPU | Canonical Ubuntu 22.04 | Canonical Ubuntu 22.04 (arm64) |
| GPU | Deep Learning Base AMI (OSS NVIDIA driver) | Deep Learning Base AMI arm64 (OSS NVIDIA driver) |

`nvidia-smi` works on boot for GPU types. Pre-Turing GPUs (K80/V100/M60) are
not supported by the OSS driver — pass `--ami`. Each AMI's root device name
and minimum root-disk size are queried before launch; a `--disk` smaller than
the AMI's snapshot is rejected with the required size.

GPU catalog shorthand: `T4:1|4|8`, `A10G:1|4|8`, `L4:1|4|8`, `L40S:1|4|8`,
`A100:8`, `A100-80GB:8`, `H100:8`, `H200:8` (see
`src/provider/aws/catalog.rs`). One shorthand maps to a ladder of instance
types with the same GPUs but growing CPU/RAM, tried cheapest-first with
failover across types and regions; `--instance-type` pins one exact type
(e.g. `g6e.8xlarge` for 1× L40S with 32 vCPUs).

## State & files

```
~/.gpu-provisioner/
├── config.toml        # optional CLI config (credentials, defaults)
├── state.db           # SQLite: one row per VM (provider, state, type, IPs, ssh fields)
├── keys/gpuprov_ed25519{,.pub}
├── ssh/<vm-name>      # generated Host block, Include'd from ~/.ssh/config
└── sockets/           # SSH ControlMaster sockets
```

VM states: `PROVISIONING → UP → TERMINATED`, `FAILED` on exhausted failover.
Re-running `provision` is idempotent: an UP record is reconciled against the
cloud, and a live instance with the identity tag is reused, never duplicated.

Set `GPUPROV_HOME` to relocate all local state (useful for testing).
`RUST_LOG=debug` increases progress-log verbosity (logs go to stderr; the
final summary goes to stdout).

## Scope / non-goals

Single VM per name, AWS only (trait is cloud-agnostic), terminate-only
teardown (no stop/resume), no spot-preemption recovery, no on-VM agent, no
storage mounts. See `SKYPILOT_VM_PROVISION.md` in the SkyPilot repo for the
full-system reference this is distilled from.
