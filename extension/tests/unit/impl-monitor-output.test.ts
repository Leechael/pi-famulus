import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { MonitorOutputBuffer } from "../../src/monitor-output";
import type { MonitorOutputRecord } from "../../src/manager-client";
import { formatMonitorEvent } from "../../src/format";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import type { MonitorOutputInfo } from "../../src/wake";
import type { ManagerClient } from "../../src/manager-client";

const record = (text: string, end: number, at = 0, fingerprint = text): MonitorOutputRecord => ({
  version: 1, kind: "line", observed_at: at, cursor_start: end - Buffer.byteLength(text + "\n"), cursor_end: end,
  preview: text, truncated: false, fingerprint: `blake3:v1:${fingerprint}`, timestamp_fingerprint: `blake3:iso-edge-v1:${fingerprint}`,
});

function setup(mode: "exact" | "ignore-timestamps" = "exact", idle = true) {
  const clock = new ManualClock();
  const batches: { text: string; output: MonitorOutputInfo }[] = [];
  const buffer = new MonitorOutputBuffer({ clock, windowMs: 60_000, mode, onFlush: (batch) => {
    batches.push(batch);
    if (idle) buffer.onDelivered();
    return true;
  } });
  return { clock, batches, buffer };
}

describe("Rust records through monitor registry (regression)", () => {
  it("falls back to original startup bytes when dense metadata exceeds the early-event budget", async () => {
    const clock = new ManualClock();
    const sent: { content: string }[] = [];
    const center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: (m) => sent.push(m) });
    let registry!: MonitorRegistry;
    const client = { ensureAvailable: async () => true, start: async () => {
      const raw = "HOST_READY\n" + "\n".repeat(300);
      const records = [record("HOST_READY", 11), ...Array.from({ length: 300 }, (_, i) => record("", 12 + i))];
      registry.handleOutput("mon_1", raw, Buffer.byteLength(raw), records);
      return { task_id: "mon_1" };
    }, watch: async () => {} } as unknown as ManagerClient;
    registry = new MonitorRegistry({ clock, getClient: () => client, sessionEnv: () => ({}), getNotifyCenter: () => center, trackTask: () => {} });
    await registry.start({ command: "startup status", description: "ready", persistent: true }, { cwd: "/tmp" } as never);
    clock.advanceBy(200);
    expect(sent).toHaveLength(1);
    expect(sent[0].content).toContain("HOST_READY");
    expect(sent[0].content).toContain('cursor-end="311"');
    registry.disposeAll(); center.dispose();
  });

  it.each([false, true])("reports pending repeat counts at timeout (exit races stop: %s)", async (exitDuringStop) => {
    const clock = new ManualClock();
    const sent: { details?: unknown }[] = [];
    const center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: (m) => sent.push(m) });
    let registry!: MonitorRegistry;
    const client = { ensureAvailable: async () => true, start: async () => ({ task_id: "mon_1" }), watch: async () => {}, stop: async () => {
      if (exitDuringStop) registry.handleExit("mon_1", { event: "task_exited", task_id: "mon_1", exit_code: null, signal: "SIGTERM" });
    } } as unknown as ManagerClient;
    registry = new MonitorRegistry({ clock, getClient: () => client, sessionEnv: () => ({}), getNotifyCenter: () => center, trackTask: () => {} });
    await registry.start({ command: "SSH checks", description: "SSH", timeout_ms: 1000 }, { cwd: "/tmp" } as never);
    registry.handleOutput("mon_1", "ssh_login_fail\n", 15, [record("ssh_login_fail", 15)]);
    clock.advanceBy(200); center.settled();
    registry.handleOutput("mon_1", "ssh_login_fail\n", 30, [record("ssh_login_fail", 30)]);
    clock.advanceBy(1000);
    await Promise.resolve(); await Promise.resolve();
    const terminal = sent.map((m) => m.details as { status?: string; output?: MonitorOutputInfo }).find((w) => w.status === "timeout");
    expect(terminal?.output?.suppressedCount).toBe(1);
    expect(sent).toHaveLength(2);
    registry.disposeAll(); center.dispose();
  });

  it("suppresses repeated records before wakes while preserving a distinct READY and raw read coordinates", async () => {
    const clock = new ManualClock();
    const sent: { content: string; details?: unknown }[] = [];
    const center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: (m) => sent.push(m) });
    const client = { ensureAvailable: async () => true, start: async () => ({ task_id: "mon_1" }), watch: async () => {} } as unknown as ManagerClient;
    const registry = new MonitorRegistry({ clock, getClient: () => client, sessionEnv: () => ({}), getNotifyCenter: () => center, trackTask: () => {} });
    await registry.start({ command: "SSH checks", description: "SSH", persistent: true }, { cwd: "/tmp" } as never);
    registry.handleOutput("mon_1", "ssh_login_fail\n", 15, [record("ssh_login_fail", 15, 1)]);
    clock.advanceBy(200); center.settled();
    registry.handleOutput("mon_1", "ssh_login_fail\n", 30, [record("ssh_login_fail", 30, 2)]);
    clock.advanceBy(200); center.settled();
    expect(sent).toHaveLength(1);
    registry.handleOutput("mon_1", "HOST_READY\n", 41, [record("HOST_READY", 41, 3)]);
    clock.advanceBy(200);
    expect(sent).toHaveLength(2);
    expect(sent[1].content).toContain("HOST_READY");
    expect(sent[1].content).toContain('duplicates-suppressed="1"');
    expect(sent[1].content).toContain('cursor-end="41"');
    registry.disposeAll(); center.dispose();
  });
});

