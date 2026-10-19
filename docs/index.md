# gpuprov

**GPU machines with Kubernetes on them, from one command.** `gpuprov` provisions
a GPU VM on the cheapest cloud offer that has capacity, turns it into a
single-node **k3s** cluster, waits until both SSH and the cluster are healthy,
and records the result locally. You can then use it over plain SSH or deploy
containers with `kubectl`.

```console
$ gpuprov provision --name dev --gpus L4:1 --cloud gcp --region us-central1 --region us-east1
VM 'dev' is UP
  Endpoint:    ubuntu@34.122.x.x:22
  Connect:     gpuprov ssh dev    (or plain: ssh dev)

Kubernetes cluster 'gpuprov-dev' is ready.
  API:         https://34.122.x.x:6443
  Kubeconfig:  ~/.gpu-provisioner/kube/dev.yaml

$ gpuprov ssh dev -- nvidia-smi
$ KUBECONFIG=~/.gpu-provisioner/kube/dev.yaml kubectl get nodes
$ gpuprov down dev
```

## What it does

- **Multi-cloud, one interface.** AWS, GCP, budget GPU clouds (Massed Compute,
  Hyperstack, Lambda, Verda), GPU marketplaces (Prime Intellect, RunPod), and a
  local Docker "cloud" for tests all sit behind one
  [`CloudProvider`](architecture/adding-a-cloud.md) trait of six idempotent
  primitives.
- **Failover that does not leak.** Offers are tried in price order. A capacity error
  tears down the partial attempt and moves on, and a quota error skips the whole
  region. If teardown cannot be confirmed, the VM is recorded as `LEAKED`
  instead of being forgotten.
- **Durable state machines.** Every operation is journaled in local SQLite. Ctrl-C
  cleans up what the run created, and a crash resumes from the last state. It never
  creates a second instance, because every step observes the cloud before it acts.
  See [State machines](architecture/state-machines.md).
- **Kubernetes by default.** On clouds with cloud-init, k3s installs while the VM
  boots. gpuprov then verifies that install over SSH and falls back to pushing the
  installer itself when needed. GPU nodes get the NVIDIA runtime and device plugin,
  and every cluster is validated before you get a kubeconfig.
  [Opt out](guides/kubernetes.md#turning-it-off) with `--no-k8s`.
- **SSH-native.** One local ed25519 key and a per-VM `~/.ssh/config` entry, so plain
  `ssh dev`, VS Code Remote, and `rsync` work.
- **No server.** A single binary with state in `~/.gpu-provisioner`.

## Where to go next

| I want to... | Read |
|---|---|
| install it and launch a first VM | [Getting started](getting-started.md) |
| see every command and flag | [CLI reference](guides/cli.md) |
| set credentials and defaults | [Configuration](guides/configuration.md) |
| use or turn off the cluster | [Kubernetes (k3s)](guides/kubernetes.md) |
| understand the design | [State machines](architecture/state-machines.md) |
| add a new cloud | [Adding a cloud](architecture/adding-a-cloud.md) |
