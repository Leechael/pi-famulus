//! Bounded, byte-cursor-preserving monitor record framing.
//!
//! Hashes cover original bytes, excluding LF, never a lossy/truncated preview.
//! No content is suppressed here. Time-window policy belongs to the client.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const PREVIEW_BYTES: usize = 2004;
const HISTORY_BYTES: usize = 512 * 1024;
const HISTORY_RECORDS: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorOutputRecord {
    pub version: u8,
    pub kind: String,
    pub observed_at: u64,
    pub cursor_start: u64,
    pub cursor_end: u64,
    pub preview: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_fingerprint: Option<String>,
}

#[derive(Default)]
struct Preview {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    len: u64,
}

impl Preview {
    fn push(&mut self, bytes: &[u8]) {
        self.len += bytes.len() as u64;
        let take = bytes
            .len()
            .min(PREVIEW_BYTES.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..take]);
        if bytes.len() >= PREVIEW_BYTES {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - PREVIEW_BYTES..]);
        } else {
            let overflow = (self.tail.len() + bytes.len()).saturating_sub(PREVIEW_BYTES);
            self.tail.drain(..overflow);
            self.tail.extend(bytes);
        }
    }

    fn render(&self) -> (String, bool) {
        let head = String::from_utf8_lossy(&self.head);
        if self.len <= self.head.len() as u64 && head.chars().count() <= 500 {
            return (head.into_owned(), false);
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&tail);
        let end: String = tail
            .chars()
            .rev()
            .take(249)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        (
            format!("{}…{}", head.chars().take(250).collect::<String>(), end),
            true,
        )
    }
}

struct Line {
    start: u64,
    exact: blake3::Hasher,
    normalized: blake3::Hasher,
    leading: Vec<u8>,
    leading_done: bool,
    tail: Vec<u8>,
    preview: Preview,
    fragment: Preview,
    fragment_start: u64,
}

impl Line {
    fn new(start: u64) -> Self {
        Self {
            start,
            exact: blake3::Hasher::new(),
            normalized: blake3::Hasher::new(),
            leading: Vec::new(),
            leading_done: false,
            tail: Vec::new(),
            preview: Preview::default(),
            fragment: Preview::default(),
            fragment_start: start,
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        self.exact.update(bytes);
        self.preview.push(bytes);
        self.fragment.push(bytes);
        if !self.leading_done {
            let n = bytes.len().min(64 - self.leading.len());
            self.leading.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.leading.len() == 64 {
                self.flush_leading();
            }
        }
        if self.leading_done {
            self.push_normalized(bytes);
        }
    }

    fn flush_leading(&mut self) {
        let leading = std::mem::take(&mut self.leading);
        let skip = iso_len(&leading)
            .filter(|&n| leading.get(n) == Some(&b' '))
            .map(|n| n + 1)
            .unwrap_or(0);
        self.leading_done = true;
        self.push_normalized(&leading[skip..]);
    }

    fn push_normalized(&mut self, bytes: &[u8]) {
        // Retain only a bounded suffix until EOF/LF, so a trailing timestamp
        // can be excluded without retaining the (possibly enormous) line.
        if bytes.len() >= 64 {
            self.normalized.update(&self.tail);
            self.normalized.update(&bytes[..bytes.len() - 64]);
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - 64..]);
        } else {
            self.tail.extend_from_slice(bytes);
            let n = self.tail.len().saturating_sub(64);
            self.normalized.update(&self.tail[..n]);
            self.tail.drain(..n);
        }
    }

    fn finish(mut self, end: u64, observed_at: u64) -> MonitorOutputRecord {
        if !self.leading_done {
            self.flush_leading();
        }
        let keep = self
            .tail
            .iter()
            .rposition(|&b| b == b' ')
            .filter(|&i| iso_len(&self.tail[i + 1..]) == Some(self.tail.len() - i - 1))
            .unwrap_or(self.tail.len());
        self.normalized.update(&self.tail[..keep]);
        let (preview, truncated) = self.preview.render();
        MonitorOutputRecord {
            version: 1,
            kind: "line".into(),
            observed_at,
            cursor_start: self.start,
            cursor_end: end,
            preview,
            truncated,
            fingerprint: Some(format!("blake3:v1:{}", self.exact.finalize().to_hex())),
            timestamp_fingerprint: Some(format!(
                "blake3:iso-edge-v1:{}",
                self.normalized.finalize().to_hex()
            )),
        }
    }

    fn progress(&mut self, end: u64, observed_at: u64) -> Option<MonitorOutputRecord> {
        if self.fragment_start == end {
            return None;
        }
        let (preview, truncated) = self.fragment.render();
        let record = MonitorOutputRecord {
            version: 1,
            kind: "progress".into(),
            observed_at,
            cursor_start: self.fragment_start,
            cursor_end: end,
            preview,
            truncated,
            fingerprint: None,
            timestamp_fingerprint: None,
        };
        self.fragment = Preview::default();
        self.fragment_start = end;
        Some(record)
    }
}

