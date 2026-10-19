# Provider acceptance

Unit and mock-server tests prove that an adapter speaks a cloud's API as
documented. They cannot prove that the cloud actually allocates, boots, and
cleans up reliably for a real account. Before a provider is labeled
**supported**, run the acceptance suite (`tests/acceptance.rs`) against it with
a fresh account.

## What it checks

| Test | Steps |
|---|---|
| `full_lifecycle` | allocate one VM for the ask → SSH login → `nvidia-smi -L` lists a GPU (GPU asks) → k3s installed and validated, GPU allocatable (where the cloud supports k3s) → terminate → the cloud reports **no instances left** and per-VM cleanup is idempotent |
| `interrupted_provision_cleans_up` | start a launch, "press Ctrl-C" once the instance is booting → the launch compensates and ends `cancelled` → nothing is left |

Each run uses a fresh temporary `GPUPROV_HOME` and a unique VM name, and prints
the journaled timeline (seconds since start for every transition). A drop guard
tears the VM down if a test fails midway, so a failed run doesn't keep billing.

## Running it

The suite creates **billable** resources, so it is `#[ignore]`d and also needs
`GPUPROV_ACCEPT=1`:

```bash
GPUPROV_ACCEPT=1 \
GPUPROV_ACCEPT_CLOUD=hyperstack \
GPUPROV_ACCEPT_GPUS=A6000:1 \
GPUPROV_ACCEPT_REGIONS=CANADA-1 \
  cargo test --test acceptance -- --ignored --nocapture --test-threads=1
```

| Variable | Meaning |
|---|---|
| `GPUPROV_ACCEPT_CLOUD` | provider id (required) |
| `GPUPROV_ACCEPT_GPUS` or `GPUPROV_ACCEPT_INSTANCE_TYPE` | the ask (exactly one) |
| `GPUPROV_ACCEPT_REGIONS` | comma-separated regions (else the cloud's default) |
| `GPUPROV_ACCEPT_K8S` | `0` to skip the k3s stage |
| `GPUPROV_ACCEPT_CONFIG` | config file with credentials (else `~/.gpu-provisioner/config.toml`, else env vars) |

Use the cheapest single-GPU offer the cloud has. The suite's value is in the
lifecycle, not the hardware.

## Afterwards

Also check the provider's console and billing page by hand: no instances,
volumes, IPs, or other billable resources should remain for the run's VM names
(`accept-…`, `accept-int-…`).

## Results

CI runs the suite against the Docker cloud on every push to keep the harness
itself working. Record real-account runs here; they decide whether a provider is
supported and the default provider ordering.

| Provider | Date | Ask / offer | Lifecycle | Interrupt | k3s + GPU | Nothing left (console) | Notes |
|---|---|---|---|---|---|---|---|
| Docker (CI) | every push | `container` | ✅ | ✅ | n/a | ✅ | harness self-test |
| Massed Compute | — | — | pending | pending | pending | pending | |
| Hyperstack (CANADA-1) | — | — | pending | pending | pending | pending | |
| Lambda | — | — | pending | pending | pending | pending | |
| DigitalOcean | — | — | pending | pending | pending | pending | |
| Verda | — | — | pending | pending | pending | pending | |
| RunPod | — | — | pending | pending | n/a | pending | |
| Prime Intellect | — | — | pending | pending | pending | pending | |
| GCP | — | — | pending | pending | pending | pending | |
| AWS | — | — | pending | pending | pending | pending | |
