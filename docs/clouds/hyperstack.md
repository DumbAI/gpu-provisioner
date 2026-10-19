# Hyperstack

[Hyperstack](https://www.hyperstack.cloud) rents GPU virtual machines by the
hour in three regions: `CANADA-1` (Quebec), `US-1` (Texas) and `NORWAY-1`.
For small workloads, Canada is the useful one, with inexpensive single-GPU
flavors such as RTX A6000 48 GB, L40 48 GB and RTX PRO 6000 96 GB. Texas mostly
offers eight-GPU H100/A100 configurations.

```bash
export HYPERSTACK_API_KEY=...          # or [hyperstack] api_key in config.toml
gpuprov provision --cloud hyperstack --name dev --gpus A6000:1
gpuprov provision --cloud hyperstack --name big --gpus L40:1 --region CANADA-1
gpuprov provision --cloud hyperstack --name eu --gpus A6000:1 --region NORWAY-1
```

!!! warning "Acceptance testing pending"
    The adapter is built and tested against Hyperstack's published API
    (official OpenAPI spec), but it has not yet passed a live acceptance run:
    fresh account → API key → one GPU → SSH → k3s/GPU readiness → terminate →
    no billable resources left, plus an interrupted provision. Treat it as
    preview until that run is recorded.

## Planning: flavor ⨝ stock ⨝ price

An offer is emitted only when three live sources agree:

1. **Flavor** (`GET /core/flavors`): the configuration exists in the region and
   reports `stock_available`.
2. **Stock** (`GET /core/stocks`): when the region lists the GPU model, it must
   have a free configuration of that GPU count (`1x`, `2x`, `4x`, `8x`).
3. **Price** (`GET /pricebook`): the per-GPU hourly rate × GPU count. Spot GPUs
   are priced under `<gpu>-spot`.

Each surviving flavor × region is one offer, cheapest first:

- `instance_type` = the flavor name, e.g. `n3-RTX-A6000x1`
- `region` = `CANADA-1`, `US-1` or `NORWAY-1`

**Default regions are the North American ones (`CANADA-1`, `US-1`).** Norway
is only searched when you ask for it with `--region NORWAY-1`, so a Norway-only
flavor never appears in a North American plan.

`--gpus A6000:1` matches the `RTX-A6000` GPU on whole name tokens (`A100-80GB`
matches `A100-80G-PCIe`; `L40` does not match `L40S`). `--instance-type` takes
an exact flavor name. `--spot` switches to the spot flavor family (flavors with
`spot` in their name).

Stock is re-checked for the chosen flavor right before deploying, so a stale
offer fails without creating anything. A deploy that hits a stock-out moves on
to the next offer.

## What gets created

| Resource | Notes |
|---|---|
| VM | named `gpuprov-<name>`, labeled `gpuprov-vm-name=<name>` and `gpuprov-managed`, with a public IP |
| firewall rules | inbound TCP 22 plus requested ports, set in the create call |
| environment | `gpuprov-<region>` (e.g. `gpuprov-canada-1`), created once per region and shared by gpuprov VMs there |
| key pair | your gpuprov public key, imported once per environment and reused |

The image is the newest Ubuntu with the newest CUDA for GPU flavors (the
variant without Docker on a tie; k3s brings its own runtime), plain Ubuntu for
CPU flavors, or whatever the flavor is hard-restricted to. `--image` takes any
Hyperstack image name. You log in as `ubuntu` on port 22: gpuprov disables the
API's port randomization when it assigns the public IP.

## Kubernetes

Hyperstack VMs boot with cloud-init, so the k3s installer rides in the VM's user
data and k3s installs while the VM boots. gpuprov then
[verifies the install over SSH](../guides/kubernetes.md#how-it-gets-installed).
Firewall rules are part of the create call, so the API (6443) and ingress
(80/443) are opened and clusters use **direct** access.

## Teardown

`gpuprov down` deletes the VM (a VM that is already gone counts as deleted).
Its public IP goes with it. The shared environment and key pair stay for the
next VM in that region; delete them in the Hyperstack console if you want
them gone.

## Open questions for the acceptance run

- Whether inline `security_rules` at create time are applied as documented, or
  whether rules must be added after creation (`POST …/sg-rules`).
- That `enable_port_randomization: false` keeps SSH on port 22 of the public IP.
- The exact pricebook names for spot GPUs (`<gpu>-spot` per the billing docs).
