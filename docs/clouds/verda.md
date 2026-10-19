# Verda

[Verda](https://verda.com) is a GPU cloud with dedicated GPU and CPU instances
(real VMs) in European data centers. Instances are billed hourly, either on
demand or as **spot** capacity.

```bash
export VERDA_CLIENT_ID=... VERDA_CLIENT_SECRET=...   # or [verda] in config.toml
gpuprov provision --cloud verda --name dev --gpus H100:1
gpuprov provision --cloud verda --name cheap --gpus A100:1 --spot --region FI
```

Credentials are the **Cloud API credentials** (client ID and secret) from the
Verda console's Credentials page. gpuprov exchanges them for a short-lived
OAuth token and refreshes it automatically.

## Planning: live availability

gpuprov reads the instance-type catalog and the current per-location
availability (`GET /v1/instance-availability`, spot or on-demand to match
`--spot`). Each location that has the type in stock becomes one offer:

- `instance_type` = Verda's type, e.g. `1H100.80S.22V` (1× H100 SXM5, 22 vCPUs)
- `region` = the location code (`FIN-01`, `FIN-02`, `ICE-01`, …)
- ordered by hourly price (the spot price with `--spot`), cheapest first

`--region` accepts location codes or ISO country codes (`--region FI`). No
`--region` means anywhere. `--gpus H100:8` picks types with exactly eight H100s;
`--instance-type` takes an exact type, including CPU-only types.

Stock is re-checked for the chosen location right before deploying, so a stale
offer fails without creating anything. A deploy that hits "no capacity" moves
on to the next offer.

## What gets created

| Resource | Notes |
|---|---|
| instance | tagged `gpuprov-vm-name=<name>` and `gpuprov-managed=true`; hostname `gpuprov-<name>` |
| OS volume | `--disk` GB, tagged the same; deleted permanently with the instance |
| startup script | the k3s installer (see below), one per VM, removed on teardown |
| SSH key | your gpuprov public key, registered once and reused; never removed |

The image is picked from the instance type's supported OS list: the newest
Ubuntu with CUDA for GPU types, plain Ubuntu for CPU types. `--image` takes any
Verda image type. You log in as `root`.

## Kubernetes

Verda runs a startup script as root on first boot, so the k3s installer goes in
as a startup script and k3s installs while the VM boots. gpuprov then
[verifies the install over SSH](../guides/kubernetes.md#how-it-gets-installed)
and falls back to pushing the installer itself if needed.

Verda has no firewall API, so gpuprov cannot guarantee that ports other than SSH
are reachable. Clusters therefore use [SSH-tunnel access](../guides/kubernetes.md#access-modes)
(`gpuprov tunnel <name>`), and `--port` is rejected.

## Teardown

`gpuprov down` deletes the instance together with its volumes (permanently, not
to the trash). It then sweeps any volume still tagged for the VM, plus the VM's
startup scripts. Teardown is retried until Verda confirms the instance is gone;
otherwise the VM is recorded as `LEAKED`.
