# Prime Intellect

[Prime Intellect](https://www.primeintellect.ai) is a GPU **marketplace**. It
resells capacity from upstream providers (Hyperstack, Lambda, DataCrunch,
Nebius, …) behind one API, which makes it a natural fit for failover across
providers.

```bash
export PRIME_API_KEY=...          # or `prime login`, or [primeintellect] api_key
gpuprov provision --cloud primeintellect --name dev --gpus H100:1
gpuprov provision --cloud primeintellect --name dev2 --gpus A100:8 --region united_states
```

## Planning: live availability

`gpuprov` queries `GET /api/v1/availability/gpus` live instead of shipping a
static catalog. A marketplace's stock changes minute to minute, so a static
catalog would be stale on arrival. Each in-stock offer becomes one
candidate:

- `instance_type` = `<provider>/<cloudId>` (e.g. `hyperstack/n3-H100x1`)
- `region` = a Prime Intellect region id (`united_states`, `canada`, `eu_west`, …).
  No `--region` means anywhere.
- `zone` = the data center id, which `--zone` pins
- ordered by on-demand price, cheapest first

GPU names match on a boundary: `H100` matches `H100_80GB`, `A100` matches both
A100 sizes, `H200` does not match `GH200`, and `A10` does not match `A100`.
`--instance-type` accepts `<provider>/<cloudId>` or a bare `cloudId`.

## Prepare / create

- `prepare` looks up or registers our public key under `/ssh_keys` and passes its
  id per pod (`sshKeyId`), leaving the account's primary key alone. It then
  **re-checks stock** for that exact offer, so a stale plan fails before anything
  is created.
- `create` posts `/pods` using the offer's own `socket` (SXM offers need the
  right value; a fixed `PCIe` would be wrong for them), data center, security
  tier, and image.
  Pods are named `gpuprov-<name>`.

## SSH and Kubernetes

The SSH user and port come from the pod's `sshConnection` (often `root` and a
NAT port), not from `--ssh-user`. Pods expose SSH only, so `--port` is rejected,
and `--k8s` clusters use [SSH-tunnel access](../guides/kubernetes.md#access-modes).

## Limits

These are declared as unsupported features and rejected up front: `--port`
(no firewall API), `--image` (images come from the offer's environment list),
and `--spot`. Pods can take up to about 16 minutes to become active, so the boot budget is
16 minutes.
