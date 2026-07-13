# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A mini-SkyPilot in Rust: `gpuprov` provisions an AWS VM with region failover, waits for SSH, and records it locally. It is a deliberate clean-room port of SkyPilot's provisioning pipeline — module doc comments throughout cite the SkyPilot "Phase N" / source-file analogs they re-implement; keep those references accurate when changing behavior.

An optional `--k8s` flag turns the VM into a single-node **k3s** cluster (real Kubernetes) and writes a local kubeconfig, so apps deploy as containers via `kubectl`. That layer has no SkyPilot analog and lives entirely in `kube.rs` plus a `Workload` selector in `userdata.rs` — it adds nothing below the `CloudProvider` trait.

Binary: `gpuprov` (`src/main.rs`). Library: `gpu_provisioner` (`src/lib.rs`).

## Commands

```bash
cargo build                      # debug build; release: cargo build --release
cargo test                       # all unit tests — no cloud access or credentials needed
cargo test failover              # tests in one module (substring match)
cargo test capacity_fails_over_to_next_region_with_teardown   # single test
cargo clippy --all-targets       # lint
cargo fmt                        # format
cargo run -- provision --name dev --instance-type t3.micro --region us-west-2   # run CLI (needs AWS creds)
cargo run -- provision --k8s --gpus A10G:1 --name dev --region us-east-2        # + single-node k3s (writes a kubeconfig)
cargo run -- kubeconfig dev            # re-fetch a --k8s cluster's local kubeconfig
```

Manual testing: set `GPUPROV_HOME=/some/tmp/dir` to relocate all local state (DB, keys, ssh configs) away from `~/.gpu-provisioner`. `RUST_LOG=debug` for verbose progress (logs → stderr, final summary → stdout).

## Architecture

Ports-and-adapters, strictly layered top-down (each layer only calls the one below):

```
orchestrator.rs   launch(): Plan → Provision-with-failover → Wait-for-SSH → Persist (blocking stage machine)
  failover.rs     walks candidate regions; classify error → teardown partial resources → block region → next
    provider/mod.rs   CloudProvider trait (name, plan_candidates, bootstrap, run_instance, wait_running, describe, terminate)
      provider/aws/   the ONLY code allowed to touch AWS SDKs (mod=clients/creds, bootstrap, instance, ami, error)
```

Adding a cloud = implementing the `CloudProvider` trait; nothing above the trait may import a cloud SDK. Each adapter self-identifies via `name()` (canonical lowercase id, e.g. `aws`) — recorded on every VM row so `down`/reconcile dispatch to the owning adapter.

**Error taxonomy drives failover.** `ProvisionError` (provider/mod.rs) is the contract between adapter and engine: `Capacity`/`Other` → teardown + block the (instance_type, region) candidate + continue; `Config` → block + continue, no teardown; `InvalidRequest` (unplannable ask), `Credentials`, and `TeardownFailed` are fatal and abort the whole loop (failed teardown means possible resource leak — never continue past it). AWS error-code → variant mapping lives in `provider/aws/error.rs`; new error handling belongs there, classified into this taxonomy, not as ad-hoc retries elsewhere.

**State machine in SQLite** (`db.rs`, one row per VM): `PROVISIONING → UP → TERMINATED`, `FAILED` on exhausted failover. The failover engine leaves the winning row in `PROVISIONING`; only the orchestrator marks `UP`, after SSH is actually confirmed. Rows record the `provider` id and `instance_type`; schema changes are additive `ALTER TABLE` migrations in `init_schema` (duplicate-column errors swallowed).

**Idempotency is two-level:** the orchestrator reconciles an existing `UP` record against the cloud before provisioning, and the AWS adapter does find-or-create keyed on the identity tag `gpuprov-vm-name=<name>` (all cloud queries filter on it; everything created is also tagged `gpuprov-managed=true`).

**Supporting subsystems:**
- `planner.rs` + `spec.rs` — the cloud-AGNOSTIC half of planning: validate `VmSpec` (YAML + CLI overrides, flags win) and split it into a `ResourceRequest` (instance_type XOR parsed `GpuRequest`, regions, zone pin) plus the candidate-independent `LaunchSpec`. Candidate enumeration is cloud-specific and lives behind `CloudProvider::plan_candidates(request) -> Vec<Candidate{instance_type, region, zone}>`: one GPU ask may resolve to several instance types (differing CPU/memory), and region/zone naming conventions belong to each cloud. AWS: `provider/aws/catalog.rs` — GPU→type ladder (cheapest first), type-major × region enumeration, `zone = region + letter` pin convention. AMI flavor (GPU/CPU × x86_64/arm64) is decided at bootstrap from live instance-type metadata (`provider/aws/instance_type.rs`, DescribeInstanceTypes), which also restricts subnets to zones actually offering the type (DescribeInstanceTypeOfferings) and validates root-device name / minimum disk per AMI (DescribeImages).
- `ssh/` — one shared local ed25519 keypair (generated on first use, injected via cloud-init `userdata.rs`; no EC2 key-pair APIs), readiness probe (TCP banner then real login, 10 min cap), command runner wrapping system OpenSSH (`run_command` streams stdio; `run_command_capture` captures output — used by the k3s finalize), and per-VM config files `Include`d from `~/.ssh/config`. ControlMaster multiplexing is auto-skipped when the socket path would exceed the OS ~104-byte limit (e.g. a deep `GPUPROV_HOME`) — see `ssh/options.rs`.
- `userdata.rs` — cloud-init builder; renders a `Workload` (`Bare` = user + key only, or `K3s { install_script }`). Cloud-init *format* is provider-agnostic; delivery is AWS-specific (base64 into `RunInstances`, `provider/aws/instance.rs`).
- `kube.rs` — the optional Kubernetes layer (no SkyPilot analog), gated by `spec.kubernetes` / `--k8s`. `K3S_INSTALL_SCRIPT` is embedded into cloud-init by the planner via `userdata::Workload::K3s`; it installs k3s as a systemd service (k3s's control plane is the on-VM reconciler — nothing custom), and when `nvidia-smi` succeeds at boot adds `nvidia-container-toolkit` + the NVIDIA device plugin with `--default-runtime nvidia`. Client-side `setup()` waits for the k3s API over SSH, reads `/etc/rancher/k3s/k3s.yaml`, rewrites its `server` to the public IP (k3s is installed with `--tls-san <public-ip>`), and writes `<GPUPROV_HOME>/kube/<name>.yaml`. The `kubernetes` flag opens ports 6443/80/443 at SG creation (planner) and rides in `spec_json` — no schema change; `kubeconfig` re-runs `setup()`, and `down` removes the local kubeconfig.
- `util.rs` — `Paths`: all local state under `$GPUPROV_HOME` or `~/.gpu-provisioner` (adds `kube/` for kubeconfigs).

## Testing conventions

Tests are inline `#[cfg(test)]` modules (no `tests/` dir). The failover engine is tested against a scripted `FakeProvider` implementing `CloudProvider` with per-region behaviors and a call log — extend that pattern for new failover logic. Async tests use `#[tokio::test(start_paused = true)]` so the retry/backoff sleeps don't slow the suite. DB tests use `Db::open_in_memory()`.
