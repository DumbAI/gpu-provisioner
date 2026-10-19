# Massed Compute

[Massed Compute](https://massedcompute.com) rents full GPU VMs by the hour from
its US data centers, with a strong low-cost range of 24–48 GB cards (RTX A5000,
RTX A6000, L40S) and A100s. It's a good default for "a usable GPU development
machine for a few dollars per session."

```bash
export MASSED_COMPUTE_API_KEY=...     # or [massedcompute] api_key
gpuprov provision --cloud massedcompute --name dev --gpus A6000:1
gpuprov provision --cloud massedcompute --name big --gpus A100-80GB:1
gpuprov ssh dev -- nvidia-smi
```

Create the token under **Settings → API** at
[vm.massedcompute.com/settings/api](https://vm.massedcompute.com/settings/api)
with **Full access**. A read-only token can plan but not launch.

## Planning: live inventory

gpuprov reads the current inventory (`GET /api/v1/gpu-inventory`). Every product
with capacity becomes one offer:

- `instance_type` = the product, e.g. `gpu_1x_a6000` (1× RTX A6000)
- `region` = `any`, because the launch API doesn't let you choose a data center
  and Massed Compute places the VM
- ordered by hourly price, cheapest first

`--gpus A6000:2` picks products with exactly two RTX A6000s. GPU names match on
whole tokens, so `L40` does not match `L40S` and `A10` does not match `A100`.
`--instance-type` takes an exact product name, CPU products included.

`--region` keeps only products that currently have capacity in one of the named
regions (e.g. `us-central-3`), but cannot pin placement. Massed Compute's data
centers are all in the US, so there is no default region filter.

Capacity is re-checked right before launching, so a stale offer fails without
creating anything.

## What gets created

| Resource | Notes |
|---|---|
| instance | named `gpuprov-<name>`; matched by that name |
| SSH key | your gpuprov public key, registered once under `gpuprov-<hash>` and reused (any existing entry with the same key is reused as-is); never removed |

The image defaults to the newest plain Ubuntu image from `GET /api/v1/images`.
Desktop and app-specific stacks are skipped. `--image` takes an image id or an
exact image name.

## SSH

You log in on port 22 as the user the instance reports, which is `Ubuntu` on the
Ubuntu images (capital U). gpuprov records it, so `gpuprov ssh <name>` and plain
`ssh <name>` just work.

## Kubernetes

Instances are full VMs, so the default k3s cluster works. The installer is
[pushed over SSH](../guides/kubernetes.md#how-it-gets-installed) after the VM is
up. The API has a startup command, but its run-as user, size limit, and whether
it runs on every boot aren't documented, so gpuprov doesn't rely on it.

There is no firewall API, so clusters use
[SSH-tunnel access](../guides/kubernetes.md#access-modes) (`gpuprov tunnel <name>`).

## Limits

Rejected up front:

| Feature | Why |
|---|---|
| `--port` | no firewall API: ports other than SSH cannot be opened or guaranteed reachable |
| `--spot` | only on-demand instances are offered |

Things to know:

- **Stopped VMs keep billing.** gpuprov only ever terminates.
- A low account balance ("Insufficient Funds") ends a launch immediately with a
  clear message.
- The running-instance cap tied to your billing *Recharge Amount* is reported as
  a quota error. Raise the recharge amount on the Billing page.
- Disk size (`--disk`) is fixed by the product; the API has no disk option.

!!! note "Acceptance testing pending"
    This adapter follows the published VM API reference. It hasn't yet been run
    end to end against a real account (allocate → SSH → k3s/GPU ready →
    terminate → no billable resources left, plus an interrupted launch).
