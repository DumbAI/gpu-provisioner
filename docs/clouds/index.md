# Clouds

Every cloud is an adapter behind the same six-primitive
[`CloudProvider`](../architecture/adding-a-cloud.md) contract. The shared state
machines supply failover, retries, compensation, and crash recovery. The table
shows how the clouds differ.

| | AWS | GCP | Prime Intellect | Docker |
|---|---|---|---|---|
| `--cloud` | `aws` (default) | `gcp` | `primeintellect` | `docker` |
| What a "VM" is | EC2 instance | Compute Engine instance | marketplace pod | local sshd container |
| Region names | `us-west-2` | `us-central1` | `united_states`, `canada`, … | `local` |
| Zone | AZ (`us-west-2a`) | zone (`us-central1-a`) | data center id | — |
| Empty region list | error | error | anywhere | `local` |
| Catalog | static GPU ladder | static GPU ladder + live zone offerings | **live** availability API | — |
| SSH key delivery | cloud-init | `ssh-keys` metadata + cloud-init | API (`sshKeyId` per pod) | container env |
| SSH user / port | `ubuntu` / 22 | `ubuntu` / 22 | dictated by the pod | `ubuntu` / random local port |
| k3s (default on) | ✓, installed at boot | ✓, installed at boot | ✓, installed over SSH | ✗ (skipped) |
| `--port`, direct k8s API | ✓ (security group) | ✓ (firewall rule) | ✗ (k8s uses SSH tunnel) | ✗ |
| `--spot` | ✓ | ✓ (SPOT, delete on preemption) | ✗ | ✗ |
| `--image` | AMI id | image URL / family | ✗ | ✗ |
| Per-VM side resources | security group | firewall rule | — | — |

Unsupported combinations are rejected **before** planning, with the cloud's
reason. For example, `--port 8080` on Prime Intellect is refused because its pods
expose SSH only and there is no firewall API.
