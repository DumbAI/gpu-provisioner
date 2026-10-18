# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`gpuprov` (Rust) provisions a GPU VM on AWS, GCP or Prime Intellect (plus a local Docker "test cloud") with failover across offers/regions/zones, makes it a validated single-node **k3s** cluster by default (`--no-k8s` / `[defaults] kubernetes = false` opt out), waits for SSH, and records it locally. The project stands on its own: don't describe it, or its modules, in terms of other tools (no "port of"/"analog of" references in code, docs, or metadata) — explain behavior and rationale directly.

Binary: `gpuprov` (`src/main.rs`). Library: `gpu_provisioner` (`src/lib.rs`). Design doc (the "why"): `docs/architecture/state-machines.md`. `doc/PRD_DRAFT.md` is the product draft.

## Commands

```bash
cargo build                      # debug build; release: cargo build --release
cargo test                       # all unit tests — no cloud credentials or Docker needed
cargo test vm::launch            # tests in one module (substring match)
cargo test quota_blocks_every_type_in_the_region   # single test
cargo test --test docker_e2e -- --ignored --nocapture   # end-to-end on local Docker (real SSH)
cargo clippy --all-targets -- -D warnings          # CI runs with -D warnings
cargo fmt
cargo run -- provision --cloud docker --name demo --instance-type container   # full lifecycle, no account (k3s auto-skipped: no systemd)
cargo run -- status demo                           # journaled state transitions
uv run --with-requirements docs/requirements.txt mkdocs build --strict   # docs site (docs/, mkdocs.yml)
cargo publish --dry-run          # packaging check (Cargo.toml `include` limits shipped files)
```

Manual testing: `GPUPROV_HOME=/tmp/dir` relocates all local state; `provision` also adds an `Include` line to the real `~/.ssh/config` (override `HOME` too to avoid that). `RUST_LOG=debug` for verbose progress (logs → stderr, summaries → stdout).

## Architecture

Strictly layered; each layer calls only the one below:

```
main.rs          CLI: builds the provider from spec.cloud, runs a machine under the Executor, maps Ctrl-C → Cancel
cluster/k3s.rs   K3sMachine: Vm(child LaunchState) → CheckingBootInstall → [Installing (SSH)] → AwaitingApi → FetchingKubeconfig → Validating → Ready|Failed
vm/launch.rs     LaunchMachine: Reconciling → Planning → [Preparing → Creating → Booting → Connecting]* → Ready|Exhausted|Aborted|Leaked
vm/teardown.rs   Teardown sub-machine (Requesting → AwaitingGone → CleaningUp) — used by `down` AND launch compensation
machine/         Executor<M: Machine>: write-ahead journal, retry budgets/backoff, Poll, cancel-at-step-boundary, resume, lease
provider/mod.rs  CloudProvider trait: capabilities, plan, prepare, create, observe, delete, cleanup
  provider/{aws,gcp,primeintellect,docker}/  — the ONLY code that talks to a cloud API
```

**The split that matters:** adapters expose small idempotent primitives and classify errors — they never loop, sleep or retry (except a bounded wait on a cloud operation whose *result* carries the error, e.g. GCP `operations.wait`). All waiting, retries, failover, compensation and recovery live once in the core. Don't add retry loops to adapters; return `ErrorKind::Transient`.

**Machine rules** (`machine/mod.rs`): a machine is a serde enum + `step()`. Steps must be idempotent and *observe before acting* (e.g. `Creating` adopts an existing instance instead of creating a second). Every transition is journaled (`machines` + `machine_events` tables) before the next side effect, so handles (instance ids) are durable before any wait. Deadlines use `StepCx::in_state`; `Retry` consumes a budget and the machine names the failure state in `on_retries_exhausted`; `on_cancel` maps to compensation. Composition = embed the child's state (`K3sState::Vm(LaunchState)`) and delegate `step`.

**Error taxonomy drives failover** (`ProviderError { kind, msg, block }`): `Transient` → retry step; `Capacity`/`Failed` → compensate (teardown) + block offer + next; `Quota` → compensate + block whole region; `Unavailable` → block, no teardown (contract: nothing was created); `InvalidRequest`/`Credentials` → compensate if needed, then abort. Teardown that can't finish → persisted `LEAKED` (never move on silently); the next `provision`/`down` cleans it first. Per-cloud code → kind mapping lives in each adapter's `error.rs`.

