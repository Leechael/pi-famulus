/**
 * Monitor output across an in-place manager upgrade (PR #134, e2e `(e3)`).
 *
 * Frame sequence RECONSTRUCTED, not captured: the CI artifact keeps the wakes
 * (faux-trace.jsonl, mon_bd7047ed: cursor ranges 0..41, 41..91, 86..91,
 * 91..141 and the duplicated `m-20`) but not the transport frames. The frames
 * replayed here are what `manager/src/daemon.rs` + `manager/src/monitor.rs`
 * produce for those ranges:
 *
 *   1. old image, live           chunk 0..41   records 0..41
 *   2. new image, first burst    chunk 41..91  records absent — the restored
 *      entry is rebuilt at the inherited cursor with `skipping = true`
 *      ("New binaries fall back to raw bytes until an inherited partial line
 *      ends", registry.rs), so `available()` is false for that chunk.
 *   3. reconnect watch           chunk ""  next_cursor 91  records 86..91 —
 *      `queue_output_replay` first re-sends records at the cursor the client
 *      was written up to ("consumers track record identity separately from
 *      their raw-byte cursor", daemon.rs).
 *   4. next burst                chunk 91..141 records 91..141
 *
 * The consumer must treat bytes it already received raw as delivered, or step
 * 3 repeats the last line of step 2. It must not treat a trailing fragment as
 * delivered: the manager frames that fragment with its own finish/EOF line
 * record at the same cursor, and under `wake_on` it has not been admitted yet.
 */
import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import type { ManagerClient, MonitorOutputRecord } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";

const TASK = "mon_bd7047ed";
const WINDOW_MS = 200;

interface Sent {
  content: string;
  details?: unknown;
}

function lineRecord(preview: string, start: number, end = start + Buffer.byteLength(preview) + 1): MonitorOutputRecord {
  return {
    version: 1,
    kind: "line",
    observed_at: 0,
    cursor_start: start,
    cursor_end: end,
    preview,
    truncated: false,
    fingerprint: `blake3:v1:${preview}`,
    timestamp_fingerprint: `blake3:iso-edge-v1:${preview}`,
  };
}

function progressRecord(preview: string, start: number, end: number): MonitorOutputRecord {
  return { version: 1, kind: "progress", observed_at: 0, cursor_start: start, cursor_end: end, preview, truncated: false };
}

/** Lines `from..to` as one chunk with their Rust-framed records. */
function burst(from: number, to: number, offset: number) {
  const records: MonitorOutputRecord[] = [];
  let cursor = offset;
  for (let i = from; i <= to; i++) {
    records.push(lineRecord(`m-${i}`, cursor));
    cursor = records[records.length - 1].cursor_end;
  }
  const text = Array.from({ length: to - from + 1 }, (_, k) => `m-${from + k}\n`).join("");
  return { text, end: cursor, records };
}

async function setup(params: { wake_on?: string; dedup_window_ms?: number } = {}) {
  const clock = new ManualClock(1_000);
  const sent: Sent[] = [];
  const center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: (m) => sent.push(m as Sent) });
  const client = {
    ensureAvailable: async () => true,
    isAvailable: () => true,
    start: async () => ({ task_id: TASK, pid: 1 }),
    watch: async () => {},
    stop: async () => {},
  } as unknown as ManagerClient;
  const registry = new MonitorRegistry({
    clock,
    getClient: () => client,
    sessionEnv: () => ({}),
    getNotifyCenter: () => center,
    trackTask: () => {},
  });
  await registry.start({ command: "./watch.sh", description: "upgrade watcher", persistent: true, ...params }, { cwd: "/tmp" } as never);

  const flush = async () => {
    clock.advanceBy(WINDOW_MS);
    await Promise.resolve();
  };
  const wakes = () =>
    sent
      .filter((m) => (m.details as { kind?: string } | undefined)?.kind === "monitor")
      .map((m) => ({
        text: (m.content.match(/<event>\n?([\s\S]*?)\n?<\/event>/)?.[1] ?? ""),
        shape: (m.content.match(/cursor-start="(\d+)" cursor-end="(\d+)"/) ?? []).slice(1, 3).join(".."),
      }));
  const lines = () => wakes().flatMap((w) => w.text.split("\n")).filter(Boolean);
  const shapes = () => wakes().map((w) => w.shape);
  const dispose = () => {
    registry.disposeAll();
    center.dispose();
  };
  return { clock, registry, sent, flush, wakes, lines, shapes, dispose };
}

