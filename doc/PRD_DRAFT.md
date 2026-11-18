# gpuprov — Product Requirements Document

**Working name:** `gpuprov` (see Open Questions §13 — the draft placeholder `[Project Name]` is resolved to the repo name for now)
**Status:** Draft v0.3
**Product stage:** Alpha definition
**Positioning:** Open-source, self-hostable, **local-first single-binary** GPU workspace tool
**Primary customer:** Cost-conscious independent AI developers and researchers
**Beachhead use case:** Interactive GPU development (SSH / VS Code / Cursor / Jupyter)
**Deployment model:** Run the binary yourself; nothing hosted in the loop for the MVP

> **Editor's note (what changed from v0.2, and why).** v0.2 was a strong positioning
> memo but tried to lead with two use cases (interactive dev *and* resilient jobs),
> listed five soft differentiators instead of one sharp wedge, and described a
> control-plane + node-agent architecture that contradicts an "it just runs"
> promise. v0.3 makes four decisions: **(1)** lead with interactive development;
> **(2)** the wedge is **radical simplicity** — a single static binary, no Ray, no
> API server, no Kubernetes to operate; **(3)** the first customer is the
> cost-chasing indie developer/researcher; **(4)** keep the ambitious vision but
> gate it behind a build-vs-reuse spike (Milestone 0). Recovery, jobs, and any
> always-on control plane move to Phase 2, where they are actually required.

---

## 1. Executive Summary

We're building **gpuprov**, an open-source, single-binary tool that lets a
cost-conscious AI developer **open their repository on the cheapest compatible
GPU across many providers — in one command, with no control plane, cluster, or
agent to operate** — so they stop losing hours to provider-specific setup and
stop overpaying for idle or single-vendor GPUs. The wedge is operational
simplicity: where SkyPilot and dstack are powerful but heavy, gpuprov is a
local-first client you run yourself. Success looks like an indie developer going
from `gpuprov up` in a repo to a working GPU shell in under five minutes, on
whichever provider is cheapest that day, with their environment reproduced
identically each time.

---

## 2. Value Proposition

### One-line positioning

> **Open your repo on the cheapest available GPU, from one command — no cluster, no control plane, no lock-in.**

### Expanded positioning

`gpuprov` is a self-hostable, local-first CLI for GPU development environments.
It searches supported clouds and GPU marketplaces according to explicit cost,
region, hardware, and trust policies; launches a reproducible environment on the
best available offer; explains why that offer was chosen; and connects it to
SSH, VS Code, Cursor, or Jupyter. There is no service to stand up and no agent
fleet to babysit — you run one binary, and your state lives in a local file and
your own object storage.

The user keeps ownership of their cloud/marketplace accounts, source code and
datasets, persistent storage, environment specification, placement policies, and
workload history.

### The wedge: radical simplicity

This is the spine of the product, and the one thing competitors structurally do
**not** offer:

| | gpuprov | SkyPilot | dstack | Brev / DGX Lepton |
|---|---|---|---|---|
| Install | single static binary | Python env + deps | Python env + server | account signup |
| To run it | nothing — it's a client | optional API server; Ray under the hood | runs a server process | hosted SaaS |
| State | local file + your storage | local + optional server | server DB | vendor's cloud |
| Self-hostable | trivially (nothing to host) | yes (heavier) | yes | control plane not public |

"Open source + multi-cloud + cheap GPUs" is **table stakes** — SkyPilot, dstack,
and Shadeform all have it. gpuprov's differentiation is not that list; it is that
it is the **simplest** self-hostable way to get a cheap GPU workspace: one binary,
no daemon, SSH-native, local state. The current codebase already *is* this — the
product bet is that "it just runs" is worth switching for.

> **Honest open question (a real risk, tracked in §11):** is "simpler" a strong
> enough reason for a developer to switch off SkyPilot/dstack? The MVP exists to
> test exactly that.

### What we deliberately do NOT promise

- **Not** "seamless recovery of any training job." An orchestrator can recreate
  infrastructure and restore files; it cannot reconstruct a valid
  model/optimizer/dataloader checkpoint from process memory. (Recovery is Phase 2
  and gets an explicit, honest contract — see §12.)
