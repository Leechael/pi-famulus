//! pi-famulus as a library: the daemon, client and `top` model, shared by
//! the `pi-famulus` binary and front ends such as `gui/`.

pub mod capacity;
pub mod cli;
pub mod out;

pub mod client;
pub mod clock;
pub mod daemon;
pub mod events;
pub mod fmt;
pub mod gc;
pub mod handover;
pub mod hosted;
pub mod inspect;
pub mod intercom;
pub mod ipc;
pub mod lifecycle;
pub mod monitor;
pub mod winpath;
pub mod pager;
pub mod proto;
pub mod registry;
pub mod rpc_child;
pub mod runner;
pub mod shellstart;
pub mod stats;
pub mod sys;
pub mod task;
pub mod top;
pub mod workkind;

#[cfg(test)]
pub mod fs_permissions;
#[cfg(test)]
pub mod hermetic;

/// Package version plus the commit it was built from ("0.1.0+066598ae00"):
/// every build of 0.1.0 would otherwise look the same in `status`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("PI_FAMULUS_GIT_SHA"));
