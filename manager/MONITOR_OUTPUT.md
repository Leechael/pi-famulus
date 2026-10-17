# Monitor output metadata (v1)

Monitor `output` events retain the existing `chunk` and `next_cursor` byte-cursor contract. The raw output file and `output(cursor)` reads are unchanged. Rust never suppresses output or applies a deduplication time window.

An optional `monitor_records` array adds logical records:

```ts
interface MonitorOutputRecord {
  version: 1;
  kind: "line" | "progress";
  observed_at: number; // original observation, Unix epoch milliseconds
  cursor_start: number; // inclusive raw byte offset
  cursor_end: number; // exclusive raw byte offset; includes LF for LF-terminated lines
  preview: string; // at most 500 Unicode scalar values
  truncated: boolean;
  fingerprint?: string;
  timestamp_fingerprint?: string;
}
```

- Missing array means metadata is unavailable: use the legacy raw path. An empty array means framing is active but no record is ready. Clients must flush pending legacy raw content before switching to metadata.
- `line` represents one LF-terminated logical line, or a nonempty trailing line at EOF. Its fingerprints cover **all original bytes excluding LF**, including any CR, before UTF-8 display replacement or preview truncation. EOF after LF adds no empty record.
- Exact identity is `blake3:v1:<64 lowercase hex digits>`. This is the default client mode.
- Optional timestamp-normalized identity is `blake3:iso-edge-v1:<64 lowercase hex digits>`. Both hashes are supplied for complete lines; the client chooses explicitly. Only a strict leading timestamp followed by one ASCII space and/or trailing timestamp preceded by one ASCII space is removed. Accepted timestamps are `YYYY-MM-DDTHH:MM:SS[.1..9 digits](Z|±HH:MM)`, with valid calendar/time fields. Other whitespace/content, internal timestamps, arbitrary numbers, paths, and IDs remain significant. Bracketed timestamps, lowercase `t`/`z`, and CRLF normalization are not supported.
- `progress` is an incremental fragment of an unfinished line, covering bytes since the previous progress fragment. It omits both fingerprints and is **never deduplication-eligible**. A 250 ms fanout timer emits pending progress without requiring newline/EOF. The final `line` still hashes the entire logical line and may repeat a bounded preview of previously emitted fragments.
- A truncated preview is the first 250 scalar values, an ellipsis, and the last 249. `truncated` is authoritative; source content may itself contain an ellipsis.
- Progress and EOF finalization can emit `chunk: ""` at an unchanged `next_cursor`. Track metadata record identity independently of raw cursor, including a final line after progress at the same cursor.

## Bounds and replay

Hashing, preview storage, and timestamp lookahead are bounded for arbitrarily long lines. The live daemon retains at most 1,024 metadata records and approximately 512 KiB of preview/record storage per monitor. Live/replay monitor raw slices are at most 4,096 bytes, bounding worst-case empty-line metadata expansion below the existing 4 MiB protocol frame limit.

Late watch/reconnect reuses original records and observation times when the relevant byte range remains in memory. Replay also sends progress/final records ending exactly at the watcher's already-written cursor: raw bytes may have arrived before progress/EOF metadata. Clients must ignore already-handled duplicate records, not reject all same-cursor records.

Metadata is intentionally not persisted or included in hot-upgrade snapshots. Older replay ranges, disk-loaded terminal tasks, and inherited partial lines explicitly omit metadata and retain raw fallback. After hot upgrade, framing resumes after the next LF; a raw transport slice crossing that unknown prefix falls back as a unit. This avoids ever assigning a false full-line fingerprint to a suffix.

Normal EOF drains fanout before `task_exited`. Existing bounded drain deadlines remain: escaped/lingering descendants can keep pipes open past task exit, so metadata/output may still arrive afterward in that pre-existing lifecycle case. Output queue overflow and the 8 MiB raw replay cap retain existing `output(cursor)` recovery semantics; that API deliberately remains raw, without metadata.
