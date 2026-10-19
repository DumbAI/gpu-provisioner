//! Stateful stubs of each vendor's API plus the shared scenarios they run.
//! See `tests/vendor_stubs.rs` for the overview.

pub mod harness;
// A shared toolkit: not every stub uses every helper.
#[allow(dead_code)]
pub mod route;

mod gcp;
mod hyperstack;
mod lambda;
mod massedcompute;
mod primeintellect;
mod runpod;
mod verda;
