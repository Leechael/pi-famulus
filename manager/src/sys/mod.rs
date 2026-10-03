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

/// Decode a `JOBOBJECT_BASIC_PROCESS_ID_LIST` buffer.
///
/// The two leading counts are `DWORD`s (4 bytes), not `usize`s. On x64 they
/// share the first 8 bytes and the ids start at offset 8. Reading `buf[0]`
/// as one `usize` count packs both fields: a 2-process job becomes
/// `assigned = (2 << 32) | 2` (`0x2_0000_0002`). The next
/// `vec![0usize; 2 + assigned]` is 8589934596 usizes, 68719476768 bytes,
/// and the allocation failure aborts the runner with
/// `STATUS_STACK_BUFFER_OVERRUN` (`0xC0000409`).
///
/// `Err(assigned)` means `in_list < assigned`: the caller must retry with at
/// least `assigned` id slots. Zero ids are dropped.
fn parse_basic_process_id_list(buf: &[u8]) -> Result<Vec<u32>, usize> {
    if buf.len() < 8 {
        return Err(0);
    }
    let assigned = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let in_list = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    if in_list < assigned {
        return Err(assigned);
    }
    let width = std::mem::size_of::<usize>();
    let n = in_list.min(buf.len().saturating_sub(8) / width);
    let mut pids = Vec::with_capacity(n);
    for i in 0..n {
        let off = 8 + i * width;
        let p = match width {
            8 => u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()) as u32,
            4 => u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()),
            _ => unreachable!("pointer width"),
        };
        if p != 0 {
            pids.push(p);
        }
    }
    Ok(pids)
}

/// A retained process-control object keyed by pid, but not *identified* by
/// pid. Windows recycles a pid as soon as the last handle to that process
/// closes, so a long-lived job must not be replaced or removed just because
/// a new process was given the same number.
///
/// `insert` refuses to drop an existing slot. `remove` drops a slot only
/// when `generation` is the one `insert` returned for it.
#[cfg(any(windows, test))]
#[derive(Debug)]
pub(crate) struct JobTable<T> {
    next: u64,
    slots: std::collections::HashMap<u32, JobSlot<T>>,
}

#[cfg(any(windows, test))]
#[derive(Debug)]
pub(crate) struct JobSlot<T> {
    pub generation: u64,
    pub value: T,
}

#[cfg(any(windows, test))]
impl<T> JobTable<T> {
    pub(crate) fn new() -> Self {
        Self { next: 1, slots: std::collections::HashMap::new() }
    }

    /// Returns the generation that [`remove`](Self::remove) must pass.
    /// `Err(value)` if `pid` still names a retained slot: replacing it would
    /// drop that slot.
    pub(crate) fn insert(&mut self, pid: u32, value: T) -> Result<u64, T> {
        if self.slots.contains_key(&pid) {
            return Err(value);
        }
        let generation = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.slots.insert(pid, JobSlot { generation, value });
        Ok(generation)
    }

    pub(crate) fn get(&self, pid: u32) -> Option<&T> {
        self.slots.get(&pid).map(|s| &s.value)
    }

    pub(crate) fn remove(&mut self, pid: u32, generation: u64) -> Option<T> {
        match self.slots.get(&pid) {
            Some(s) if s.generation == generation => self.slots.remove(&pid).map(|s| s.value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod job_table_tests {
    use super::JobTable;

    #[test]
    fn pid_reuse_does_not_drop_or_clean_up_the_wrong_generation() {
        let mut jobs = JobTable::new();
        let g1 = jobs.insert(7, "job-old").unwrap();
        // A recycled pid must not replace the retained job.
        assert_eq!(jobs.insert(7, "job-new").unwrap_err(), "job-new");
        assert_eq!(jobs.get(7).copied(), Some("job-old"));
        // Stale cleanup from the previous generation is a no-op.
        assert!(jobs.remove(7, g1.wrapping_add(9)).is_none());
        assert_eq!(jobs.get(7).copied(), Some("job-old"));
        assert_eq!(jobs.remove(7, g1).unwrap(), "job-old");
        let g2 = jobs.insert(7, "job-new").unwrap();
        assert_ne!(g1, g2);
        // The old watcher's drop runs after the pid was reused.
        assert!(jobs.remove(7, g1).is_none());
        assert_eq!(jobs.get(7).copied(), Some("job-new"));
        assert_eq!(jobs.remove(7, g2).unwrap(), "job-new");
    }
}

#[cfg(test)]
mod tests {
    use super::parse_basic_process_id_list;

    #[test]
    fn job_pid_list_counts_are_dwords_not_usizes() {
        // A 2-process list. Reading the header as one usize yields
        // assigned = (2 << 32) | 2, and vec![0usize; 2 + assigned] is
        // 68719476768 bytes on x64.
        let width = std::mem::size_of::<usize>();
        let mut buf = vec![0u8; 8 + 2 * width];
        buf[0..4].copy_from_slice(&2u32.to_le_bytes());
        buf[4..8].copy_from_slice(&2u32.to_le_bytes());
        if width == 8 {
            buf[8..16].copy_from_slice(&111u64.to_le_bytes());
            buf[16..24].copy_from_slice(&222u64.to_le_bytes());
        } else {
            buf[8..12].copy_from_slice(&111u32.to_le_bytes());
            buf[12..16].copy_from_slice(&222u32.to_le_bytes());
        }
        assert_eq!(parse_basic_process_id_list(&buf).unwrap(), vec![111, 222]);

        let mut small = vec![0u8; 8 + width];
        small[0..4].copy_from_slice(&4u32.to_le_bytes());
        small[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_basic_process_id_list(&small).unwrap_err(), 4);
    }
}
