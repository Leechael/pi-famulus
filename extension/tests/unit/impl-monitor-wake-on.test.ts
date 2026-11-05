import { describe, expect, it } from "vitest";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { ManualClock } from "../../src/clock";
import type { ManagerClient, MonitorOutputRecord } from "../../src/manager-client";
import { createMonitorTool, MonitorRegistry } from "../../src/monitor";
import type { OverloadControllerOptions } from "../../src/monitor-batching";
import { NotifyCenter } from "../../src/notify";
import type { FamulusWake } from "../../src/wake";

// Agent-written watch scripts print a status line every poll
// (`HEARTBEAT billing ready=0 unacked=0 consumers=1`), and each one woke the
// model. wake_on keeps every line in the task log but admits only matching
// lines to wakes; stall_after_ms replaces heartbeats as liveness.

const TERMINAL_MS = 10_000;

type Sent = { details?: unknown; content?: string; opts?: { triggerTurn?: boolean } };

interface SetupOptions {
  params?: { wake_on?: string; stall_after_ms?: number; dedup_window_ms?: number; timeout_ms?: number; persistent?: boolean };
  overload?: Partial<OverloadControllerOptions>;
  /** Output the manager dispatches before start() learns the task id. */
  early?: (registry: MonitorRegistry) => void;
}

async function setup(opts: SetupOptions = {}) {
  const clock = new ManualClock(1_000);
  const sent: Sent[] = [];
  const events: { type: string; fields?: Record<string, unknown> }[] = [];
  const stopped: string[] = [];
  let registry!: MonitorRegistry;
  const manager = {
    ensureAvailable: async () => true,
    isAvailable: () => true,
    start: async () => {
      opts.early?.(registry);
      return { task_id: "mon_1", pid: 1 };
    },
    watch: async () => {},
    stop: async (id: string, reason?: string) => { stopped.push(`${id}:${reason}`); },
  } as unknown as ManagerClient;
  const center = new NotifyCenter({ sendMessage: (m, o) => sent.push({ ...m, opts: o }), isIdle: () => true, clock });
  registry = new MonitorRegistry({
    getClient: () => manager,
    sessionEnv: () => ({}),
    getNotifyCenter: () => center,
    trackTask: () => {},
    clock,
    logEvent: (type, fields) => events.push({ type, fields }),
    ...(opts.overload ? { overload: opts.overload } : {}),
  });
  await registry.start(
    { command: "./watch.sh", description: "billing", persistent: true, ...opts.params },
    { cwd: "/tmp" } as ExtensionContext,
  );
  const monitorWakes = () =>
    sent.map((m) => m.details as FamulusWake | undefined).filter(
      (d): d is Extract<FamulusWake, { kind: "monitor" }> => d?.kind === "monitor",
    );
  const terminals = () =>
    sent.filter((m) => (m.details as FamulusWake | undefined)?.kind === "monitor-terminal");
  const done = () => {
    center.dispose();
    registry.disposeAll();
  };
  return { clock, registry, center, sent, events, stopped, monitorWakes, terminals, done };
}

/** Rust-framed line records with a running byte cursor. */
function framer() {
  let cursor = 0;
  return (text: string, at = 0): { chunk: string; cursor: number; record: MonitorOutputRecord } => {
    const chunk = `${text}\n`;
    const start = cursor;
    cursor += Buffer.byteLength(chunk);
    return {
      chunk,
      cursor,
      record: {
        version: 1, kind: "line", observed_at: at, cursor_start: start, cursor_end: cursor,
        preview: text, truncated: false,
        fingerprint: `blake3:v1:${text}`, timestamp_fingerprint: `blake3:iso-edge-v1:${text}`,
      },
    };
  };
}

const HEARTBEAT = "HEARTBEAT billing ready=0 unacked=0 consumers=1";