**Two tables, two roles** (`db.rs`): `vms` is the user-facing projection (`PROVISIONING/UP/FAILED/TERMINATED/LEAKED`, endpoint incl. `ssh_port`); `machines`/`machine_events` is the source of truth for in-flight operations. Steps update `vms` idempotently. Distinguish `abort()` (marks the row FAILED) from `reject()` (request refused — leave a possibly-healthy VM's row alone). Schema changes are additive `ALTER TABLE` in `init_schema`.

**Identity & reuse:** every cloud resource is labeled with the VM name (`gpuprov-vm-name` tag/label, or name `gpuprov-<vm>`); `observe` filters on it. `Reconciling` reuses a running VM from an `UP` row, cleans `LEAKED`/stale rows, and refuses a VM owned by another provider. Unsupported features (`Capabilities::unsupported`) are rejected before reuse or planning.

**Provider specifics:** `Offer.detail` carries provider-private plan data (persisted). Provider-dictated SSH user/port come back in `Observed.endpoint` (Prime Intellect: `root`, NAT port). Region names are per-cloud (`AppConfig::default_regions(provider)`); AWS/GCP require regions, Prime Intellect treats none as "anywhere". GCP offers are per zone; PI plans from the live availability API; Docker uses region `local`.

**Supporting subsystems:**
- `planner.rs` + `spec.rs` — validate `VmSpec` (YAML + CLI, flags win; `cloud` selects the provider) into `Plan { spec: InstanceSpec, request: ResourceRequest, required_features }`. Workload concerns are NOT planned here.
- `kube.rs` — the k3s installer and single-shot SSH primitives: `probe_boot` (+ pure `BootProbe::decide`), `install`, `api_ready`, `fetch_kubeconfig`, `health` (one round trip, incl. cert-SAN check), `reachable`. The installer has two routes: **boot** (appended to cloud-init user data by `K3sMachine::new` when `Capabilities::cloud_init`) and **SSH** (fallback chosen by `CheckingBootInstall` when the boot run is missing/failed/timed out or missed a GPU). It is `flock`-serialized, writes a status marker (`/var/lib/gpuprov/k3s-install.status`), owns `/etc/rancher/k3s/config.yaml` (rendered from a SAN list; restart only on change), and finds the public IP via AWS/GCP metadata at boot; `Validating` re-runs it once with the SAN if the cert lacks it. `embedded_scripts_parse_as_bash` runs `bash -n` on every embedded script. Access mode: `Direct` when the provider supports `OpenPorts`, else `SshTunnel` (`gpuprov tunnel`, local 16443).
- **k3s default:** `VmSpec.kubernetes: Option<bool>` (CLI `--k8s`/`--no-k8s` > spec > `[defaults] kubernetes`, default true). An explicit ask adds `Feature::Kubernetes` (hard requirement); the implicit default is dropped via `Plan::kubernetes_for` on providers that don't support it (Docker), with a note. `main` stores the resolved choice in `spec_json`.
- `ssh/` — shared ed25519 keypair, single `probe` (TCP banner + login; the polling lives in `Connecting`), runner (`run_command`, `run_command_capture`, `exec_interactive`, `exec_forward`), per-VM ssh config (with `Port`). ControlMaster is skipped when the socket path would exceed ~104 bytes.
- `userdata.rs` — cloud-init (login user + key) for clouds that boot with it.
- `config.rs` — `[aws] [gcp] [primeintellect] [docker] [defaults]`, `deny_unknown_fields`; credentials resolve config → env → the cloud's own CLI files.

**Scope / non-goals** (don't build without being asked): one VM per name, terminate-only teardown (no stop/resume), no spot-preemption recovery, no storage mounts, single-node k3s only. `examples/*.yaml` are k8s manifests for `--k8s` clusters, not VM specs.

## Testing conventions

Unit tests are inline `#[cfg(test)]` modules (larger suites in a sibling `tests.rs`, e.g. `vm/launch/tests.rs`). Machine logic is tested against `provider::fake::FakeProvider` — a simulated cloud with scripted per-offer `Behavior`s, injectable delete failures, a call log and a max-live-instances counter; extend it rather than writing ad-hoc fakes. HTTP adapters use `wiremock` with test-only constructors taking a base URL (never read env/credential files in tests). Async machine tests use `#[tokio::test(start_paused = true)]`; DB tests use `Db::open_in_memory()`. `tests/docker_e2e.rs` is the only integration test: `#[ignore]`d, skips without Docker unless `GPUPROV_E2E_REQUIRE_DOCKER=1` (set in CI). CI: `.github/workflows/unit.yml` (fmt, clippy, tests on Linux+macOS, MSRV 1.91.1), `integration.yml` (Docker e2e), and `docs.yml` (strict MkDocs build; deploys https://dumbai.github.io/gpu-provisioner/ from `gh-pages` on pushes to `main`).