/// Strict ISO8601 token only: YYYY-MM-DDTHH:MM:SS[.1..9 digits](Z|±HH:MM).
/// No regex over arbitrary numbers, identifiers, or paths; separators must be
/// spaces at the outer line edges. CR and other content remain significant.
fn iso_len(b: &[u8]) -> Option<usize> {
    fn number(b: &[u8]) -> Option<u32> {
        b.iter().try_fold(0, |n, c| {
            c.is_ascii_digit().then(|| n * 10 + (c - b'0') as u32)
        })
    }
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = number(&b[..4])?;
    let month = number(&b[5..7])?;
    let day = number(&b[8..10])?;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0
        || day > days
        || number(&b[11..13])? > 23
        || number(&b[14..16])? > 59
        || number(&b[17..19])? > 59
    {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start || i - start > 9 {
            return None;
        }
    }
    match b.get(i) {
        Some(b'Z') => Some(i + 1),
        Some(b'+' | b'-')
            if b.len() >= i + 6
                && b[i + 3] == b':'
                && number(&b[i + 1..i + 3])? <= 23
                && number(&b[i + 4..i + 6])? <= 59 =>
        {
            Some(i + 6)
        }
        _ => None,
    }
}

pub struct MonitorState {
    cursor: u64,
    line: Line,
    /// Nonzero inherited cursors may start in the middle of an unknown line.
    skipping: bool,
    coverage_start: u64,
    history: VecDeque<MonitorOutputRecord>,
    history_bytes: usize,
}

impl MonitorState {
    pub fn new(cursor: u64) -> Self {
        Self {
            cursor,
            line: Line::new(cursor),
            skipping: cursor != 0,
            coverage_start: cursor,
            history: VecDeque::new(),
            history_bytes: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8], observed_at: u64) -> Vec<MonitorOutputRecord> {
        let mut records = Vec::new();
        for segment in bytes.split_inclusive(|&b| b == b'\n') {
            self.cursor += segment.len() as u64;
            let newline = segment.last() == Some(&b'\n');
            if self.skipping {
                self.coverage_start = self.cursor;
                if newline {
                    self.skipping = false;
                    self.line = Line::new(self.cursor);
                }
                continue;
            }
            self.line.push(if newline {
                &segment[..segment.len() - 1]
            } else {
                segment
            });
            if newline {
                let line = std::mem::replace(&mut self.line, Line::new(self.cursor));
                records.push(line.finish(self.cursor, observed_at));
            }
        }
        self.remember(&records);
        records
    }

    pub fn finish(&mut self, observed_at: u64) -> Vec<MonitorOutputRecord> {
        if self.skipping || self.line.start == self.cursor {
            return Vec::new();
        }
        let line = std::mem::replace(&mut self.line, Line::new(self.cursor));
        let records = vec![line.finish(self.cursor, observed_at)];
        self.remember(&records);
        records
    }

    pub fn progress(&mut self, observed_at: u64) -> Vec<MonitorOutputRecord> {
        if self.skipping {
            return Vec::new();
        }
        let records: Vec<_> = self
            .line
            .progress(self.cursor, observed_at)
            .into_iter()
            .collect();
        self.remember(&records);
        records
    }

    fn remember(&mut self, records: &[MonitorOutputRecord]) {
        for record in records {
            self.history_bytes += record.preview.len() + 256;
            self.history.push_back(record.clone());
        }
        while self.history_bytes > HISTORY_BYTES || self.history.len() > HISTORY_RECORDS {
            let old = self.history.pop_front().unwrap();
            self.history_bytes -= old.preview.len() + 256;
            self.coverage_start = self.coverage_start.max(old.cursor_end);
        }
    }

