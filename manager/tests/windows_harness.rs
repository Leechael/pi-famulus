//! Run the Windows test-harness regression tests with libtest. The portable
//! platform/resource suites use custom mains and do not discover #[test]s.
#![cfg(windows)]

mod common;
