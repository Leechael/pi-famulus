//! OS process-control seam (design doc §3.4).
//!
//! Unix: session leaders, signals, and pipes (`unix` module).
//! Windows: Job Objects, named pipes, and `TerminateProcess` (`windows` module).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Portable civil time used by CLI formatting.
#[derive(Clone, Copy, Debug)]
pub struct LocalTime {
    /// Common-era year (e.g. 2026).
    pub year: i32,
    /// Month 1–12.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// FNV-1a 64-bit, shared with the extension so named-pipe identities match.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}
