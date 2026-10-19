# DigitalOcean

[DigitalOcean](https://www.digitalocean.com) rents GPU Droplets: full VMs with
NVIDIA (H100, L40S, RTX 4000/6000 Ada) or AMD (MI300X) GPUs. Which GPUs are
offered depends on the region, and GPU Droplets may first need to be enabled
for your account. gpuprov boots DigitalOcean's AI/ML-ready GPU images, which
are Ubuntu with the GPU drivers preinstalled.

```bash
export DIGITALOCEAN_ACCESS_TOKEN=...      # or [digitalocean] api_key in config.toml
gpuprov provision --cloud digitalocean --name dev --gpus L40S:1
gpuprov provision --cloud digitalocean --name big --gpus H100:8 --region nyc2
```

Create a token with read and write scope at
<https://cloud.digitalocean.com/account/api/tokens>. `$DIGITALOCEAN_TOKEN` is
also accepted.

!!! note "Acceptance testing pending"
    This adapter is covered by unit, mock-API and
    [vendor-stub](../development/vendor-stubs.md) tests. It has not yet passed
    the end-to-end acceptance run against a live account (allocate → SSH →
    k3s/GPU ready → terminate → nothing billable left), so treat it as preview.

## Planning: live sizes

One call, `GET /v2/sizes`, returns every Droplet size with its hourly price, its
GPU (`gpu_info`), and the regions it can be created in. Each available size ×
region becomes one offer:

- `instance_type` = the size slug, e.g. `gpu-h100x1-80gb` or `gpu-l40sx1-48gb`
- `region` = the region slug (`tor1`, `nyc2`, `atl1`, …); there are no zones
- ordered by hourly price, cheapest first

Without `--region`, only **North American regions** (New York, San Francisco,
Toronto, Atlanta, Richmond) are considered. Pass `--region` explicitly for any
other region, e.g. `--region ams3`. `--zone` also pins a region, because
DigitalOcean has no zones.

`--gpus` matches the GPU model by whole tokens: `L40` does not match `L40S`,
`RTX4000` and `RTX-4000-Ada` both match `nvidia_rtx_4000_ada`, `MI300X` matches
`amd_mi300x`, and the count must equal the size's GPU count (`H100:1` vs
`H100:8`). `--instance-type` takes an exact size slug, including CPU sizes such
as `s-2vcpu-4gb`.

Availability is re-checked right before launching, so a stale offer fails before
anything is registered or created. A create that DigitalOcean rejects because
the size is not available in the region fails over to the next offer.

## What gets created

| Resource | Notes |
|---|---|
| Droplet | named `gpuprov-<name>`, tagged `gpuprov` and `gpuprov-vm-<name>` |
| tag `gpuprov-vm-<name>` | the VM's identity; created before the firewall so the firewall can reference it, deleted on teardown |
| cloud firewall | always one per VM, named `gpuprov-<name>` and applied to the VM's tag; allows inbound SSH, ICMP and any `--port`s from anywhere (IPv4 and IPv6), and all outbound traffic; deleted on teardown |
| SSH key | your gpuprov public key, registered once and reused; never removed |

The firewall is always created because a cloud firewall drops any inbound
traffic it doesn't list, and gpuprov needs to know exactly which ports are open.

`--image` takes an image slug (e.g. `ubuntu-24-04-x64`) or a numeric image id.
Without it, gpuprov follows
[DigitalOcean's GPU guidance](https://docs.digitalocean.com/products/droplets/getting-started/recommended-gpu-setup/):

| Size | Image |
|---|---|
| NVIDIA, 1 GPU (any model) | `gpu-h100x1-base` |
| NVIDIA, multiple GPUs | `gpu-h100x8-base` |
| AMD | `gpu-amd-base` |
| CPU | `ubuntu-24-04-x64` |

`--disk` has no effect. A Droplet's disk is fixed by its size, and for GPU sizes
it includes a large local scratch disk.

## SSH

The login is **`root`** on port 22. DigitalOcean installs the SSH keys passed at
creation for `root` on every image, so the login doesn't depend on cloud-init.
gpuprov's cloud-init user data still creates the usual `ubuntu` user with your
key.

## Kubernetes

Droplets accept cloud-init user data, so the k3s installer runs while the
Droplet boots and gpuprov [verifies it over SSH](../guides/kubernetes.md#how-it-gets-installed).
On NVIDIA images the installer sees `nvidia-smi` and adds the NVIDIA runtime and
device plugin. The cloud firewall attaches per VM, so clusters use **direct**
API access: ports 6443, 80 and 443 are opened in the VM's firewall.

## Billing and health

DigitalOcean bills a Droplet for as long as it exists, **including while it is
powered off**. `gpuprov health` reports a powered-off Droplet as UNHEALTHY and
says it is still billing. Run `gpuprov down` to stop the charges. A Droplet that
is locked while an action runs (resize, snapshot) is reported as information.

## Limits

| Feature | Why |
|---|---|
| `--spot` | DigitalOcean offers on-demand Droplets only |

API rate limits are 5,000 requests per hour and 250 per minute. When gpuprov is
throttled (`429`), it retries the step.

## Teardown

`gpuprov down` deletes the Droplet and waits until DigitalOcean no longer lists
it. It then deletes the VM's cloud firewall and its `gpuprov-vm-<name>` tag.
