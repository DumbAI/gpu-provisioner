# Adding a cloud

A cloud is one module under `src/provider/<id>/` that implements
`CloudProvider`, plus one line in `provider::build`. Nothing above the trait
changes. Failover, retries, polling, deadlines, compensation, crash recovery, the
journal, SSH, and k3s all come from the shared core.

```rust
#[async_trait]
pub trait CloudProvider: Send + Sync {
    fn name(&self) -> &'static str;                 // "mycloud": recorded on every VM row
    fn capabilities(&self) -> Capabilities;         // unsupported features + timing hints
    async fn plan(&self, req: &ResourceRequest) -> Result<Vec<Offer>, ProviderError>;
    async fn prepare(&self, offer: &Offer, spec: &InstanceSpec) -> Result<serde_json::Value, ProviderError>;
    async fn create(&self, offer: &Offer, prepared: &serde_json::Value, spec: &InstanceSpec) -> Result<String, ProviderError>;
    async fn observe(&self, at: &Location, vm_name: &str) -> Result<Vec<Observed>, ProviderError>;
    async fn delete(&self, at: &Location, vm_name: &str, ids: &[String]) -> Result<(), ProviderError>;
    async fn cleanup(&self, at: &Location, vm_name: &str) -> Result<(), ProviderError>;
    // optional — default derived from `observe`:
    async fn diagnose(&self, at: &Location, vm_name: &str, instance_id: Option<&str>) -> Result<Diagnosis, ProviderError>;
}
```

## The contract

| Primitive | Must | Must not |
|---|---|---|
| `plan` | return offers in preference order (cheapest first). Put anything `create` needs (marketplace ids, sockets) into `Offer::detail`. | create anything |
| `prepare` | be idempotent per (VM, location): firewall/security group, key registration, image resolution. Return private JSON; it is journaled. | create the instance |
| `create` | make **one** attempt and label everything with the VM identity (tag, label, or name `gpuprov-<vm>`) | wait for running, or retry |
| `observe` | return **all** non-terminated instances with that identity, with normalized `InstanceStatus`, and the `SshEndpoint` once reachable (user/port if the cloud dictates them) | filter out broken instances (report them as `Failed`) |
| `delete` | request deletion; unknown or already-gone ids count as success | wait |
| `cleanup` | delete per-VM side resources; idempotent | touch shared resources (keys, VPCs) |

Long waits belong to the core, which polls `observe` using your
`Capabilities { boot_timeout, delete_timeout, poll_interval }`. The one exception
is a bounded wait on a cloud operation whose *result* carries the error, as with
GCP's `operations.wait` after `instances.insert`.

## Health diagnosis (optional)

`gpuprov health` asks the adapter for a `Diagnosis`: the instance as the vendor
sees it (`None` if it no longer exists, which means gone) plus vendor-native
`Finding`s. The default implementation derives this from `observe`, so a new
cloud gets health checks for free. If the vendor exposes more, such as status
checks, scheduled maintenance, preemption reasons, or why an instance was
terminated, override `diagnose` and return findings:

| Severity | Use for | Verdict effect |
|---|---|---|
| `Finding::info` | context ("spot instance") | none |
| `Finding::warn` | usable but at risk (scheduled retirement, maintenance) | degraded |
| `Finding::critical` | the vendor considers it broken or reclaimed (failed status check, preempted) | unhealthy |

Keep it read-only. Recently terminated instances are worth listing here, even
though `observe` hides them, so a reclaim can be attributed ("terminated by the
vendor: spot capacity reclaimed").

## Classifying errors

Map every API failure to an `ErrorKind`. This is the entire failover policy, and
it lives in your `error.rs`:

| Kind | Use for | Core does |
|---|---|---|
| `Transient` | throttling, 5xx, timeouts | retries the step with backoff. `Creating` observes first, so a request that actually landed is adopted, not duplicated. |
| `Capacity` | stockouts | tears down, blocks the offer, and moves to the next |
| `Quota` | account limits | tears down and blocks the whole region (override with `with_block`) |
| `Unavailable` | not viable here and **nothing was created** | blocks the offer with no teardown |
| `Failed` | anything else after resources may exist | tears down, blocks the offer, and moves to the next |
| `InvalidRequest` | the ask can't be satisfied (unknown GPU, bad zone) | aborts |
| `Credentials` | auth failures | tears down what may exist, then aborts |

## Checklist

1. `src/provider/<id>/mod.rs`, with `pub const PROVIDER_ID`, the provider struct, and
   the trait impl. The module doc should map each primitive to the cloud API calls it makes.
2. A config section in `config.rs`, and an arm in `AppConfig::default_regions` if
   your region names differ from AWS's.
3. Register it in `provider::build` and `PROVIDER_IDS`.
4. Tests: pure tests for parsing and classification, plus `wiremock` tests for every
   HTTP primitive. Add a test-only constructor that takes a base URL so tests never
   read env or credential files. See `provider/primeintellect` and `provider/gcp`.
5. A page under `docs/clouds/`, and a row in the comparison table.
