//! Integration tests: every HTTP cloud adapter, driven by the real launch and
//! teardown state machines, against a **stateful stub of its vendor's API**.
//!
//! Each stub (`tests/stubs/<vendor>.rs`) simulates one vendor account — the
//! endpoints gpuprov calls, with response shapes taken from the vendor's own
//! API reference (linked per endpoint in the module docs) — and lets tests
//! inject the vendor's documented failure modes. The same four scenarios
//! (`tests/stubs/harness.rs`) run against every stub:
//!
//! - **lifecycle**: plan → create → boot → ready (correct SSH login) → down,
//!   and the simulated account holds no per-VM billable resources afterwards;
//! - **capacity failover**: the vendor's "no capacity" response on create
//!   makes gpuprov compensate and launch the next offer;
//! - **interrupt**: cancelling mid-boot tears the instance down;
//! - **reuse**: provisioning the same name again adopts the instance.
//!
//! The stubs need no network or credentials, so these run in every
//! `cargo test`. AWS is covered by the moto emulator instead
//! (`tests/aws_moto.rs`), since the EC2 Query/XML protocol is better served by
//! a maintained emulator than a hand-written stub.

mod stubs;
