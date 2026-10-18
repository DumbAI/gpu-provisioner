# GCP

```bash
gcloud auth login && gcloud config set project my-project
gpuprov provision --cloud gcp --name dev --gpus L4:1 --region us-central1 --region us-east1
```

The adapter calls the Compute Engine v1 REST API directly. Authentication uses an
OAuth access token from `[gcp] access_token`, `$GOOGLE_OAUTH_ACCESS_TOKEN`, or
`gcloud auth print-access-token` (cached and refreshed). Leaving out
service-account key signing keeps the binary free of crypto dependencies.

## Planning

GCP capacity is **zonal**, so offers are per zone. The GPU ladder below is crossed
with the zones of each region, but only zones that actually offer the machine
type (live `aggregated/machineTypes` / `aggregated/acceleratorTypes`) are kept.

| GPU | count → machine types |
|---|---|
| T4 | 1 → n1-standard-4/8/16 · 2 → n1-standard-8/16 · 4 → n1-standard-16/32 (T4 attached as a guest accelerator) |
| L4 | 1 → g2-standard-4 … g2-standard-32 · 2 → g2-standard-24 · 4 → g2-standard-48 · 8 → g2-standard-96 |
| A100 | 1/2/4/8 → a2-highgpu-{1,2,4,8}g |
| A100-80GB | 1/2/4/8 → a2-ultragpu-{1,2,4,8}g |
| H100 | 1/2/4/8 → a3-highgpu-{1,2,4,8}g |

## Prepare / create

- `prepare` makes sure a per-VM firewall rule `gpuprov-<name>` exists, allowing
  tcp:22 plus requested ports to the instance's network tag. It also resolves the
  image: the Deep Learning VM family
  `common-cu129-ubuntu-2204-nvidia-580` for GPU types (CUDA 12.9, driver 580), and
  `ubuntu-2204-lts` otherwise, unless you pass `--image`.
- `create` runs `instances.insert` with labels `gpuprov-vm-name`/`gpuprov-managed`,
  `ssh-keys` and cloud-init `user-data` metadata, a public NAT IP, and
  `onHostMaintenance=TERMINATE` for GPUs. `--spot` gives a SPOT instance that is
  deleted on preemption. It then waits for the zone operation, a bounded wait,
  because stockouts only show up in the finished operation.

## Error mapping

| Operation / HTTP error | Kind | Effect |
|---|---|---|
| `ZONE_RESOURCE_POOL_EXHAUSTED(_WITH_DETAILS)`, `UNSUPPORTED_OPERATION`, `RESOURCE_NOT_READY` | Capacity | next zone |
| `QUOTA_EXCEEDED` (incl. `GPUS_ALL_REGIONS`) | Quota | skip the region |
| `VPC_NOT_FOUND` | Unavailable | skip the region |
| 429 / 5xx / network | Transient | retry |
| 401, non-quota 403 | Credentials | abort |