    /// A replay outside bounded history, or inherited unknown line, explicitly
    /// lacks metadata. Never pretend an incomplete history is authoritative.
    pub fn replay(&self, from: u64, to: u64) -> Option<Vec<MonitorOutputRecord>> {
        if self.skipping || from < self.coverage_start || to > self.cursor {
            return None;
        }
        Some(
            self.history
                .iter()
                .filter(|r| r.cursor_end > from && r.cursor_end <= to)
                .cloned()
                .collect(),
        )
    }

    pub fn replay_at(&self, cursor: u64) -> Option<Vec<MonitorOutputRecord>> {
        if !self.available(cursor) {
            return None;
        }
        Some(
            self.history
                .iter()
                .filter(|r| r.cursor_end == cursor)
                .cloned()
                .collect(),
        )
    }

    pub fn available(&self, from: u64) -> bool {
        !self.skipping && from >= self.coverage_start
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_hash_is_not_a_preview_hash_and_long_lines_stay_bounded() {
        let prefix = "界".repeat(10_000);
        let line = |suffix: &str| {
            let mut state = MonitorState::new(0);
            for _ in 0..100 {
                assert!(state.push(prefix.as_bytes(), 7).is_empty());
            }
            state.push(suffix.as_bytes(), 7);
            assert!(state.line.preview.head.len() <= PREVIEW_BYTES);
            assert!(state.line.preview.tail.len() <= PREVIEW_BYTES);
            assert!(state.line.tail.len() <= 64);
            state.finish(7).pop().unwrap()
        };
        let a = line("A");
        let b = line("B");
        assert_ne!(a.fingerprint, b.fingerprint);
        assert!(a.truncated);
        assert_eq!(a.preview.chars().count(), 500);
        assert!(a.preview.ends_with('A'));
        assert!(a.preview.contains('…'));
        // A differing byte outside BOTH preview ends still changes identity.
        let hidden = |middle: &str| {
            let mut state = MonitorState::new(0);
            let text = format!("{}{middle}{}\n", "a".repeat(1000), "z".repeat(1000));
            state.push(text.as_bytes(), 1).pop().unwrap()
        };
        let (a, b) = (hidden("A"), hidden("B"));
        assert_eq!(a.preview, b.preview);
        assert_ne!(a.fingerprint, b.fingerprint);
        let mut state = MonitorState::new(0);
        let record = state.push(b"abc\n", 1).pop().unwrap();
        assert_eq!(
            record.fingerprint.as_deref(),
            Some("blake3:v1:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85")
        );
    }

    #[test]
    fn normalization_only_removes_explicit_valid_iso_timestamps_at_edges() {
        let record = |text: &str| {
            let mut state = MonitorState::new(0);
            state.push(text.as_bytes(), 1);
            state.finish(1).pop().unwrap()
        };
        let plain = record("build job=123 /tmp/run42 id=abc 45");
        for text in [
            "2026-10-08T12:34:56Z build job=123 /tmp/run42 id=abc 45",
            "build job=123 /tmp/run42 id=abc 45 2026-10-08T12:34:56.123456789+01:00",
            "2026-10-08T12:34:56Z build job=123 /tmp/run42 id=abc 45 2026-10-09T01:02:03Z",
        ] {
            let stamped = record(text);
            assert_ne!(stamped.fingerprint, plain.fingerprint);
            assert_eq!(
                stamped.timestamp_fingerprint, plain.timestamp_fingerprint,
                "{text}"
            );
        }
        for text in [
            "build job=124 /tmp/run42 id=abc 45",
            "build job=123 /tmp/run43 id=abc 45",
            "build job=123 /tmp/run42 id=abd 45",
            "build 2026-10-08T12:34:56Z job=123 /tmp/run42 id=abc 45",
            "2026-02-30T12:34:56Z build job=123 /tmp/run42 id=abc 45",
            "2026-10-08T12:34:56Z/path build job=123 /tmp/run42 id=abc 45",
        ] {
            assert_ne!(
                record(text).timestamp_fingerprint,
                plain.timestamp_fingerprint,
                "{text}"
            );
        }
        let input = format!(
            "2026-10-08T12:34:56Z {} 2026-10-09T01:02:03.12-04:00",
            "long".repeat(100)
        );
        let expected = record(&input);
        for split in 0..input.len() {
            let mut state = MonitorState::new(0);
            state.push(&input.as_bytes()[..split], 1);
            state.push(&input.as_bytes()[split..], 1);
            assert_eq!(state.finish(1).pop().unwrap(), expected);
        }
    }

    #[test]
    fn progress_is_incremental_unhashed_and_does_not_change_final_line_identity() {
        let mut state = MonitorState::new(0);
        state.push(b"first", 10);
        let first = state.progress(11).pop().unwrap();
        assert_eq!(first.kind, "progress");
        assert_eq!(first.preview, "first");
        assert_eq!((first.cursor_start, first.cursor_end), (0, 5));
        assert!(first.fingerprint.is_none() && first.timestamp_fingerprint.is_none());
        assert!(state.progress(12).is_empty());
        state.push(b" second", 13);
        let second = state.progress(14).pop().unwrap();
        assert_eq!(second.preview, " second");
        assert_eq!((second.cursor_start, second.cursor_end), (5, 12));
        let completed = state.push(b"\n", 15).pop().unwrap();
        let expected = MonitorState::new(0)
            .push(b"first second\n", 15)
            .pop()
            .unwrap();
        assert_eq!(completed, expected);
        assert!(state.finish(16).is_empty());
    }

    #[test]
    fn history_is_bounded_and_inherited_partial_lines_explicitly_fall_back() {
        let mut state = MonitorState::new(0);
        state.push(&vec![b'\n'; HISTORY_RECORDS + 1], 10);
        assert!(state.history.len() <= HISTORY_RECORDS);
        assert!(state.history_bytes <= HISTORY_BYTES);
        assert!(state.replay(0, 1).is_none());
        assert_eq!(state.replay(1, 2).unwrap().len(), 1);
        let mut inherited = MonitorState::new(20);
        assert!(!inherited.available(20));
        inherited.push(b"unknown tail", 10);
        assert!(inherited.progress(11).is_empty());
        assert!(inherited.finish(12).is_empty());
        inherited.push(b"\n", 13);
        assert!(!inherited.available(20));
        assert!(inherited.available(33));
        let record = inherited.push(b"new line\n", 14).pop().unwrap();
        assert_eq!(record.cursor_start, 33);
        assert_eq!(inherited.replay(33, 42).unwrap(), vec![record.clone()]);
        assert_eq!(inherited.replay_at(42).unwrap(), vec![record]);
    }

    #[test]
    fn worst_case_live_metadata_frame_fits_protocol_limit() {
        let mut state = MonitorState::new(0);
        let records = state.push(&vec![b'\n'; 4096], 1);
        assert_eq!(records.len(), 4096);
        let event = crate::proto::Event::new(crate::proto::EventKind::Output {
            task_id: "mon_12345678".into(),
            chunk: "\n".repeat(4096),
            next_cursor: 4096,
            monitor_records: Some(records),
        });
        assert!(serde_json::to_vec(&event).unwrap().len() < crate::proto::MAX_FRAME_SIZE as usize);
    }

    #[test]
    fn output_protocol_is_additive_and_distinguishes_empty_from_missing_metadata() {
        use crate::proto::EventKind;
        let legacy: EventKind = serde_json::from_value(serde_json::json!({
            "event":"output", "task_id":"mon_1", "chunk":"hello", "next_cursor":5
        }))
        .unwrap();
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("monitor_records")
            .is_none());
        let value = serde_json::to_value(EventKind::Output {
            task_id: "mon_1".into(),
            chunk: "hello".into(),
            next_cursor: 5,
            monitor_records: Some(Vec::new()),
        })
        .unwrap();
        assert_eq!(value["monitor_records"], serde_json::json!([]));
        assert_eq!(value["chunk"], "hello");
        assert_eq!(value["next_cursor"], 5);
    }

    #[test]
    fn logical_lines_are_stable_across_every_utf8_transport_split() {
        let input = "hello 世界\nsecond\ntrailing".as_bytes();
        let mut whole = MonitorState::new(0);
        let mut expected = whole.push(input, 123);
        expected.extend(whole.finish(123));
        assert_eq!(expected.len(), 3);
        assert_eq!(expected[0].preview, "hello 世界");
        assert_eq!(expected[0].cursor_end, 13);
        for split in 0..=input.len() {
            let mut state = MonitorState::new(0);
            let mut actual = state.push(&input[..split], 123);
            actual.extend(state.push(&input[split..], 123));
            actual.extend(state.finish(123));
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(&expected).unwrap(),
                "split {split}"
            );
        }
    }
}