describe("monitor output across an in-place upgrade", () => {
  it("delivers each line once when a replay re-sends records for a burst that arrived unframed", async () => {
    const t = await setup();
    try {
      const first = burst(1, 10, 0);
      t.registry.handleOutput(TASK, first.text, first.end, first.records);
      await t.flush();

      // Upgrade landed here: the restored manager cannot frame this burst.
      const second = burst(11, 20, first.end);
      t.registry.handleOutput(TASK, second.text, second.end, undefined);
      await t.flush();

      // Reconnect watch replays records at the cursor already written (91).
      t.registry.handleOutput(TASK, "", second.end, [lineRecord("m-20", 86)]);
      await t.flush();

      const third = burst(21, 30, second.end);
      t.registry.handleOutput(TASK, third.text, third.end, third.records);
      await t.flush();

      expect(t.shapes()).toEqual(["0..41", "41..91", "91..141"]);
      expect(t.lines()).toEqual(Array.from({ length: 30 }, (_, i) => `m-${i + 1}`));
    } finally {
      t.dispose();
    }
  });

  it("keeps identical text at a new byte range (dedup_window_ms 0)", async () => {
    const t = await setup({ dedup_window_ms: 0 });
    try {
      t.registry.handleOutput(TASK, "dup\n", 4, undefined);
      await t.flush();
      t.registry.handleOutput(TASK, "dup\n", 8, [lineRecord("dup", 4)]);
      await t.flush();
      expect(t.shapes()).toEqual(["0..4", "4..8"]);
      expect(t.lines()).toEqual(["dup", "dup"]);
    } finally {
      t.dispose();
    }
  });

  it("keeps a line record that completes a progress preview at the same cursor", async () => {
    const t = await setup();
    try {
      t.registry.handleOutput(TASK, "", 5, [progressRecord("match", 0, 5)]);
      await t.flush();
      t.registry.handleOutput(TASK, "", 5, [lineRecord("match", 0, 5)]);
      await t.flush();
      expect(t.shapes()).toEqual(["0..5", "0..5"]);
      expect(t.lines()).toEqual(["match", "match"]);
    } finally {
      t.dispose();
    }
  });

  it("does not swallow the finish record of a raw trailing fragment (wake_on)", async () => {
    const t = await setup({ wake_on: "match" });
    try {
      // No newline: wake_on holds the fragment, so nothing was delivered yet.
      t.registry.handleOutput(TASK, "match", 5, undefined);
      await t.flush();
      expect(t.lines()).toEqual([]);
      // The manager's finish() frames the same bytes as a line record.
      t.registry.handleOutput(TASK, "", 5, [lineRecord("match", 0, 5)]);
      await t.flush();
      expect(t.shapes()).toEqual(["0..5"]);
      expect(t.lines()).toEqual(["match"]);
    } finally {
      t.dispose();
    }
  });

  it("drops a replay of complete raw lines while still filtering with wake_on", async () => {
    const t = await setup({ wake_on: "match" });
    try {
      t.registry.handleOutput(TASK, "noise\nmatch\n", 12, undefined);
      await t.flush();
      // Replay of the matching complete line already delivered raw.
      t.registry.handleOutput(TASK, "", 12, [lineRecord("match", 6)]);
      await t.flush();
      // One wake for the chunk's original byte envelope; nothing added by the
      // replay of the complete matching line.
      expect(t.shapes()).toEqual(["0..12"]);
      expect(t.lines()).toEqual(["match"]);
    } finally {
      t.dispose();
    }
  });

  it("moves the completed boundary only when the newline lands", async () => {
    const t = await setup({ wake_on: "match" });
    try {
      t.registry.handleOutput(TASK, "match", 5, undefined);
      await t.flush();
      // Mid-line: wake_on holds the fragment, nothing is shown yet.
      expect(t.lines()).toEqual([]);
      t.registry.handleOutput(TASK, "\n", 6, undefined);
      await t.flush();
      expect(t.lines()).toEqual(["match"]);
      // The line is complete now: its replay covers bytes already delivered.
      t.registry.handleOutput(TASK, "", 6, [lineRecord("match", 0, 6)]);
      await t.flush();
      expect(t.lines()).toEqual(["match"]);
    } finally {
      t.dispose();
    }
  });
});
