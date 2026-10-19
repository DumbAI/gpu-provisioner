# Clouds

Every cloud is an adapter behind the same six-primitive
[`CloudProvider`](../architecture/adding-a-cloud.md) contract. The shared state
machines supply failover, retries, compensation, and crash recovery; each
adapter only maps a cloud's API onto the contract. Pick one with `--cloud <id>`.

## Pick a cloud

| For… | Use |
|---|---|
| cheap 24–48 GB NVIDIA GPUs for a development session | `massedcompute` (US), `hyperstack` (Canada) |
| a second, independent US source when one is sold out | `lambda` |
| the hyperscalers you already have accounts with | `aws`, `gcp` |
| an existing DigitalOcean account; Toronto RTX Ada / L40S, US H100 / MI300X | `digitalocean` |
| European capacity, spot pricing | `verda` |
| marketplace breadth (many upstream providers) | `primeintellect`, `runpod` |
| trying gpuprov without any account | `docker` |

Prices and stock change minute to minute. Every adapter except AWS and GCP plans
from **live** availability, so the offers are what can actually be launched now,
cheapest first.

## At a glance

| Cloud | `--cloud` | What a VM is | Regions (`--region`) | Default when none given | Planning |
|---|---|---|---|---|---|
| [AWS](aws.md) | `aws` (default) | EC2 instance | `us-west-2`, … | required | static GPU ladder |
| [GCP](gcp.md) | `gcp` | Compute Engine instance | `us-central1`, … (zone per offer) | required | static ladder + live zone offerings |
| [Massed Compute](massedcompute.md) | `massedcompute` | VM | `any`; Massed chooses the placement | anywhere (US) | live inventory |
| [Hyperstack](hyperstack.md) | `hyperstack` | VM | `CANADA-1`, `US-1`, `NORWAY-1` | `CANADA-1`, `US-1` | live flavor ⨝ stock ⨝ price |
| [Lambda](lambda.md) | `lambda` | VM | `us-east-1`, … | US regions | live capacity per region |
| [DigitalOcean](digitalocean.md) | `digitalocean` | GPU Droplet | `tor1`, `nyc2`, `atl1`, … | North American regions | live sizes × regions |
| [Verda](verda.md) | `verda` | VM | location `FIN-01`, … or country `FI` | anywhere | live per-location availability |
| [Prime Intellect](primeintellect.md) | `primeintellect` | marketplace pod (VM) | `united_states`, `canada`, … | anywhere | live marketplace availability |
| [RunPod](runpod.md) | `runpod` | GPU container (pod) | continent, or country (`US`) | anywhere | live per-data-center stock |
| [Docker](docker.md) | `docker` | local sshd container | `local` | `local` | one offer |

## Capabilities

| Cloud | k3s (on by default) | k8s API access | `--port` | `--spot` | `--image` | Login | Per-VM side resources |
|---|---|---|---|---|---|---|---|
| AWS | ✓ at boot (cloud-init) | direct | ✓ security group | ✓ | AMI id | `ubuntu`@22 | security group |
| GCP | ✓ at boot (cloud-init) | direct | ✓ firewall rule | ✓ | image URL / family | `ubuntu`@22 | firewall rule |
| Massed Compute | ✓ over SSH | SSH tunnel | ✗ | ✗ | image id / name | `Ubuntu`@22 | — |
| Hyperstack | ✓ at boot (cloud-init) | direct | ✓ VM security rules | ✓ spot flavors | image name | `ubuntu`@22 | — (environment and key pair are shared per region) |
| Lambda | ✓ at boot (cloud-init) | direct | ✓ per-VM firewall ruleset | ✗ | image id / family | `ubuntu`@22 | firewall ruleset (when ports are opened) |
| DigitalOcean | ✓ at boot (cloud-init) | direct | ✓ per-VM cloud firewall | ✗ | image slug / id | `root`@22 | cloud firewall (always: SSH + requested ports) and per-VM tag |
| Verda | ✓ at boot (startup script) | SSH tunnel | ✗ | ✓ | image type | `root`@22 | OS volume, startup script |
| Prime Intellect | ✓ over SSH | SSH tunnel | ✗ | ✗ | ✗ | dictated by the pod | — |
| RunPod | ✗ (containers, no systemd) | — | ✗ | ✗ | container image | `root`@random port | — |
| Docker | ✗ (no systemd) | — | ✗ | ✗ | ✗ | `ubuntu`@random local port | — |

Unsupported combinations are rejected **before** planning, with the cloud's
reason. For example, `--port 8080` on Massed Compute is refused because there is
no firewall API. The default k3s cluster is skipped, with a note, only on clouds
that cannot host it.

## Support status

Adapters are tested against each cloud's documented API, with pure tests plus
mock HTTP servers. A cloud is labeled **supported** only after a real-account run
of the [acceptance suite](../development/acceptance.md): allocate, SSH, k3s +
GPU, terminate, verify nothing billable remains, and interrupt-midway cleanup.
See that page for the current results.
