//! Mini-SkyPilot: provision a cloud VM (AWS) with failover and SSH into it.
//!
//! Layering (ports-and-adapters):
//!
//! ```text
//! orchestrator ── the long-running launch() stage machine   (SkyPilot "Phase 3")
//!     └── failover ── candidate loop, blocklist, teardown   (SkyPilot "Phase 4")
//!             └── provider ── CloudProvider trait           (SkyPilot "Phase 5" API)
//!                     └── provider::aws ── the only adapter that touches the AWS SDK
//! ```
//!
//! Supporting subsystems: `ssh` (keys, wait, runner, ssh-config), `db` (local
//! SQLite state), `planner`/`catalog`/`spec` (mini-optimizer), `config` (CLI
//! config file), `userdata` (cloud-init), `kube` (optional single-node k3s
//! layer installed on top of a provisioned VM).

pub mod config;
pub mod db;
pub mod failover;
pub mod kube;
pub mod orchestrator;
pub mod planner;
pub mod provider;
pub mod spec;
pub mod ssh;
pub mod userdata;
pub mod util;
