# Vendor API stubs

Every cloud adapter is tested at three levels:

| Level | What runs | Catches |
|---|---|---|
| unit / mock | one adapter call against a canned `wiremock` response | parsing and error classification of single calls |
| **vendor stubs** (this page) | the **real** adapter + launch/teardown state machines against a **stateful** simulation of the vendor's API | cross-call bugs: an instance that never shows up in the list, a delete that leaves a volume behind, a retried create that duplicates, a boot script that never reaches the VM |
| [acceptance](acceptance.md) | the real cloud with a real account | everything the documentation doesn't tell you |

## How it works

`tests/vendor_stubs.rs` is the integration-test crate. Under `tests/stubs/`:

- `route.rs` — a small stateful layer on `wiremock`: one simulated account
  per stub, with requests routed to `Fn(&Request, &mut Account) -> Response`.
- `harness.rs` — the [`VendorStub`] trait and the scenarios, written once.
- `<vendor>.rs` — one module per vendor. Each module doc has a table mapping
  **every endpoint the adapter calls to the vendor's API reference page**,
  with the behavior simulated. Response shapes follow the vendor's spec, not
  gpuprov's parser, so the stubs check the adapters independently.

The adapter is built through `provider::build` from an `AppConfig` whose
`base_url` points at the stub, which is the same configuration path users take.
SSH is the only other simulated piece: an accepting connector that records the
endpoint it was asked to reach, so the login the vendor reports is still
checked.

## Scenarios (every vendor)

| Scenario | Proves |
|---|---|
| lifecycle | plan → create → boot → ready, with the vendor-reported SSH user/port → `down`, and the simulated account holds **no per-VM billable resources** afterwards |
| capacity failover | the vendor's documented "no capacity" response on create → compensation → the next offer launches; the failed attempt created nothing |
| interrupt | cancel (Ctrl-C) while the instance boots → teardown → nothing left |
| reuse | provisioning the same name again adopts the running instance, with no second create |
| boot script | on clouds that run one, the boot script (body and env) reaches the vendor in its native form, and teardown removes every boot-script resource; other clouds skip it |

The leak check has teeth. Breaking a stub so deletes trash Verda volumes instead
of deleting them fails the lifecycle with `billable resources left behind`, and
dropping Lambda's `user_data` fails the boot-script scenario.

## Coverage

| Vendor | Stub | API reference the stub follows | Faults simulated | Documented assumptions |
|---|---|---|---|---|
| RunPod | `runpod.rs` | [REST v2](https://docs.runpod.io/api-reference-v2) | capacity (`ErrorResponse`), cursor pagination (page size 1) | capacity status/body not documented |
| Prime Intellect | `primeintellect.rs` | [API reference](https://docs.primeintellect.ai/api-reference) | capacity, offset pagination, `DELETING` → gone | capacity body and delete transition not documented |
| Verda | `verda.rs` | [v1 API](https://api.verda.com/v1/docs) | 503 `service_unavailable` capacity, OAuth token required, trashed vs permanent volume delete | `ip` untyped in the spec |
| Massed Compute | `massedcompute.rs` | [API v1](https://vm-docs.massedcompute.com/api/v1) | "No Availability" launch error, unknown SSH key names, `os_booted` boot flag | deploy-error status/body not documented; only `rented` status documented |
| Hyperstack | `hyperstack.rs` | [Infrahub API](https://docs.hyperstack.cloud/docs/api-reference/) ([OpenAPI](https://github.com/NexGenCloud/hyperstack-sdk-go/blob/main/api/openapi.yaml)) | `out_of_stock` on create, duplicate environment/keypair (409) | stock-out wording not documented; volumes/IPs not modelled separately |
| Lambda | `lambda.rs` | [Cloud API](https://docs.lambda.ai/api/cloud) | `insufficient-capacity`, ruleset `in-use` on delete, `page_token` pagination, plus a dedicated firewall-ruleset test | — |
| DigitalOcean | `digitalocean.rs` | [API v2](https://docs.digitalocean.com/reference/api/digitalocean/) | `422 unprocessable_entity` "Size is not available in this region." on create, `page`/`per_page` pagination (page size 1), Droplet still listed and `locked` for one list after delete, firewall referencing a missing tag (`422`), plus firewall and powered-off-health tests | capacity status/wording not documented; firewall requires an existing tag |
| GCP | `gcp.rs` | [Compute Engine REST v1](https://cloud.google.com/compute/docs/reference/rest/v1) | asynchronous `ZONE_RESOURCE_POOL_EXHAUSTED` on the insert operation | stockout HTTP code not documented |
| AWS | `tests/aws_moto.rs` | [EC2 API](https://docs.aws.amazon.com/AWSEC2/latest/APIReference/) via the [moto](https://github.com/getmoto/moto) emulator | — | moto can't return capacity errors (failover not run) and boots instantly (interrupt runs during the SSH wait) |

AWS uses moto, a maintained EC2/SSM emulator, rather than a hand-written stub:
the EC2 Query/XML protocol is not worth re-implementing. The test seeds what moto
lacks (an internet-gateway route; an explicit AMI, since moto refuses the public
SSM AMI parameters) through moto's own API.

## Running

```bash
cargo test --test vendor_stubs                 # all vendors (also part of `cargo test`)
cargo test --test vendor_stubs hyperstack      # one vendor

docker run -d --rm -p 127.0.0.1:5000:5000 motoserver/moto:5.2.3
GPUPROV_MOTO_ENDPOINT=http://127.0.0.1:5000 cargo test --test aws_moto -- --ignored
```

CI runs both in `.github/workflows/integration.yml` (jobs `vendor-stubs` and
`aws-moto`, the latter with moto as a service container).

## Adding a stub for a new cloud

1. Create `tests/stubs/<cloud>.rs` and add `mod <cloud>;` to `tests/stubs/mod.rs`.
2. Simulate the endpoints the adapter calls, following the vendor's reference,
   and document each one in the module's endpoint table with its doc link.
3. Implement `VendorStub`: seed at least two in-stock offers, inject the
   vendor's real capacity error, model boot and delete transitions, count every
   per-VM billable resource in `leftovers()`, and record boot payloads if the
   cloud runs boot scripts.
4. End the module with `vendor_suite!(<Stub>);`.
