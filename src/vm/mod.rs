//! VM lifecycle state machines on top of the shared executor (`machine`):
//! [`launch`] (plan → failover attempts → SSH-verified `Ready`),
//! [`teardown`] (delete → await gone → cleanup; also the launch machine's
//! compensation step), and the read-only [`health`] check.

pub mod health;
pub mod launch;
pub mod teardown;
