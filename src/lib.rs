//! gpuprov — provision GPU VMs across clouds (AWS, GCP,
//! Prime Intellect, and a local Docker test cloud) with failover, SSH into
//! them, and optionally turn them into single-node k3s clusters.
//!
//! Layering (ports-and-adapters, each layer calls only the one below):
//!
//! ```text
//! cluster::k3s ── K3sMachine: VM child machine + install → API → kubeconfig → validate
//! vm::launch   ── LaunchMachine: reconcile → plan → [prepare → create → boot → connect]* → ready
//! vm::teardown ── TeardownMachine: delete → await gone → cleanup (also launch compensation)
//!   machine    ── Executor: journal, retry budgets, polling, cancellation, resume, lease
//!     provider ── CloudProvider trait: plan / prepare / create / observe / delete / cleanup
//!       provider::{aws, gcp, primeintellect, docker} ── the only code that talks to a cloud
//! ```
//!
//! Supporting subsystems: `ssh` (keys, probe, runner, ssh-config), `db` (local
//! SQLite projection + machine journal), `planner`/`spec` (request
//! validation), `config` (CLI config file), `userdata` (cloud-init), `kube`
//! (k3s installer and kubeconfig handling). Design:
//! `doc/STATE_MACHINE_DESIGN.md`.

pub mod cluster;
pub mod config;
pub mod db;
pub mod kube;
pub mod machine;
pub mod planner;
pub mod provider;
pub mod spec;
pub mod ssh;
pub mod userdata;
pub mod util;
pub mod vm;