- **Not** the lowest possible hourly price as the only signal — we surface an
  explainable *effective* choice (price + reliability + startup time + trust).
- **Not** a managed cluster, an MLOps suite, or multi-node distributed training.

---

## 3. Problem Statement

The cost-conscious AI developer can usually *find* a cheap GPU. Using it
**reliably and repeatably**, across more than one provider, is where the hours go.

### Provider-specific setup friction
Every provider exposes different instance names, pricing, APIs, storage, auth,
and lifecycle semantics. Moving a workload means re-learning a provider, not just
picking a machine.

### Environment is recreated over and over
Driver, CUDA, Python, Docker, repo checkout, secrets, and editor setup get
rebuilt by hand every time the developer changes machines or providers — the
"works on the box I destroyed yesterday" problem.

### Single-vendor pricing and idle burn
Committing to one cloud means paying its price and eating idle-GPU spend when a
developer forgets to tear a box down. The cheapest GPU this week is rarely on the
same provider as last week.

### Existing tools are powerful but heavy
SkyPilot and dstack solve much of the above but ask the developer to adopt a
Python toolchain and/or run a server. For a solo developer who just wants a GPU
shell, that is more machinery than the job needs.

### Evidence (⚠️ gap — see Open Questions §13)
This is currently a **hypothesis grounded in the author's own experience** as the
target persona, not yet validated with external users. Phase 0 (§12) exists to
close this gap with 15–20 developer interviews. A credible PRD must not present
the problem as proven before then.

---

## 4. Target Customers & Personas

### Primary persona: "Indie Ivy" — independent AI developer / researcher

A developer working alone or in a team of ~2–10 who:

- Uses PyTorch, JAX, Hugging Face, Axolotl, vLLM, or similar.
- Uses Docker but has **no interest in operating a cluster or a control plane**.
- Works through SSH, VS Code, Cursor, or Jupyter.
- Runs interactive experiments and single-node fine-tunes.
- **Is price-sensitive and already willing to use more than one provider**
  (clouds *and* marketplaces like Runpod / Vast).
- Needs control over images, packages, ports, storage, and startup commands.
- Tolerates variable infrastructure quality **when it is explicitly disclosed**.
- Adopts open-source CLIs readily; distrusts SaaS control planes in the data path.

This persona feels the multi-provider pain most acutely and is the fastest
adopter of a single-binary OSS tool. It is also, plausibly, the project author —
a genuine dogfooding advantage.

### Secondary persona: small-team infrastructure owner (Phase 3, not now)

An engineering lead who wants to connect several provider accounts, set team
budgets and placement policies, standardize environments, and preserve the option
to self-host. Explicitly a **later** customer — the MVP is not built for them.

### Explicitly NOT the customer (for the MVP)

- Frontier-model pretraining across hundreds of nodes.
- Organizations requiring certified/regulated environments (SOC 2, HIPAA, sovereign).
- Mission-critical inference with contractual availability guarantees.
- Teams seeking a full experiment tracker / model registry / MLOps suite.
- Users who require fully-managed hardware and application support.

---

## 5. Use Cases (ranked)

The MVP **leads with one** use case and treats the rest as supporting or deferred.
This ranking is the antidote to v0.2's diluted dual-lead.

1. **[BEACHHEAD] Interactive development.**
   *"When I need to work on an AI repo, I want to request a GPU by capability and
   budget and get a ready environment — without researching provider-specific
   instances."* → `gpuprov up` in a repo → cheapest compatible GPU → SSH/editor/Jupyter.

2. **[Supporting] Cost-aware, explainable placement.**
   *"When several providers have compatible hardware, I want to see the choices and
   tradeoffs so I can pick on price, reliability, location, and startup time."*

3. **[Supporting] Reproducibility / portability.**
   *"When I recreate an environment, I want the same image, repo revision, setup
   steps, secrets, and workspace — so provider changes don't cause drift."*

4. **[Phase 2] Interruption recovery for jobs.**
   *"When my spot instance disappears, I want replacement capacity, my files
   restored, and my resume command invoked."* — Deferred; it's where SkyPilot is
   strongest and where an always-on component becomes necessary.

5. **[Phase 3] Team budgets & policies.** Connect accounts, shared policies, cost
   history, idle-spend guardrails.

