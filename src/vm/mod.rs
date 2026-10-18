//! VM lifecycle state machines on top of the shared executor (`machine`):
//! [`launch`] (plan → failover attempts → SSH-verified `Ready`) and
//! [`teardown`] (delete → await gone → cleanup), the latter doubling as the
//! launch machine's compensation step.

pub mod launch;
pub mod teardown;
