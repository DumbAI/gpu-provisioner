# Testing

```bash
cargo test                       # all unit tests: no cloud or Docker needed
cargo test vm::launch            # one module (substring match)
cargo test quota_blocks_every_type_in_the_region   # one test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test --test docker_e2e -- --ignored --nocapture   # end-to-end, needs Docker
```

## Layers

| Layer | How it's tested |
|---|---|
| Executor (`machine/`) | a toy machine: journaling, retry-budget exhaustion, resume from a journaled state, cancel → compensation, cancel → resumable |
| Launch / teardown / k3s machines | `provider::fake::FakeProvider`, a small simulated cloud with scripted per-offer behaviors (`Boots`, `CreateFails`, `CreatedButErrors`, `BootFails`, `NeverBoots`, …), injectable delete failures, a call log, and a max-concurrent-instances counter for leak and duplicate checks |
| Adapters | pure parsing and classification tests, plus `wiremock` HTTP servers for Prime Intellect and GCP |
| End to end | `tests/docker_e2e.rs`: the Docker cloud plus real SSH. It provisions, runs a command, re-provisions (reuse), simulates a crash after create (adopts the container, no duplicate), and tears down. |

Async tests run on paused time (`#[tokio::test(start_paused = true)]`), so the
machines' 5-second polls and 30-second backoffs cost nothing. Database tests use
`Db::open_in_memory()`.

The launch-machine suite pins the behaviors that matter most:

- capacity triggers compensation and moves to the next region; quota skips the
  whole region; unavailable skips without teardown
- a transient create error after the request landed adopts the instance instead
  of creating a second one
- a fatal error after create tears down before aborting
- a failed teardown becomes `LEAKED`, and the next run cleans up first
- a crash while `Booting` resumes without a second create
- an unsupported feature is rejected even when a running VM could be reused, and
  the VM's record is left untouched

## CI

| Workflow | Runs |
|---|---|
| `.github/workflows/unit.yml` | fmt, clippy (`-D warnings`), unit tests on Linux and macOS, MSRV (1.91.1) check |
| `.github/workflows/integration.yml` | the Docker e2e suite on Linux, with `GPUPROV_E2E_REQUIRE_DOCKER=1` so a missing daemon fails the job instead of skipping |
| `.github/workflows/docs.yml` | strict MkDocs build on docs PRs; deploys to GitHub Pages on pushes to `main` |

## Manual testing against real clouds

Use `GPUPROV_HOME=/tmp/gp` to keep state away from `~/.gpu-provisioner`, and
`RUST_LOG=debug` for verbose progress. `gpuprov status <name>` shows the journaled
transitions with failure details.