6. **[Ongoing] Community extensibility.** A documented adapter interface so a user
   or provider can add support without touching the scheduler core.

---

## 6. Solution Overview (local-first)

`gpuprov` is a client binary. It talks to provider APIs directly, keeps state in a
local file (as today's `gpuprov` does with SQLite), and uses SSH + object-storage
sync for the workspace. **There is no long-running control plane and no node agent
in the MVP** — the reconciliation that a control plane would do is unnecessary for
launch-and-connect; it is only introduced in Phase 2 for recovery.

The user declares: required GPU capability, max hourly price, acceptable
regions/providers, acceptable trust level, container/environment config,
persistent workspace location. gpuprov then: (1) searches compatible capacity,
(2) ranks offers by the user's policy, (3) explains the placement decision,
(4) provisions the selected instance, (5) configures the environment, (6) opens
SSH / editor / Jupyter, (7) syncs the declared workspace to user-owned storage.

### Primary user flow — launching a workspace

1. `gpuprov up` from a repository.
2. gpuprov reads `gpu.yaml`.
3. Retrieves compatible offers across configured providers.
4. Shows the recommended offer **and** the alternatives it considered.
5. Provisions the selected instance (with fallback across offers on failure).
6. Pulls the declared image + repo revision; mounts/restores the workspace.
7. Runs startup commands.
8. Opens SSH, the selected editor, or Jupyter.

### First-time setup

Install the binary → create a project → connect one or more provider accounts
(credentials validated, least-privilege) → configure user-owned object storage for
the workspace → launch a sample environment.

### Recovering a workload

Deferred to Phase 2 — documented there, including the always-on component it requires.

---

## 7. Product Principles

- **Local-first / no daemon until necessary.** A long-running process must *earn*
  its place. The MVP has none; Phase 2 adds one only for recovery. *(New in v0.3 —
  this is the principle that makes the simplicity wedge real.)*
- **User-owned infrastructure.** Bring-your-own-cloud / bring-your-own-account.
  gpuprov is not the merchant of record in the MVP.
- **User-owned state.** Persistent data lives in storage the user controls.
  Deleting gpuprov must not delete the user's source, checkpoints, or workspace.
- **Explainable placement.** Show why an offer was selected, what alternatives
  existed, and which constraints eliminated others.
- **No hidden markup.** If a managed tier ever charges, provider rate / storage
  rate / estimated transfer / platform fee are shown as separate line items.
- **Portability before maximum abstraction.** The workload spec exposes relevant
  infrastructure differences rather than pretending all GPUs behave identically.
- **Community extensibility.** Provider adapters, the workload spec, and the CLI
  are public and documented.
- **Explicit recovery semantics.** *(Applies in Phase 2.)* Distinguish recreating
  infrastructure, restoring files, and resuming application progress.

---

## 8. Scope

### Milestone 0 — Build-vs-reuse spike (the gate)

Before building a provider controller from scratch, run a time-boxed engineering
spike comparing three paths, and decide with evidence:

1. **Extend gpuprov's existing Rust core** (add a second cloud + an offer catalog
   behind the existing `CloudProvider` trait).
2. **Build on SkyPilot** (wrap its provisioning; inherit its recovery later).
3. **Build on dstack** (closest existing scope).

Decision rule: *only* build a new orchestration engine when a critical product
requirement — chiefly the **simplicity wedge** (single binary, no daemon) — cannot
be delivered through an existing one. This directly honors v0.2's own §12 risk
and prevents accidentally re-implementing SkyPilot.

### MVP — local-first interactive workspace (Phase 1)

Included:

- Interactive development workspaces (the beachhead).
- 1–8 GPUs on a single provider instance where available; NVIDIA + CUDA first.
- Custom OCI images (digest-pinned), Git checkout, startup commands.
- SSH access; VS Code / Cursor connectivity; Jupyter port forwarding.
- User-owned S3-compatible workspace storage (initial restore + periodic sync).
- Idle shutdown; max hourly price and budget limits.
- Provider fallback **during provisioning** (not runtime recovery).
- Normalized offer catalog + policy scheduler (`cheapest`, `balanced`,
  `fixed-provider`).
- **Local state** (no server); CLI-first with a minimal read-only status view.
- Self-hosted by definition (you run the binary).

