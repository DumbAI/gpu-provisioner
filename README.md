# gpu-provisioner (`gpuprov`)

[![Unit tests](https://github.com/DumbAI/gpu-provisioner/actions/workflows/unit.yml/badge.svg)](https://github.com/DumbAI/gpu-provisioner/actions/workflows/unit.yml)
[![Integration tests](https://github.com/DumbAI/gpu-provisioner/actions/workflows/integration.yml/badge.svg)](https://github.com/DumbAI/gpu-provisioner/actions/workflows/integration.yml)

GPU machines with Kubernetes on them, from one command. `gpuprov` provisions a GPU
VM on **AWS**, **GCP**, or **Prime Intellect**, failing over across offers,
regions, and zones until one has capacity. It turns the VM into a validated
single-node **k3s** cluster (on by default; `--no-k8s` for a plain VM), waits
until SSH and the cluster are healthy, and records everything locally.

```console
$ gpuprov provision --name dev --gpus L4:1 --cloud gcp --region us-central1 --region us-east1
VM 'dev' is UP
  Endpoint:    ubuntu@34.122.x.x:22
  Connect:     gpuprov ssh dev    (or plain: ssh dev)

Kubernetes cluster 'gpuprov-dev' is ready.
  Kubeconfig:  ~/.gpu-provisioner/kube/dev.yaml

$ gpuprov ssh dev -- nvidia-smi
$ gpuprov status dev       # journaled state transitions, incl. why offers were skipped
$ gpuprov down dev
```

📖 **Documentation:** <https://dumbai.github.io/gpu-provisioner/> (sources in [`docs/`](docs/))

## Highlights

- **One contract, many clouds.** Every cloud implements six idempotent primitives
  (`plan / prepare / create / observe / delete / cleanup`). A local **Docker**
  cloud runs the same machinery end to end without an account.
- **Durable, failure-tolerant state machines.** Launch, teardown, and k3s setup are
  explicit state machines journaled in SQLite. Retries, backoff, polling,
  deadlines, Ctrl-C compensation, crash resume, and a single-writer lease are
  implemented once, in the shared executor. Each step observes the cloud before
  it acts, so a resumed run adopts an instance rather than creating a second one.
- **Failover without leaks.** Capacity errors tear down and try the next offer, and
  quota errors skip the whole region. If teardown cannot be confirmed, the VM is
  recorded as `LEAKED` and the next run must clean it up first.
- **k3s by default, anywhere.** On cloud-init clouds, k3s installs while the VM
  boots. gpuprov verifies that over SSH and falls back to pushing the installer
  itself; the installer is lock-serialized and idempotent. GPU nodes get the NVIDIA
  runtime and device plugin. Clusters are validated, including the certificate SAN
  and API reachability, and reached directly or through `gpuprov tunnel` on clouds
  without inbound ports.
- **No server.** A single binary with state under `~/.gpu-provisioner`.

## Install

```bash
cargo install gpu-provisioner    # binary: gpuprov
```

Requirements: Rust 1.91.1+, and the OpenSSH client (`ssh`) on `PATH` (macOS or Linux).

## Quick start

```bash
# no cloud account needed: VMs are local sshd containers (plain VMs: no systemd for k3s)
gpuprov provision --cloud docker --name demo --instance-type container
gpuprov ssh demo -- uname -a
gpuprov down demo

# AWS (default cloud): cheapest T4 type, us-west-2 then us-east-1
gpuprov provision --name dev --gpus T4:1 --region us-west-2 --region us-east-1

# Prime Intellect marketplace: cheapest in-stock H100 anywhere
gpuprov provision --cloud primeintellect --name pi --gpus H100:1

# every VM is a k3s cluster by default
export KUBECONFIG=~/.gpu-provisioner/kube/dev.yaml && kubectl get nodes

# a plain VM, no Kubernetes
gpuprov provision --name plain --gpus T4:1 --region us-west-2 --no-k8s
```

See the docs for [configuration and credentials](docs/guides/configuration.md),
the [CLI reference](docs/guides/cli.md), [per-cloud details](docs/clouds/index.md),
and the [state-machine design](docs/architecture/state-machines.md).

## Architecture

```text
cluster::k3s   K3sMachine      Vm(child) → CheckingBootInstall → [Installing (SSH)] → AwaitingApi → FetchingKubeconfig → Validating → Ready
vm::launch     LaunchMachine   Reconciling → Planning → [Preparing → Creating → Booting → Connecting]* → Ready
vm::teardown   TeardownMachine Requesting → AwaitingGone → CleaningUp → Done | Leaked
machine        Executor        journal · retry budgets · polling · cancellation · resume · lease
provider       CloudProvider   aws · gcp · primeintellect · docker
```

The [state-machine design notes](docs/architecture/state-machines.md) explain each
layer and the trade-offs behind it.

## Development

```bash
cargo test                                               # unit tests, no cloud needed
cargo test --test docker_e2e -- --ignored --nocapture    # end-to-end on Docker
cargo clippy --all-targets -- -D warnings
```

## Scope / non-goals

One VM per name. Teardown terminates instances; there is no stop/resume. There is
no spot-preemption recovery, no storage mounts, and no multi-node k3s or HA
control plane.

## License

Apache-2.0. See [LICENSE](LICENSE).
