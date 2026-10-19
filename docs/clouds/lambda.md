# Lambda

[Lambda](https://lambda.ai) rents on-demand GPU instances (full VMs) in several
US regions, billed per minute. Instances boot the Lambda Stack image
(Ubuntu with NVIDIA drivers, CUDA and common ML frameworks).

```bash
export LAMBDA_API_KEY=...          # or [lambda] api_key in config.toml
gpuprov provision --cloud lambda --name dev --gpus A6000:1
gpuprov provision --cloud lambda --name big --gpus H100:8 --region us-east-1
```

Create an API key at <https://cloud.lambda.ai/api-keys>.

!!! note "Acceptance testing pending"
    This adapter is covered by unit and mock-API tests. It has not yet passed
    the end-to-end acceptance run against a live account (allocate → SSH →
    k3s/GPU ready → terminate → nothing billable left), so treat it as preview.

## Planning: live capacity

One call, `GET /api/v1/instance-types`, returns every instance type with its
price and the regions that can launch it right now. Each type × region with
capacity becomes one offer:

- `instance_type` = Lambda's type, e.g. `gpu_1x_a6000` or `gpu_8x_h100_sxm5`
- `region` = Lambda region (`us-east-1`, `us-west-1`, …); there are no zones
- ordered by hourly price, cheapest first

Without `--region`, only **US regions** (`us-*`) are considered. Pass
`--region` explicitly for any other region. `--zone` also pins a region,
because Lambda has no zones.

`--gpus` matches the GPU description by whole tokens: `A10` does not match
`A100`, `A6000` matches the RTX A6000, `A100-80GB` matches only the 80 GB A100,
and `H100:1` picks the single-GPU PCIe type while `H100:8` picks the 8× SXM5
box. `--instance-type` takes an exact type name, including CPU types.

Capacity is re-checked right before launching, so a stale offer fails before
anything is registered or created.

## What gets created

| Resource | Notes |
|---|---|
| instance | named and hostnamed `gpuprov-<name>`, tagged `gpuprov-vm-name=<name>` and `gpuprov-managed=true` |
| firewall ruleset | only when extra ports are needed (`--port`, or the k3s API); named `gpuprov-<name>`, allows SSH plus those ports, deleted on teardown |
| SSH key | your gpuprov public key, registered once and reused; never removed |

The login user is `ubuntu`, created by gpuprov's cloud-init user data. `--image`
takes a Lambda image family (e.g. `lambda-stack-24-04`) or an image id.

## Kubernetes

Launches accept cloud-init user data, so the k3s installer runs while the VM
boots and gpuprov [verifies it over SSH](../guides/kubernetes.md#how-it-gets-installed).
Firewall rulesets attach per instance, so clusters use **direct** API access:
ports 6443, 80 and 443 are opened in the VM's ruleset.

## Limits

| Feature | Why |
|---|---|
| `--spot` | Lambda offers on-demand instances only |

Lambda's legacy account-wide firewall rules do not apply in `us-south-1`, and
gpuprov uses per-instance rulesets instead. If a region rejects ruleset
creation, that offer is skipped and the next one is tried.

## Teardown

`gpuprov down` terminates the instance, waits until Lambda reports it gone, then
deletes the VM's firewall ruleset in that region. While the instance is still
terminating the ruleset is "in use", so the delete is retried until it frees up.