Provider strategy: prove the concept across two different infrastructure
*categories* first (e.g. one hyperscaler + one marketplace). The exact two are an
output of the Milestone 0 spike, chosen on API quality, credits, startup latency,
and effort. AWS is a natural anchor since the current code already supports it.

### Explicitly out of the MVP

- **Runtime recovery / resilient jobs** → Phase 2 (needs an always-on component).
- **Always-on control plane / node agent** → introduced only when recovery requires it.
- Managed service / team features → Phase 3.
- Multi-node distributed training; Slurm/Kubernetes cluster management.
- Unified provider billing; buying/reselling GPU capacity.
- Automatic checkpointing of arbitrary programs.
- AMD / Intel / TPU (schema is vendor-neutral from day 1; NVIDIA is the first
  *tested* implementation).
- SOC 2 / HIPAA / GDPR / sovereign-cloud certification.
- Automatic dataset migration across arbitrary storage services.

---

## 9. Success Metrics

Scoped to the **interactive-dev beachhead** (recovery metrics move to Phase 2).

### North-star metric
**Weekly successful workspace launches** — a launch that reaches the declared
ready state and provides usable access, terminating with a known final state.

### Beta targets
- ≥ 60% of users who configure a provider launch their first workspace in the same session.
- Median time from `up` to usable shell **< 5 min**; P90 **< 10 min** (excluding provider capacity shortages).
- ≥ 95% provisioning success after the configured fallback chain.
- **Zero duplicate instances** from client retries (idempotency — already a gpuprov strength).
- ≥ 25% of activated users use **two or more providers** within 30 days *(validates the core multi-provider thesis)*.
- ≥ 30% of active projects enable an idle or budget guardrail.
- Zero known plaintext credential exposures.

Cost savings are **measured, not guaranteed**: compare the selected offer against
the user's first-choice / fixed-provider baseline and report the observed delta.

---

## 10. Competitive Landscape

