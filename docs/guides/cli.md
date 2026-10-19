# CLI reference

Global flag: `--config <PATH>` selects the config file (default
`~/.gpu-provisioner/config.toml`; see [Configuration](configuration.md)).

Environment: `GPUPROV_HOME` relocates all local state, and `RUST_LOG=debug`
gives verbose progress. Logs go to stderr and summaries to stdout.

## `gpuprov provision`

Provision a VM and block until it is SSH-ready and, unless k3s is turned off,
until its cluster is validated. Re-running the command with the same `--name`
is idempotent: a running VM is verified and reused, and an interrupted run is
resumed.

| Flag | Meaning |
|---|---|
| `--name <NAME>` | VM name: lowercase letters, digits and `-`, up to 40 characters. It also works as `name:` in the spec file. |
| `--spec <FILE>` | YAML spec file. Flags override its fields. |
| `--cloud <ID>` | `aws` (default), `gcp`, `massedcompute`, `hyperstack`, `lambda`, `digitalocean`, `verda`, `primeintellect`, `runpod`, or `docker`. |
| `--instance-type <T>` | Exact type: `g5.xlarge`, `g2-standard-8`, `hyperstack/n3-H100x1`, `"NVIDIA H100 80GB HBM3"` (RunPod), `1H100.80S.22V` (Verda), `gpu_1x_a6000` (Lambda), `gpu-6000adax1-48gb` (DigitalOcean), `n3-RTX-A6000x1` (Hyperstack), or `container`. Cannot be combined with `--gpus`. |
| `--gpus <NAME[:N]>` | GPU shorthand resolved by the cloud's catalog, e.g. `T4:1`, `L4:1`, `A100-80GB:8`, `H100:8`. |
| `--region <R>` | Candidate region, in failover order. Repeatable. Region names are cloud-specific. |
| `--zone <Z>` | Pin to one zone (AWS and GCP) or data center (Prime Intellect, RunPod). |
| `--image <ID>` (alias `--ami`) | Machine-image override (container image on RunPod). |
| `--disk <GB>` | Root disk size (default 100). |
| `--spot` | Spot or preemptible capacity, where the cloud supports it. |
| `--port <P>` | Extra inbound port besides SSH. Repeatable. Requires a cloud with firewall control. |
| `--ssh-user <U>` | Login user to create (default `ubuntu`). Marketplace clouds dictate their own. |
| `--k8s` | Require the k3s cluster. It's on by default; this re-enables it when the config turns it off, and it fails on clouds that can't host k3s ([guide](kubernetes.md)). |
| `--no-k8s` | Plain VM: skip k3s. |
| `--retry-until-up` | When every offer fails, wait 30s and re-plan, forever. |

Unsupported feature/cloud combinations, such as `--port` on Prime Intellect, are
rejected before anything is planned.

Spec file example:

```yaml
name: trainer
cloud: gcp
gpus: L4:1
regions: [us-central1, us-east1]
disk_gb: 200
kubernetes: false   # plain VM; omit to use the config default (on)
```

## `gpuprov ssh <NAME> [-- CMD...]`

Opens an interactive shell, or runs one command and propagates its exit code.
It uses the recorded user and port.

## `gpuprov list`

Lists every VM row: state, provider, region and zone, type, instance id, and
endpoint (`host:port` when the port isn't 22).

States: `PROVISIONING`, `UP`, `FAILED`, `TERMINATED`, and `LEAKED`. `LEAKED`
means teardown could not be confirmed. Run `down` again.

## `gpuprov health [TARGET...]`

Checks whether VMs are still running and usable, or were stopped, preempted, or
reclaimed by the vendor. It asks the vendor first, then probes ping, TCP, and SSH,
and falls back to those probes alone when the vendor API is unavailable.
`TARGET` is a VM name, instance id, or IP; with none, it checks every live VM.
An IP gpuprov has no record of gets network probes only.

| Flag | Meaning |
|---|---|
| `--deep` | also `nvidia-smi -L` and, for k3s VMs, the cluster's health |
| `--json` | print the reports as JSON |
| `--port <P>` | port to probe for unrecorded addresses (default 22) |

Exit code: 0 healthy, 1 degraded, 2 unhealthy/gone, 3 unknown. See
[Health checks](health.md).

## `gpuprov status <NAME>`

Shows the VM row, the last operation's state (marked *in flight* if it was
interrupted), and up to 30 recent journaled transitions with failure details.
This is the place to look when a provision fails.

## `gpuprov down <NAME>`

Terminates the VM's instances, waits until they are gone, deletes per-VM side
resources (security group or firewall rule), and removes local SSH and kubeconfig
files. Ctrl-C does not stop a teardown halfway; press it twice to force-exit, then
re-run to resume.

## `gpuprov kubeconfig <NAME>`

For cluster VMs, re-runs the cluster steps (install check with SSH fallback, API
wait, kubeconfig fetch, and validation) and rewrites
`~/.gpu-provisioner/kube/<NAME>.yaml`.

## `gpuprov tunnel <NAME>`

For clusters on clouds without inbound-port control (Massed Compute, Verda, Prime Intellect),
forwards `127.0.0.1:16443` to the node's API server. The kubeconfig of such
clusters already points there. Keep it running while you use `kubectl`.