describe("monitor wake_on: which lines wake", () => {
  it("framed records: heartbeats never wake, a matching line does, and liveness still sees every line", async () => {
    const { clock, registry, monitorWakes, done } = await setup({ params: { wake_on: "ERROR|unacked=[1-9]" } });
    const line = framer();
    for (let i = 0; i < 20; i++) {
      const f = line(HEARTBEAT, i);
      registry.handleOutput("mon_1", f.chunk, f.cursor, [f.record]);
      clock.advanceBy(5_000);
    }
    expect(monitorWakes()).toEqual([]);
    expect(registry.listActive()[0].lastOutputAt).toBe(1_000 + 19 * 5_000);
    expect(registry.listActive()[0].lastEventAt).toBeUndefined();
    const hit = line("HEARTBEAT billing ready=0 unacked=3 consumers=1", 99);
    registry.handleOutput("mon_1", hit.chunk, hit.cursor, [hit.record]);
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual(["HEARTBEAT billing ready=0 unacked=3 consumers=1"]);
    // The wake's byte reference points at the matching line, not the heartbeats.
    expect(monitorWakes()[0].output).toMatchObject({ cursorStart: hit.record.cursor_start, cursorEnd: hit.record.cursor_end });
    done();
  });

  it("raw (legacy) chunks: only matching complete lines wake, split across chunks too", async () => {
    const { clock, registry, monitorWakes, done } = await setup({ params: { wake_on: "^ERROR" } });
    registry.handleOutput("mon_1", `${HEARTBEAT}\n${HEARTBEAT}\nERR`);
    clock.advanceBy(200);
    expect(monitorWakes()).toEqual([]);
    registry.handleOutput("mon_1", `OR disk full\n${HEARTBEAT}\n`);
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual(["ERROR disk full"]);
    done();
  });

  it("raw (legacy) chunks: a matching unterminated last line is still reported at exit", async () => {
    const { clock, registry, center, sent, done } = await setup({ params: { wake_on: "^ERROR" } });
    registry.handleOutput("mon_1", `${HEARTBEAT}\nERROR crashed`);
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: 1, duration_ms: 10 });
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    const all = JSON.stringify(sent.map((m) => m.details));
    expect(all).toContain("ERROR crashed");
    expect(all).not.toContain("HEARTBEAT");
    done();
  });

  it("without wake_on every line still wakes (today's behaviour)", async () => {
    const { clock, registry, monitorWakes, done } = await setup();
    registry.handleOutput("mon_1", `${HEARTBEAT}\n`);
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual([HEARTBEAT]);
    done();
  });
});