describe("monitor output window (regression: repeated SSH checks)", () => {
  it("emits alternating repeated states once per fixed window, and new READY immediately", () => {
    const { clock, batches, buffer } = setup();
    buffer.push(record("port22_open", 12));
    buffer.push(record("ssh_login_fail", 27));
    clock.advanceBy(200);
    expect(batches).toHaveLength(1);
    for (let i = 1; i <= 3; i++) {
      clock.advanceBy(15_000);
      buffer.push(record("port22_open", 27 + i * 27 - 15));
      buffer.push(record("ssh_login_fail", 27 + i * 27));
      clock.advanceBy(200);
    }
    expect(batches).toHaveLength(1);
    buffer.push(record("HOST_READY", 120));
    clock.advanceBy(200);
    expect(batches).toHaveLength(2);
    expect(batches[1].text).toContain("HOST_READY");
    expect(batches[1].output.suppressedCount).toBe(6);
    clock.advanceBy(15_000);
    buffer.push(record("ssh_login_fail", 150));
    clock.advanceBy(200);
    expect(batches).toHaveLength(3); // Repeats did not extend the original window.
    buffer.dispose();
  });

  it("starts the window on delivery, suppressing pending duplicates while the model is busy", () => {
    const { clock, batches, buffer } = setup("exact", false);
    buffer.push(record("ssh_login_fail", 15));
    clock.advanceBy(200);
    clock.advanceBy(120_000);
    buffer.push(record("ssh_login_fail", 30));
    clock.advanceBy(200);
    expect(batches).toHaveLength(1);
    buffer.onDelivered();
    clock.advanceBy(59_000);
    buffer.push(record("ssh_login_fail", 45));
    clock.advanceBy(200);
    expect(batches).toHaveLength(1);
    clock.advanceBy(1_000);
    buffer.push(record("ssh_login_fail", 60));
    clock.advanceBy(200);
    expect(batches).toHaveLength(2);
    buffer.dispose();
  });

  it("does not start a suppression window when no NotifyCenter is available", async () => {
    const clock = new ManualClock();
    const sent: { content: string }[] = [];
    let center: NotifyCenter | null = null;
    const client = { ensureAvailable: async () => true, start: async () => ({ task_id: "mon_1" }), watch: async () => {} } as unknown as ManagerClient;
    const registry = new MonitorRegistry({
      clock, getClient: () => client, sessionEnv: () => ({}),
      getNotifyCenter: () => center, trackTask: () => {},
    });
    await registry.start({ command: "SSH checks", description: "SSH", persistent: true }, { cwd: "/tmp" } as never);
    registry.handleOutput("mon_1", "ssh_login_fail\n", 15, [record("ssh_login_fail", 15)]);
    clock.advanceBy(200);
    expect(sent).toHaveLength(0);
    center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: (m) => sent.push(m) });
    registry.handleOutput("mon_1", "ssh_login_fail\n", 30, [record("ssh_login_fail", 30)]);
    clock.advanceBy(200);
    expect(sent).toHaveLength(1);
    expect(sent[0].content).toContain("ssh_login_fail");
    registry.disposeAll(); center.dispose();
  });

  it("does not reserve a suppression window when a batch is dropped by overload protection", () => {
    const clock = new ManualClock();
    const text: string[] = [];
    let accept = false;
    const buffer = new MonitorOutputBuffer({ clock, onFlush: (batch) => {
      text.push(batch.text);
      if (accept) buffer.onDelivered();
      return accept;
    } });
    buffer.push(record("ssh_login_fail", 15)); clock.advanceBy(200);
    accept = true;
    buffer.push(record("ssh_login_fail", 30)); clock.advanceBy(200);
    expect(text).toEqual(["ssh_login_fail", "ssh_login_fail"]);
    buffer.push(record("ssh_login_fail", 45)); clock.advanceBy(200);
    expect(text).toHaveLength(2);
    expect(buffer.takeSummary().suppressedCount).toBe(1);
    expect(buffer.takeSummary().suppressedCount).toBeUndefined();
    buffer.dispose();
  });

  it("never deduplicates incomplete progress, and does not merge different full fingerprints with equal previews", () => {
    const { clock, batches, buffer } = setup();
    const samePreview = record("long shared preview…", 1000, 0, "tail-A");
    buffer.push(samePreview); clock.advanceBy(200);
    buffer.push({ ...samePreview, cursor_start: 1000, cursor_end: 2000, fingerprint: "blake3:v1:tail-B" }); clock.advanceBy(200);
    const progress = { ...record("progress", 2009), kind: "progress" as const };
    buffer.push(progress); clock.advanceBy(200);
    buffer.push({ ...progress, cursor_start: 2009, cursor_end: 2018 }); clock.advanceBy(200);
    expect(batches).toHaveLength(4);
    buffer.dispose();
  });

  it("does not suppress an identity whose representative text was omitted from delivered context", () => {
    const { clock, batches, buffer } = setup("exact", false);
    buffer.push(record("first-event", 12));
    for (let i = 0; i < 50; i++) buffer.push(record(`filler-${i}: ${"x".repeat(450)}`, 1000 + i * 500));
    buffer.push(record("HOST_READY", 27000));
    for (let i = 0; i < 50; i++) buffer.push(record(`tail-${i}: ${"y".repeat(450)}`, 28000 + i * 500));
    clock.advanceBy(200);
    expect(batches[0].text).not.toContain("HOST_READY");
    buffer.onDelivered(batches[0].text);
    buffer.push(record("HOST_READY", 54000));
    clock.advanceBy(200);
    expect(batches).toHaveLength(2);
    expect(batches[1].text).toContain("HOST_READY");
    buffer.dispose();
  });

  it.each(["HOST_READY", "30 events"])("keeps omitted %s eligible after busy aggregation", (state) => {
    const clock = new ManualClock();
    let idle = false;
    const sent: { content: string }[] = [];
    const center = new NotifyCenter({ clock, isIdle: () => idle, sendMessage: (m) => sent.push(m) });
    const buffer = new MonitorOutputBuffer({ clock, onFlush: (batch) => {
      center.notifyMonitorEvent("state", "mon_1", batch.text, 0, { output: batch.output, onDelivered: buffer.onDelivered });
      return true;
    } });
    for (let i = 0; i < 30; i++) {
      buffer.push(record(i === 15 ? state : `status-${i}: ${"x".repeat(400)}`, (i + 1) * 500));
      clock.advanceBy(200);
    }
    idle = true; center.settled();
    expect(sent).toHaveLength(1);
    buffer.push(record(state, 16000)); clock.advanceBy(200);
    expect(sent).toHaveLength(2);
    buffer.dispose(); center.dispose();
  });

  it("does not acknowledge an omitted READY merely because NOT_READY is visible", () => {
    const { clock, batches, buffer } = setup("exact", false);
    buffer.push(record("READY", 6)); clock.advanceBy(200);
    buffer.onDelivered("NOT_READY");
    buffer.push(record("READY", 12)); clock.advanceBy(200);
    expect(batches).toHaveLength(2);
    buffer.dispose();
  });

  it("fails open when distinct fingerprints share a truncated preview", () => {
    const { clock, batches, buffer } = setup("exact", false);
    const preview = "long shared preview…";
    const a = { ...record(preview, 1000, 0, "tail-A"), truncated: true };
    const b = { ...record(preview, 2000, 1, "tail-B"), truncated: true, cursor_start: 1000 };
    buffer.push(a);
    buffer.push(b);
    clock.advanceBy(200);
    buffer.onDelivered(batches[0].text);
    buffer.push({ ...a, cursor_start: 2000, cursor_end: 3000 });
    clock.advanceBy(200);
    buffer.push({ ...b, cursor_start: 3000, cursor_end: 4000 });
    clock.advanceBy(200);
    expect(batches).toHaveLength(3);
    buffer.dispose();
  });

  it("retains bounded head/tail previews and explicit truncation metadata", () => {
    const { clock, batches, buffer } = setup();
    for (let i = 0; i < 100; i++) buffer.push(record(`${i}: ${"x".repeat(490)}`, (i + 1) * 500));
    clock.advanceBy(200);
    expect(batches[0].text.length).toBeLessThanOrEqual(3000);
    expect(batches[0].text).toContain("0:");
    expect(batches[0].text).toContain("99:");
    expect(batches[0].output.truncated).toBe(true);
    expect(batches[0].output.omittedChars).toBeGreaterThan(0);
    buffer.dispose();
  });

  it("uses only the chosen Rust fingerprint; default exact does not ignore timestamps", () => {
    const { clock, batches, buffer } = setup();
    const first = record("fail 2026-10-15T23:20:15Z", 30, 1, "first");
    const second = { ...record("fail 2026-10-15T23:20:38Z", 60, 2, "second"), timestamp_fingerprint: first.timestamp_fingerprint };
    buffer.push(first); clock.advanceBy(200);
    buffer.push(second); clock.advanceBy(200);
    expect(batches).toHaveLength(2);
    buffer.dispose();
    const normalized = setup("ignore-timestamps");
    normalized.buffer.push(first); normalized.clock.advanceBy(200);
    normalized.buffer.push(second); normalized.clock.advanceBy(200);
    expect(normalized.batches).toHaveLength(1);
    normalized.buffer.dispose();
  });

  it("keeps original coordinates and observation times separate from emission time", () => {
    const { clock, batches, buffer } = setup();
    buffer.push(record("中", 4, 10));
    clock.advanceBy(5_000);
    expect(batches[0].output).toMatchObject({ firstObservedAt: 10, lastObservedAt: 10, cursorStart: 0, cursorEnd: 4 });
    const wake = formatMonitorEvent("SSH", "mon_1", batches[0].text, undefined, { output: batches[0].output });
    expect(wake.content).toContain('cursor-start="0" cursor-end="4"');
    expect(wake.content).toContain("task_output");
    buffer.dispose();
  });
});
