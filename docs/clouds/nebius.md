# Nebius

[Nebius AI Cloud](https://nebius.com) rents full VMs on NVIDIA GPU platforms:
B300, B200, H200, H100, RTX PRO 6000 and L40S, in regions across Europe, the
US, Israel and the UK. Each platform has fixed sizes, called *presets*, from 1
to 8 GPUs. gpuprov boots Nebius's Ubuntu 24.04 image for NVIDIA GPUs, which
has the driver, CUDA and the NVIDIA Container Toolkit preinstalled. VMs can be
on-demand or preemptible (`--spot`), and preemptible VMs pay a live
[spot price](https://nebius.com/spot-prices).

```bash
nebius profile create                     # once; or export NEBIUS_IAM_TOKEN=...
gpuprov provision --cloud nebius --name dev --gpus H100:1
gpuprov provision --cloud nebius --name big --gpus B300:8 --region us-north1 --spot
```

!!! note "Acceptance testing pending"
    This adapter is covered by unit, mock-API and
    [vendor-stub](../development/vendor-stubs.md) tests. It has not yet passed
    the end-to-end acceptance run against a live account (allocate → SSH →
    k3s/GPU ready → terminate → nothing billable left), so treat it as preview.

## Credentials, tenant and projects

gpuprov uses the Nebius REST API (`https://api.nebius.cloud`) with an IAM
access token. It looks for one in this order:

1. `[nebius] token` in the config file
2. `$NEBIUS_IAM_TOKEN`
3. `nebius iam get-access-token`, run with `[nebius] profile` if you set one.
   The token is cached for an hour (tokens are valid for 12).

The CLI route covers user accounts and service-account profiles alike. A
service account needs a group with the `editor` role in the tenant or project.

Nebius projects are **regional**: a tenant has one project per region. gpuprov
lists the tenant's active projects and maps them by region, so a region
(`eu-north1`, `us-central1`, …) is all you name. If a region has several
projects, the one with the lowest id is used. The tenant comes from
`[nebius] tenant_id`, then `$NEBIUS_TENANT_ID`; failing both, it is the only
tenant your credentials can see. To use specific projects and skip discovery,
pin them:

```toml
[nebius]
projects = { eu-north1 = "project-e00…", us-central1 = "project-e00…" }
```

## Planning: live platforms, prices and capacity

For each candidate region, gpuprov lists the platforms and presets available
in that region's project. Each preset that matches the ask becomes one offer:

- `instance_type` = `platform/preset`, e.g. `gpu-h100-sxm/1gpu-16vcpu-200gb`
- `region` = the project's region; there are no zones (`--zone` pins a region)
- ordered by hourly price, cheapest first

Without `--region`, every region where you have a project is a candidate.

**Prices.** On-demand offers are priced in one batch by Nebius's billing
calculator. Preemptible offers use the public spot-price list, which quotes
one GPU hour per platform and region, multiplied by the preset's GPU count. An
offer that can't be priced is kept and tried last.

**Capacity advisor.** When the tenant is known, gpuprov reads Nebius's
[capacity advisor](https://docs.nebius.com/compute/virtual-machines/capacity-advisor)
for your quotas and current stock. It drops an offer whose fresh advice says
nothing can launch, and among offers at the same price it tries the more
available one first. The advisor is only a hint: if it can't be read (for
example, without permission), planning goes on without it.

`--gpus` matches the platform by whole tokens of its id or name: `H100` matches
`gpu-h100-sxm` but not `gpu-h200-sxm`, `L40` doesn't match `L40S`, and
`RTX6000` and `RTXPRO6000` both match `gpu-rtx6000`. The count must equal the
preset's GPU count (`H200:1` or `H200:8`). `--instance-type` takes
`platform/preset`, or a bare platform for all of its presets. `--spot` keeps
only platforms that allow preemptible VMs.

When a launch fails with Nebius's "not enough resources" error, gpuprov fails
over to the next offer. A quota error moves on to the next region.

## What gets created

| Resource | Notes |
|---|---|
| Compute instance | named `gpuprov-<name>`, labeled `gpuprov-vm-name=<name>`; one network interface on the project's default subnet with a dynamic public IP |
| boot disk | VM-managed `NETWORK_SSD` disk of `--disk` GiB, named `gpuprov-<name>-boot`; created and deleted together with the instance |

Nothing else is created. The subnet belongs to the project. Every VM is in its
network's default security group, which Nebius defines as allowing all
traffic and which can't be changed. `--port` therefore needs no firewall, and
the VM is reachable on **every** port from the internet. Secure the services
you run on it.

`--image` takes an image family (e.g. `ubuntu24.04-cuda12`) or an image id
(`computeimage-…`). Without it, gpuprov follows
[Nebius's recommendation](https://docs.nebius.com/compute/storage/boot-disk-images):
`ubuntu24.04-cuda13.0` on GPU platforms and `ubuntu24.04-driverless` on
CPU-only ones.

## SSH

The login is the spec's user (default **`ubuntu`**) on port 22, created with
your key by cloud-init.

## Kubernetes

Instances accept cloud-init user data, so the k3s installer runs while the VM
boots and gpuprov [verifies it over SSH](../guides/kubernetes.md#how-it-gets-installed).
The GPU image already has the driver, so the installer adds the NVIDIA runtime
and device plugin. Clusters use **direct** API access on port 6443.

## Spot (preemptible) VMs

`--spot` creates a preemptible VM that follows the current spot price, with
recovery policy `FAIL`, as Nebius requires. On preemption Nebius **stops** the
VM rather than deleting it. A stopped VM's compute isn't billed, but its boot
disk is, and its quota stays held. `gpuprov health` reports a preempted VM as
UNHEALTHY. Run `gpuprov down` to delete it.

## Health

Besides the instance state, `gpuprov health` reports:

- preemption: a stopped preemptible VM is critical
- a stopped on-demand VM (still holding quota and disk) is critical
- a maintenance event that affects the VM is a warning

## Teardown

`gpuprov down` deletes the instance, which also deletes its VM-managed boot
disk, then waits until Nebius no longer lists it. There is nothing else to
clean up.
