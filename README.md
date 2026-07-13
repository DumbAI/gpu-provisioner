# gpu-provisioner (`gpuprov`)

A mini-SkyPilot in Rust: provision a cloud VM (AWS) with region failover, then
SSH into it and run anything. A clean-room re-implementation of the core of
SkyPilot's provisioning pipeline — the stage machine, the failover engine, and
the cloud provisioner API — without the CLI/server split, API server, Ray, or
skylet. Optionally, `--k8s` turns the VM into a single-node **k3s** cluster and
writes a local kubeconfig, so apps deploy as containers with plain `kubectl`.

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

## Kubernetes (k3s)

`--k8s` installs a single-node [k3s](https://k3s.io) cluster on the VM at boot
(via cloud-init) and writes a local kubeconfig, so other applications deploy as
containers with plain `kubectl` — nothing extra to run. k3s's own control plane
(the `k3s.service` systemd unit) is the on-VM reconciler; there is no custom
orchestrator.

```bash
$ gpuprov provision --k8s --gpus A10G:1 --name dev --region us-east-2
...VM UP, then: waiting for k3s to come up (first boot installs it)...

Kubernetes cluster 'gpuprov-dev' is ready.
  API:         https://3.147.x.x:6443
  Kubeconfig:  ~/.gpu-provisioner/kube/dev.yaml

$ export KUBECONFIG=~/.gpu-provisioner/kube/dev.yaml
$ kubectl get nodes
$ kubectl apply -f examples/nginx.yaml       # a container app (reachable via Traefik on :80)
$ kubectl apply -f examples/gpu-test.yaml    # GPU smoke test (runs nvidia-smi)
$ gpuprov kubeconfig dev                     # re-fetch the kubeconfig anytime
```

- **Ports.** `--k8s` opens `6443` (API) plus `80`/`443` (the k3s-bundled Traefik
  ingress / ServiceLB) at security-group creation. They open to `0.0.0.0/0` (like
  SSH `22`); API access is gated by the client cert baked into the kubeconfig —
  treat that file as a secret. Add `--port N` for extra NodePorts.
- **GPU.** Auto-detected at boot via `nvidia-smi` (the GPU AMI ships the driver):
  installs `nvidia-container-toolkit` and the NVIDIA device plugin and runs k3s
  with `--default-runtime nvidia`, so pods request `nvidia.com/gpu` with no extra
  config (see `examples/gpu-test.yaml`).
- **Lifecycle.** The kubeconfig lives at `~/.gpu-provisioner/kube/<name>.yaml` and
  is removed on `gpuprov down`. The public IP is stable for the VM's lifetime (no
  Elastic IP); `gpuprov kubeconfig <name>` re-fetches it if the local file is lost.

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
kubernetes: false       # true: install single-node k3s + write a local kubeconfig
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
├── ssh/<vm-name>       # generated Host block, Include'd from ~/.ssh/config
├── kube/<vm-name>.yaml # kubeconfig for a --k8s cluster (server rewritten to public IP)
└── sockets/            # SSH ControlMaster sockets
```

VM states: `PROVISIONING → UP → TERMINATED`, `FAILED` on exhausted failover.
Re-running `provision` is idempotent: an UP record is reconciled against the
cloud, and a live instance with the identity tag is reused, never duplicated.

Set `GPUPROV_HOME` to relocate all local state (useful for testing).
`RUST_LOG=debug` increases progress-log verbosity (logs go to stderr; the
final summary goes to stdout).

## Scope / non-goals

Single VM per name, AWS only (trait is cloud-agnostic), terminate-only
teardown (no stop/resume), no spot-preemption recovery, no storage mounts, and
no on-VM agent beyond the optional `--k8s` k3s cluster (single-node; no
multi-node join or HA control plane). See `SKYPILOT_VM_PROVISION.md` in the
SkyPilot repo for the full-system reference this is distilled from.
