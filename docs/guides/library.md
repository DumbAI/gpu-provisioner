# Embedding gpuprov

The `gpuprov` binary keeps everything under `~/.gpu-provisioner`: a SQLite state
file and a generated SSH key. A program that embeds the library (crate
`gpu_provisioner`) — for example a workflow worker deployed to hosts whose disks
are wiped on every deploy, or several workers sharing work — can keep gpuprov's
state in **Postgres** and supply **its own SSH key** instead. Nothing gpuprov
needs to resume an operation then lives on the worker's disk.

## The state store

Everything gpuprov remembers goes through one interface,
`gpu_provisioner::db::StateStore`: the `vms` projection (one row per VM), the
state-machine journal (`machines` + `machine_events`) and the per-VM lease. Two
backends implement it with identical semantics:

| Backend | Open with | Used by |
|---|---|---|
| `SqliteStore` | `SqliteStore::open(&paths.db_path())`, `SqliteStore::open_in_memory()` | the CLI, tests |
| `PgStore` | `PgStore::connect(url)`, `PgStore::connect_with(url, PgOptions { schema, max_connections })` | embedders |

Machines (`LaunchMachine`, `TeardownMachine`, `K3sMachine`) and the `Executor`
borrow a `&dyn StateStore`; own the store as an `Arc<dyn StateStore>` and lend
`&*store`.

### Postgres

- **URL:** `postgres://user:pass@host:port/db`. `sslmode` works as in libpq
  (default `prefer`); certificates are verified against the system's roots.
  Managed Postgres signed by a provider CA (DigitalOcean) needs that CA:
  add `sslrootcert=/path/to/ca.pem` to the URL and it is trusted in addition to
  the system roots.
- **Its own schema.** gpuprov's tables live in the `gpuprov` schema (created if
  absent; `PgOptions::schema` to change it). Every statement is
  schema-qualified, and gpuprov never touches anything outside its schema, so it
  can share a database with the embedder's own tables.
- **Migrations** are gpuprov's own and append-only, recorded in
  `gpuprov.migrations`, applied by `connect`, and serialized across processes
  with a transaction-scoped advisory lock: workers starting together are safe.
- **One pool per process.** `PgStore` is a connection pool (8 connections by
  default); create it once and share it.

### The lease

One run drives a VM at a time, across processes and hosts:

- Every `Executor` holds the VM's lease under its own `LeaseOwner`
  (`host:pid:random-token`), so two runs never share an identity — not even two
  attempts of the same operation in one worker process.
- The lease is taken in **one atomic statement**: granted when the VM is
  unleased, already this owner's, its operation is finished, or its heartbeat is
  older than `LEASE_STALE_SECS` (60 s); otherwise `StoreError::Busy` names the
  holder. Heartbeats and staleness use the **database's clock**, never a
  worker's.
- The executor heartbeats every 20 s while a step runs or polls.
- **Fencing.** Journal writes and heartbeats succeed only while the run still
  owns the lease. A run that stalled past the window and was taken over gets
  `StoreError::LeaseLost` at its next write or heartbeat and stops at once
  (`ExecError::is_lease_lost()`): it never writes again, and the new owner
  carries the operation on from the journal, observing the cloud before it acts.

Treat `Busy` and `LeaseLost` as retryable: retrying later either resumes the
journaled operation or finds it finished.

## Supplying the SSH key

`ssh::keys::ensure_keypair(keys_dir)` (the CLI's) generates a key on first use.
An embedder that owns the key — read from a secret — passes it instead:

```rust
let keypair = keys::materialize_keypair(&private_key_openssh, keys_dir)?;
```

The key must be an unencrypted OpenSSH private key (`-----BEGIN OPENSSH PRIVATE
KEY-----`). `ssh` needs a file, so gpuprov writes it to `keys_dir` (created
`0700`; the key `0600`, written atomically, untouched when unchanged) and returns
the same `KeyPair` as `ensure_keypair`.

VM records store the key's path (`ssh_key_path`). **Use the same `keys_dir` on
every host** (for example `/var/lib/<app>/gpuprov/keys`, materialized from the
secret at startup), so a record written on one host is usable on any other.

## Example: a stateless worker

```rust
use std::path::Path;
use std::sync::Arc;

use gpu_provisioner::cluster::k3s::{K3sMachine, K3sState};
use gpu_provisioner::db::{PgStore, StateStore};
use gpu_provisioner::machine::{Cancel, Executor};
use gpu_provisioner::ssh::{keys, wait};
use gpu_provisioner::vm::launch::{LaunchMachine, SshConnector};
use gpu_provisioner::{planner, provider};

// Once per process.
let store: Arc<dyn StateStore> = Arc::new(PgStore::connect(&database_url).await?);
let keypair = keys::materialize_keypair(&ssh_private_key, Path::new("/var/lib/worker/gpuprov/keys"))?;

// Per operation.
let cloud = provider::build(&cloud_id, &config).await?;
let plan = planner::prepare(&spec, &config.defaults, config.default_regions(&cloud_id), &keypair.public_key_openssh)?;
let vm_name = plan.spec.vm_name.clone();
let launch = LaunchMachine {
    provider: &*cloud,
    db: &*store,
    paths: &paths,
    connector: &SshConnector,
    spec: plan.spec,
    request: plan.request,
    required_features: plan.required_features,
    spec_json: serde_json::to_string(&spec)?,
    ssh_key_path: keypair.private_path.clone(),
    retry_until_up: false,
    ssh_timeout: wait::DEFAULT_TIMEOUT,
};
let machine = K3sMachine::new(launch, &paths);
let end = Executor::new(&machine, &*store, &vm_name, Cancel::new())
    .run(K3sState::start())
    .await?;
```

`paths` (`util::Paths`) still names local directories for per-VM ssh config
files, ControlMaster sockets and kubeconfigs. On a stateless worker these are
scratch: nothing in them is needed to resume an operation.

## Moving existing state

`gpuprov state migrate --to postgres://…` copies a SQLite state file into
Postgres (see the [CLI reference](cli.md)).