describe("monitor wake_on: dedup, overload and terminal bookkeeping", () => {
  it("repeated heartbeats are not counted as suppressed repeats and do not suppress a later matching line", async () => {
    const { clock, registry, monitorWakes, done } = await setup({ params: { wake_on: "ERROR" } });
    const line = framer();
    for (let i = 0; i < 50; i++) {
      const f = line(HEARTBEAT, i);
      registry.handleOutput("mon_1", f.chunk, f.cursor, [f.record]);
    }
    clock.advanceBy(200);
    const first = line("ERROR queue stuck", 60);
    registry.handleOutput("mon_1", first.chunk, first.cursor, [first.record]);
    clock.advanceBy(200);
    expect(monitorWakes()).toHaveLength(1);
    expect(monitorWakes()[0].event).toBe("ERROR queue stuck");
    expect(monitorWakes()[0].output?.suppressedCount ?? 0).toBe(0);
    // Same matching line again inside the window: suppressed and counted once;
    // the heartbeats in between are not counted.
    const again = line("ERROR queue stuck", 61);
    registry.handleOutput("mon_1", again.chunk, again.cursor, [again.record]);
    for (let i = 0; i < 10; i++) {
      const f = line(HEARTBEAT, 62 + i);
      registry.handleOutput("mon_1", f.chunk, f.cursor, [f.record]);
    }
    const other = line("ERROR consumer gone", 80);
    registry.handleOutput("mon_1", other.chunk, other.cursor, [other.record]);
    clock.advanceBy(200);
    expect(monitorWakes()).toHaveLength(2);
    expect(monitorWakes()[1].event).toBe("ERROR consumer gone");
    expect(monitorWakes()[1].output?.suppressedCount).toBe(1);
    done();
  });

  it("a heartbeat flood does not enter summary mode, drop, or stop, and a later match arrives as a line", async () => {
    const { clock, registry, monitorWakes, events, stopped, done } = await setup({
      params: { wake_on: "ERROR", dedup_window_ms: 0 },
      overload: { windowMs: 1_000, summaryLinesPerSec: 2, stopLinesPerSec: 20, stopSustainMs: 2_000 },
    });
    for (let s = 0; s < 10; s++) {
      registry.handleOutput("mon_1", `${`${HEARTBEAT} seq=${s}\n`.repeat(50)}`);
      clock.advanceBy(1_000);
      await Promise.resolve();
    }
    expect(monitorWakes()).toEqual([]);
    expect(events.filter((e) => e.type === "monitor.drop" || e.type === "monitor.stop")).toEqual([]);
    expect(stopped).toEqual([]);
    expect(registry.has("mon_1")).toBe(true);
    registry.handleOutput("mon_1", "ERROR consumer gone\n");
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual(["ERROR consumer gone"]);
    done();
  });

  it("matching lines still drive overload summaries, and the summary counts only them", async () => {
    const { clock, registry, monitorWakes, done } = await setup({
      params: { wake_on: "ERROR", dedup_window_ms: 0 },
      overload: { windowMs: 1_000, summaryLinesPerSec: 2, cadenceMs: [2_000, 2_000, 2_000, 2_000], exitQuietMs: 3_600_000 },
    });
    for (let s = 0; s < 4; s++) {
      registry.handleOutput("mon_1", `${"ERROR boom\n".repeat(5)}${`${HEARTBEAT}\n`.repeat(100)}`);
      clock.advanceBy(1_000);
      await Promise.resolve();
    }
    const enter = monitorWakes().filter((w) => w.event.includes("entering summary"));
    expect(enter).toHaveLength(1);
    const summary = monitorWakes().find((w) => w.event.includes("[Monitor overload summary]"));
    expect(summary).toBeDefined();
    expect(summary!.event).not.toContain("HEARTBEAT");
    expect(summary!.event).toMatch(/lines=(5|10|15)\b/);
    done();
  });

  it("a clean exit right after a heartbeat is not mistaken for an exit caused by an event", async () => {
    const { clock, registry, center, sent, done } = await setup({ params: { wake_on: "ERROR" } });
    registry.handleOutput("mon_1", `${HEARTBEAT}\n`);
    clock.advanceBy(300);
    registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: 0, duration_ms: 10 });
    center.settled();
    clock.advanceBy(TERMINAL_MS);
    const turns = sent.filter((m) => m.opts?.triggerTurn !== false);
    expect(turns).toHaveLength(1);
    expect((turns[0].details as FamulusWake).kind).toBe("monitor-terminal");
    expect(JSON.stringify(turns[0].details)).not.toContain("HEARTBEAT");
    expect(JSON.stringify(turns[0].details)).not.toMatch(/suppressedCount/);
    done();
  });

  it("output replayed before registration honours the filter", async () => {
    const { clock, monitorWakes, done } = await setup({
      params: { wake_on: "READY" },
      early: (registry) => registry.handleOutput("mon_1", `${HEARTBEAT}\nREADY port=8080\n`),
    });
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual(["READY port=8080"]);
    done();
  });

  it("output queued during a reconnect replay honours the filter", async () => {
    const { clock, registry, monitorWakes, done } = await setup({ params: { wake_on: "READY" } });
    const client = (registry as unknown as { deps: { getClient: () => ManagerClient } }).deps.getClient();
    (client as unknown as { watch: () => Promise<void> }).watch = async () => {
      registry.handleOutput("mon_1", `${HEARTBEAT}\n`, Buffer.byteLength(`${HEARTBEAT}\n`));
      registry.handleOutput("mon_1", "READY again\n", Buffer.byteLength(`${HEARTBEAT}\nREADY again\n`));
    };
    await registry.rewatchAll();
    clock.advanceBy(200);
    expect(monitorWakes().map((w) => w.event)).toEqual(["READY again"]);
    done();
  });
});

