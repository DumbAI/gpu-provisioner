# RunPod

[RunPod](https://www.runpod.io) rents GPUs by the hour from its own data centers
(**secure** cloud) and from vetted hosts (**community** cloud). A RunPod "VM" is
a **pod**: a GPU container, reached over SSH as `root`.

```bash
export RUNPOD_API_KEY=...          # or [runpod] api_key, or ~/.runpod/config.toml
gpuprov provision --cloud runpod --name dev --gpus RTX4090:1
gpuprov provision --cloud runpod --name big --gpus H100:8 --region north_america
gpuprov ssh dev -- nvidia-smi
```

## Planning: live availability

`gpuprov` asks RunPod's catalog for current stock
(`GET /v2/catalog/gpus?include=AVAILABILITY&product=POD`) once per cloud tier,
and turns every data center that has the GPU in stock into one offer:

- `instance_type` = `<RunPod GPU id> x<count> <tier>`, e.g. `NVIDIA GeForce RTX 4090 x1 community`
- `region` = the data center's continent (`north_america`, `europe`, `asia`, …)
- `zone` = the data center id (`US-TX-3`, `EU-RO-1`, …), which `--zone` pins
- ordered by hourly price (secure and community tiers compete), cheapest first

`--region` accepts continents or ISO country codes (`--region US --region CA`).
No `--region` means anywhere.

GPU names match on whole tokens: `H100` matches every H100 variant (PCIe, SXM,
NVL), `A100-80GB` matches both 80 GB A100s, `RTX4090` matches the GeForce RTX
4090, and `L40` does not match `L40S`. `--instance-type` takes an exact RunPod GPU
id, such as `"NVIDIA H100 80GB HBM3"`.

Because stock changes minute to minute, `prepare` re-checks the chosen data
center right before creating, so a stale offer fails without creating anything.

## SSH

Pods are created with port `22/tcp` published and the public key passed in the
`PUBLIC_KEY` environment variable. Official `runpod/*` images start sshd from it.
gpuprov adds the keys already registered on your account, read-only, so your own
SSH keys keep working. Your account's key list is never modified.

SSH goes to the pod's `ssh.direct` endpoint, a public IP plus a random port
mapped to 22, as `root`. `gpuprov ssh <name>` and plain `ssh <name>` handle the
port for you.

## Images

The default image is `runpod/base:1.4.1-cuda1290-ubuntu2204` (Ubuntu 22.04,
CUDA 12.9). Override it per VM with `--image` or for all pods with
`[runpod] image`. A custom image must start sshd from `$PUBLIC_KEY`; images built
on the official `runpod/*` images do.

## Limits

These are rejected up front:

| Feature | Why |
|---|---|
| `--port` | pod ports are published on random public ports, not the requested number |
| `--spot` | the REST API has no interruptible pods |
| `--k8s` | pods are containers without systemd, so k3s cannot run; the default k3s cluster is skipped with a note |

Pods can take several minutes to pull a large image, so the boot budget is 15
minutes. Disk (`--disk`) is the pod's container disk, which is wiped if the pod
restarts.
