//! Run the Windows test-harness regression tests with libtest. The portable
//! platform/resource suites use custom mains and do not discover #[test]s.

// Daemons and task children spawned here are reaped by the harness
// (`Home::drop` kills them), not by `wait()` at each call site.
#![allow(clippy::zombie_processes)]
#![cfg(windows)]

mod common;