Disciplined, corrected view (carried forward from v0.2's competitive corrections):

- **SkyPilot** — Apache-2.0, multi-cloud, managed-job recovery. Powerful but
  heavier: Python toolchain, Ray under the hood, API server for teams. *Our edge:
  simplicity and a client-only footprint; not "open + multi-cloud," which it has.*
- **dstack** — open source, dev environments + multi-cloud, heterogeneous
  hardware. **Closest competitor to the MVP.** *Our edge: single binary, no server.*
- **Brev (NVIDIA)** — CLI is public/MIT; multi-provider NVIDIA instances with
  SSH/Jupyter/Docker/editors. *No public, self-hostable control plane found — that,
  not "closed source," is the accurate distinction.*
- **DGX Cloud Lepton (NVIDIA)** — managed platform already claiming cost/region/
  performance placement, sovereignty, health monitoring, recovery, dev pods.
  Closed/managed. *So "multi-cloud, sovereignty, resilience, dev access" are not
  differentiators by themselves.*
- **Shadeform** — aggregates price/availability across 15+ providers. *Confirms
  "cheap multi-cloud aggregation" is table stakes.*

**Net:** the defensible position is **operational simplicity for the indie
developer**, plus honest scope — not the feature list everyone already ships.

---

## 11. Risks & Mitigations

- **"Simpler" may not be enough to switch.** *(The wedge's central risk.)*
  Mitigation: the MVP is a cheap experiment; measure switching + activation before
  investing in Phase 2. If simplicity doesn't move users, revisit the wedge.
- **Parity-not-value vs. dstack/SkyPilot.** Mitigation: Milestone 0 spike;
  build new only if simplicity can't be delivered on an existing engine.
- **Local-first limits recovery.** Recovery needs something always watching, which
  the MVP deliberately lacks. Mitigation: Phase 2 explicitly introduces an optional
  always-on component — an architecture change scoped and flagged now, not later.
- **Recovery expectations (Phase 2).** Users may read "automatic recovery" as zero
  progress loss. Mitigation: three explicit recovery levels; show checkpoint age;
  require a tested resume contract; never say "seamless."
- **Community-GPU security.** Root on a community host doesn't protect the workload
  from the host operator. Mitigation: trust tiers, strong warnings, allowlists,
  default exclusion of community infra for sensitive projects.
- **Data-transfer costs** can erase compute savings. Mitigation: show storage
  locality; estimate transfer where possible; prefer providers near persistent state.
- **Provider API instability** (long-tail providers). Mitigation: conformance
  tests, capability declarations, adapter versioning, health scoring.
- **OSS monetization** (a larger platform hosts the project). Mitigation:
  differentiate on managed operations, reliability data, and community trust — not
  code exclusivity.

---

## 12. Rollout

**Phase 0 — Validation.** Interview 15–20 target developers; document their most
recent GPU provisioning experience; quantify setup time, provider count,
interruption frequency, lost work; test willingness to use user-owned object
storage and a declarative manifest; secure 3–5 design partners. *Closes the §3
evidence gap.*

**Phase 1 — Local-first interactive workspace (the MVP).** Two provider adapters,
offer normalization, CLI, custom container, SSH + VS Code/Cursor, workspace sync,
idle shutdown. **No server.**
Milestone: *"A developer opens the same repo + workspace on Provider A and
Provider B without manually rebuilding it."*

**Phase 2 — Resilient single-node jobs (introduces the always-on component).** Job
mode, spot policy, heartbeat + interruption detection, provider fallback at
runtime, checkpoint/resume contract, recovery timeline, cost history. This is where
a long-running process first earns its place.
Milestone: *"A controlled instance termination causes the job to resume on another
compatible provider from the latest persisted checkpoint."*

**Phase 3 — Managed service & community ecosystem.** Optional hosted control plane,
team accounts, public adapter SDK, template registry, reliability scoring, budget
policies, a third provider category.

**Phase 4 — Heterogeneous & local compute.** AMD/ROCm, user-registered machines,
on-prem SSH fleets, regional providers, optional K8s/Slurm integration, multi-node
(only after single-node reliability is proven).

### Business model (deferred — OSS-first)

The MVP is OSS with **no monetization**. A managed control plane and team/enterprise
features (SSO, RBAC, audit export, org budgets, in-VPC deployment) are a **Phase 3**
question, guided by "no hidden markup." Documented here only so the vision is
coherent; not in scope now. *(v0.2 led with a full business model; v0.3 defers it.)*

---

## 13. Open Questions

- **Name.** Keep `gpuprov`, or pick a product name for the messaging? (Placeholder resolved to `gpuprov` for now.)
- **Evidence.** Do we have any external validation yet, or is the author the sole
  data point? Phase 0 is the plan — is there appetite to run it before building?
- **Milestone 0 outcome.** Extend the existing Rust core, build on SkyPilot, or
  build on dstack? Everything downstream depends on this.
- **Which two providers** for the MVP? (Output of the spike; AWS is the anchor.)
- **When does an always-on component become mandatory** — strictly at Phase 2
  recovery, or earlier for something we haven't scoped?
- **How much of today's AWS code is reused** vs. rewritten under the offer-catalog model?
- **Monetization scope** — confirmed deferred to Phase 3, or does a stakeholder need it in this PRD?

---

## Appendix A — Public messaging

**Headline:** *Your GPU workspace — one command, any cloud, nothing to run.*

**Subheadline:** Launch a compatible GPU in minutes and work through SSH, VS Code,
Cursor, or Jupyter. One binary, no cluster, no control plane, no lock-in.

**Proof points:**
- **Radically simple:** a single static binary — no Ray, no API server, no
  Kubernetes to operate.
- **Open & self-hostable:** you own the binary, your provider accounts, your
  workload definitions, and your storage.
- **Cost-aware & explainable:** set price, region, hardware, and trust policies and
  see why each placement was chosen.
- **Portable by design:** recreate the same environment and workspace across
  supported providers.

*(Phase 2 adds: "Recovery with honest guarantees — automatic infrastructure
restoration plus explicit checkpoint-and-resume support." Not claimed until it ships.)*

---

## Appendix B — Example workload manifest (aspirational)

> The MVP may start with a simpler subset; the `recovery:` block is **Phase 2**.
> Shown here to illustrate the portability contract, not to commit the MVP surface.

```yaml
apiVersion: gpu.dev/v1alpha1
kind: Workspace
metadata:
  name: qlora-experiment
spec:
  image: ghcr.io/example/qlora@sha256:0123456789abcdef
  source:
    repository: git@github.com:example/model-project.git
    revision: main
  resources:
    gpu:
      vendor: nvidia
      count: 1
      minVramGiB: 24
      preferredModels: [RTX_4090, RTX_5090, L40S, A100_40GB]
    cpu: 8
    memoryGiB: 32
    diskGiB: 200
  placement:
    providers: { allow: [runpod, vast, aws] }
    regions:   { allow: [us-west, us-central] }
    markets:   [spot, on-demand]
    maxHourlyUsd: 1.50
    minimumTrustTier: verified-datacenter
    strategy: balanced
  workspace:
    path: /workspace
    storageUri: s3://my-gpu-workspaces/qlora-experiment
    syncIntervalSeconds: 60
  recovery:                       # Phase 2 — not in the MVP
    enabled: true
    maxAttempts: 5
    checkpointPath: /workspace/checkpoints
    preemptionCommand: python scripts/save_checkpoint.py
    resumeCommand: python train.py --resume latest
  access:
    ssh: true
    vscode: true
    jupyter: { enabled: true, port: 8888 }
  lifecycle:
    idleStopAfter: 30m
    maximumRuntime: 24h
```

---

## Appendix C — Detailed functional requirements (carried from v0.2, reframed local-first)

> These remain valid targets. Two reframings from v0.2: **(a)** "the platform"
> means the client binary in the MVP, not a hosted service; **(b)** anything
> requiring an always-on process (§recovery controller, control-plane restart
> idempotency) is Phase 2. The rest applies to the MVP.

- **Declarative workload spec** — versioned, portable manifest (see Appendix B).
- **Normalized offer catalog** — provider, location, GPU model/arch/VRAM, CPU,
  memory, storage, network, market type, hourly price, billing granularity,
  estimated startup, availability timestamp, trust tier, known limitations,
  reliability score when data exists. *Unknown values shown as unknown, never
  silently estimated.*
- **Policy scheduler** — apply hard constraints, drop stale offers, rank, return
  recommendation + alternatives, explain every score/exclusion, fall back on
  provisioning failure, never exceed the user's provider/trust/region/price limits.
  Strategies: `cheapest`, `fastest-ready`, `most-reliable`, `balanced`,
  `fixed-provider`.
- **Provider lifecycle** — per-adapter conformance: validate creds, list offers,
  create/status/access/stop/delete instance, inject startup config, surface the
  original provider error code, report interruption/usage when supported.
  **Provisioning must be idempotent** (a client retry must not duplicate instances).
- **Environment creation & access** — custom OCI images, digest pinning, repo
  checkout, startup scripts, env/secret references, SSH key injection, secure port
  forwarding, Jupyter, VS Code/Cursor remote. The UI discloses the execution model
  (dedicated VM / shared-host container / community host / bare metal); "root
  access" is not presented as equivalent across them.
- **Workspace persistence** — protect a declared directory via user-owned storage:
  initial restore, periodic sync, sync on graceful stop, last-sync display,
  unhealthy-sync warning, cache/temp exclusions, integrity verification. A
  distributed POSIX filesystem is **not** required for the MVP.
- **Recovery controller** — **Phase 2.** Three levels (infrastructure / workspace /
  application); never recover process memory or post-sync files; never say
  "seamless" when progress was lost.
- **Cost controls** — pre-launch estimate; provider price vs. platform fee shown
  separately; max hourly price; max runtime; project budget; idle shutdown; cost
  alerts; final usage when provider data permits; warn on unknown storage/transfer
  cost; confirm before a recovery that materially raises price (Phase 2).
- **Security & trust** — encrypt provider creds at rest; least-privilege IAM; avoid
  long-lived cloud creds on workload nodes; redact secrets from logs; audit
  lifecycle/access events; trust-tier restrictions; warn that community GPUs are
  unsuitable for sensitive data unless the user accepts the risk. User datasets do
  not pass through any gpuprov service.
- **Open-source & community** — public source for CLI, scheduler, adapters (and, in
  Phase 2+, agent/server); stable provider-plugin SDK; conformance tests; public
  RFC process, roadmap, issue tracker; documented + reproducible builds; signed
  community templates; exportable workload/state metadata; opt-in telemetry only;
  no requirement to use any hosted service.