describe("monitor stall_after_ms", () => {
  it("wakes once when the source goes silent, and re-arms only after the next output line", async () => {
    const { clock, registry, monitorWakes, done } = await setup({ params: { wake_on: "ERROR", stall_after_ms: 30_000 } });
    // Heartbeats every 10s keep it alive without waking anyone.
    for (let i = 0; i < 6; i++) {
      registry.handleOutput("mon_1", `${HEARTBEAT}\n`);
      clock.advanceBy(10_000);
    }
    expect(monitorWakes()).toEqual([]);
    clock.advanceBy(20_000); // 30s since the last line
    expect(monitorWakes()).toHaveLength(1);
    expect(monitorWakes()[0].event).toMatch(/No output for 30s/);
    expect(monitorWakes()[0].event).toMatch(/still running/);
    clock.advanceBy(300_000);
    expect(monitorWakes()).toHaveLength(1);
    registry.handleOutput("mon_1", `${HEARTBEAT}\n`); // a non-matching line re-arms
    clock.advanceBy(29_999);
    expect(monitorWakes()).toHaveLength(1);
    clock.advanceBy(1);
    expect(monitorWakes()).toHaveLength(2);
    done();
  });

  it("counts from start when the source never prints", async () => {
    const { clock, monitorWakes, done } = await setup({ params: { stall_after_ms: 10_000 } });
    clock.advanceBy(9_999);
    expect(monitorWakes()).toEqual([]);
    clock.advanceBy(1);
    expect(monitorWakes()).toHaveLength(1);
    done();
  });

  it("is silent after the monitor ended or was stopped by the model", async () => {
    const ended = await setup({ params: { stall_after_ms: 10_000 } });
    ended.registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: 1, duration_ms: 10 });
    ended.clock.advanceBy(60_000);
    expect(ended.monitorWakes()).toEqual([]);
    ended.done();
    const stopping = await setup({ params: { stall_after_ms: 10_000 } });
    stopping.registry.noteStopRequested("mon_1");
    stopping.clock.advanceBy(60_000);
    expect(stopping.monitorWakes()).toEqual([]);
    stopping.done();
  });
});

describe("monitor wake_on / stall_after_ms validation and listing", () => {
  it("rejects an invalid wake_on regex and out-of-range stall_after_ms at start", async () => {
    await expect(setup({ params: { wake_on: "ERROR(" } })).rejects.toThrow(RangeError);
    await expect(setup({ params: { wake_on: "ERROR(" } })).rejects.toThrow(/wake_on is not a valid regular expression/);
    await expect(setup({ params: { wake_on: "" } })).rejects.toThrow(RangeError);
    await expect(setup({ params: { stall_after_ms: 9_999 } })).rejects.toThrow(/stall_after_ms must be an integer between 10000 and 86400000/);
    await expect(setup({ params: { stall_after_ms: 86_400_001 } })).rejects.toBeInstanceOf(RangeError);
    await expect(setup({ params: { stall_after_ms: 15_000.5 } })).rejects.toBeInstanceOf(RangeError);
  });

  it("listActive carries the filter and stall threshold for task_list and post-compaction lists", async () => {
    const { registry, done } = await setup({ params: { wake_on: "ERROR", stall_after_ms: 60_000 } });
    expect(registry.listActive()[0]).toMatchObject({ wakeOn: "ERROR", stallAfterMs: 60_000 });
    done();
    const plain = await setup();
    expect(plain.registry.listActive()[0]).not.toHaveProperty("wakeOn");
    expect(plain.registry.listActive()[0]).not.toHaveProperty("stallAfterMs");
    plain.done();
  });

  it("exposes both params on the tool schema", () => {
    const registry = new MonitorRegistry({ getClient: () => null, sessionEnv: () => ({}), getNotifyCenter: () => null, trackTask: () => {} });
    const params = JSON.parse(JSON.stringify(createMonitorTool(registry).parameters)) as {
      properties: Record<string, { description?: string; minimum?: number; maximum?: number; type?: string }>;
    };
    expect(params.properties.wake_on?.type).toBe("string");
    expect(params.properties.wake_on?.description).toMatch(/only/i);
    expect(params.properties.stall_after_ms).toMatchObject({ minimum: 10_000, maximum: 86_400_000 });
  });
});
