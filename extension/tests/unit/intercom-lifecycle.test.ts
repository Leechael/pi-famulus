/**
 * Lifecycle tests for the pi-famulus intercom port, ported from
 * agent-intercom @ 98183fb plugin/pi lifecycle.test.mjs (node:test → vitest,
 * mock Unix socket → fake intercom transport + mocked timers).
 *
 * Models the manager-connection boundary, not plugin internals: requests and
 * connection-owned registrations are the same public boundary the port has.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { registerIntercom } from "../../src/intercom/intercom";
import {
  createFakeCtx,
  createFakePi,
  createFakeTransport,
  flush,
  makeTempDir,
  type FakeTransport,
} from "./intercom-harness";

const cleanups: (() => void)[] = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()!();
  vi.restoreAllMocks();
});

function lifecycleHarness(options: { autoUp?: boolean; delayedClose?: boolean } = {}) {
  const { autoUp = true } = options;
  const { dir, cleanup } = makeTempDir("fam-ic-life-");
  cleanups.push(cleanup);
  const fake: FakeTransport = createFakeTransport();
  if (!autoUp) fake.setUp(false);

  const intervals = new Set<{ callback: () => void; unref?: () => void }>();
  const retiredIntervals: { callback: () => void }[] = [];
  vi.spyOn(globalThis, "setInterval").mockImplementation(((callback: () => void) => {
    const timer = { callback, unref: () => {} };
    intervals.add(timer);
    return timer as any;
  }) as any);
  vi.spyOn(globalThis, "clearInterval").mockImplementation(((timer: { callback: () => void }) => {
    intervals.delete(timer);
    retiredIntervals.push(timer);
  }) as any);

  const pi = createFakePi();
  const { ctx, state } = createFakeCtx(dir, "primary");
  const registrations = fake.registrations;
  registerIntercom(pi.pi as any, { transport: fake.transport, extensionDirs: ["/nonexistent-extensions"] });
  const emit = (name: string, event: any = {}, context: any = ctx) => pi.handlers.get(name)?.(event, context);
  const afterShutdown: (() => void)[] = [];
  return {
    fake, pi, ctx, state, registrations, intervals, retiredIntervals,
    emit, afterShutdown,
  };
}

// Regression: a dead extension instance must not be revived by queued hooks,
// a previously queued retry tick, or a tool retaining its old closure.
it("shutdown is terminal until an explicit session_start", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  expect(h.registrations.length).toBe(1);
  const retry = [...h.intervals][0];
  await h.emit("session_shutdown", { reason: "new" });
  expect(h.registrations.length).toBe(1); // the unregister is best-effort; the server closes the port
  await h.emit("turn_start");
  await h.emit("session_info_changed", { name: "old runtime" });
  retry.callback();
  const result = await h.pi.tools.get("ListSessions").execute("stale-list", {});
  expect(result.isError).toBe(true);
  expect(h.intervals.size).toBe(0);
  await h.emit("session_start", { reason: "new" });
  expect(h.registrations.length).toBe(2);
});

it("tools and concurrent hooks await a single registration", async () => {
  const h = lifecycleHarness();
  // The manager connection is up but the register op is held back (a slow
  // bridge); hooks and tools must share the one in-flight registration.
  h.fake.held.add("register");
  const starting = h.emit("session_start");
  await flush();
  const turning = h.emit("turn_start");
  let completed = false;
  const listing = h.pi.tools
    .get("ListSessions")
    .execute("list", {})
    .then((result: any) => {
      completed = true;
      return result;
    });
  await flush();
  expect(completed).toBe(false);
  expect(h.fake.requests.map((r: any) => r.op)).toEqual(["register"]);
  const waiter = h.fake.waiters.find((w: any) => w.req.op === "register");
  expect(waiter).toBeDefined();
  waiter!.respond();
  await Promise.all([starting, turning]);
  expect((await listing).isError).not.toBe(true);
  expect(h.fake.requests.map((r: any) => r.op)).toEqual(["register", "list"]);
  await h.emit("session_shutdown", {});
});

it("session_start rebinds a changed session id on a live connection", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.state.sessionId = "replacement";
  await h.emit("session_start", { reason: "switch" });
  expect(h.registrations.length).toBe(2);
  expect(h.registrations.at(-1)!.id).toBe("replacement");
  const sent = await h.pi.tools.get("SendMessage").execute("send", { to: "claude/peer", message: "hello" });
  expect(sent.isError).not.toBe(true);
  expect(h.fake.requests.find((r: any) => r.op === "send")!.reply_to).toBe("pi/replacement");
  await h.emit("session_shutdown", {});
});

it("late activity and compaction hooks cannot touch a shut-down context", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  await h.emit("session_shutdown");
  h.state.stale = true;
  for (const hook of [
    "turn_start",
    "session_info_changed",
    "session_before_compact",
    "session_compact",
    "session_compact_failed",
    "agent_start",
    "agent_end",
    "agent_settled",
    "message_end",
    "before_agent_start",
  ]) {
    await expect(Promise.resolve(h.emit(hook, { reason: "manual", systemPrompt: "BASE" })), hook).resolves.toBeUndefined();
  }
  expect(h.intervals.size).toBe(0);
});

it("connected context invalidation cleans up but valid idle SDK sessions stay registered", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  const retry = [...h.intervals][0];
  for (let i = 0; i < 5; i++) {
    retry.callback();
    await flush();
    expect(h.registrations.length).toBe(1);
  }
  expect(h.fake.requests.filter((r) => r.op === "unregister").length).toBe(0);
  h.state.stale = true;
  retry.callback();
  await flush();
  expect(h.intervals.size).toBe(0);
  retry.callback();
  await flush();
  expect(h.registrations.length).toBe(1);
  await h.emit("session_shutdown", {});
});

it("a bridge reset retires the registration without poisoning the replacement", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.fake.held.add("list");
  let completed = false;
  const listing = h.pi.tools
    .get("ListSessions")
    .execute("replacement-list", {})
    .then((result: any) => {
      completed = true;
      return result;
    });
  await flush();
  const request = h.fake.waiters.find((w: any) => w.req.op === "list");
  expect(request).toBeDefined();
  // The bridge restarted mid-request: the manager fails the pending op.
  h.fake.resetBridge();
  h.fake.rejectHeld();
  await flush();
  expect(completed).toBe(true);
  expect((await listing).isError).toBe(true);
  // The next activity re-registers on the new bridge.
  await h.emit("turn_start");
  expect(h.registrations.length).toBe(2);
  await h.emit("session_shutdown", {});
});

it("shutdown cancels an unacknowledged registration without a late revival", async () => {
  const h = lifecycleHarness();
  h.fake.held.add("register");
  const starting = h.emit("session_start");
  await flush();
  const request = h.fake.waiters.find((w: any) => w.req.op === "register");
  expect(request).toBeDefined();
  const stopping = h.emit("session_shutdown");
  h.fake.rejectHeld();
  await Promise.all([starting, stopping]);
  h.fake.held.delete("register");
  const restarting = h.emit("session_start");
  await restarting;
  // A late reply for the cancelled registration must not mark it live.
  request!.respond();
  await flush();
  expect(h.registrations.length).toBe(1);
  const result = await h.pi.tools.get("SendMessage").execute("new-send", { to: "claude/peer", message: "hello" });
  expect(result.isError).not.toBe(true);
  expect(h.fake.requests.find((r: any) => r.op === "send")!.reply_to).toBe("pi/primary");
  await h.emit("session_shutdown", {});
});

it("explicit shutdown is bounded and idempotent if unregister never replies", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.fake.held.add("unregister");
  const stopping = h.emit("session_shutdown", { reason: "exit" });
  const stoppingAgain = h.emit("session_shutdown", { reason: "exit" });
  expect(h.fake.requests.filter((r) => r.op === "unregister").length).toBe(1);
  let timer: ReturnType<typeof setTimeout>;
  try {
    await Promise.race([
      Promise.all([stopping, stoppingAgain]),
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("shutdown exceeded one second")), 1000);
      }),
    ]);
  } finally {
    clearTimeout(timer!);
  }
  // A new start while old cleanup waits must not be destroyed by it.
  await h.emit("session_start");
  expect(h.registrations.length).toBe(2);
  expect(h.intervals.size).toBe(1);
  await h.emit("session_shutdown", {});
});

it("an old tool waiting on list cannot send through a replacement runtime", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.fake.held.add("list");
  const sending = h.pi.tools.get("SendMessage").execute("old-send", { to: "claude/peer", message: "stale" });
  await flush();
  const stopping = h.emit("session_shutdown");
  const restarting = h.emit("session_start");
  await Promise.all([stopping, restarting]);
  h.fake.rejectHeld();
  const result = await sending;
  expect(result.isError).toBe(true);
  expect(h.fake.requests.filter((r) => r.op === "send")).toEqual([]);
  await h.emit("session_shutdown", {});
});

it("rename waits for initial registration and cannot overwrite replacement identity", async () => {
  const h = lifecycleHarness();
  h.fake.held.add("register");
  const starting = h.emit("session_start");
  await flush();
  const initial = h.fake.waiters.find((w: any) => w.req.op === "register");
  expect(initial).toBeDefined();
  const renaming = h.emit("session_info_changed", { name: "renamed" });
  await flush();
  expect(h.fake.requests.filter((r) => r.op === "register").length).toBe(1);
  initial!.respond();
  await starting;
  await flush();
  // The rename re-register is also held; resolve it, then replace the
  // session before the callback continues.
  const renameWaiter = h.fake.waiters.at(-1)!;
  expect(renameWaiter.req.op).toBe("register");
  renameWaiter.respond();
  h.fake.held.delete("register");
  h.state.sessionId = "replacement";
  const replacement = h.emit("session_start", { reason: "switch" });
  await Promise.all([renaming, replacement]);
  const sent = await h.pi.tools.get("SendMessage").execute("send", { to: "claude/peer", message: "hello" });
  expect(sent.isError).not.toBe(true);
  expect(h.fake.requests.filter((r: any) => r.op === "send").at(-1)!.reply_to).toBe("pi/replacement");
  await h.emit("session_shutdown", {});
});

it("a manager disconnect retries while busy without reviving retired retries", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  const retry = [...h.intervals][0];
  h.fake.setUp(false);
  await flush();
  h.state.idle = false;
  retry.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(1);
  h.fake.setUp(true);
  retry.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(2);
  expect(h.fake.registrations.map((r) => r.status)).toEqual(["idle", "busy"]);
  await h.emit("session_shutdown");
  await h.emit("session_start");
  expect(h.fake.registrations.length).toBe(3);
  h.fake.setUp(false);
  await flush();
  retry.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(3);
  h.fake.setUp(true);
  [...h.intervals][0]!.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(4);
  await h.emit("session_shutdown", {});
});

it("a disposed SDK context cancels even an in-flight registration without a shutdown hook", async () => {
  const h = lifecycleHarness({ autoUp: false });
  const starting = h.emit("session_start");
  await flush();
  h.state.stale = true;
  h.fake.setUp(true);
  [...h.intervals][0]!.callback();
  await starting;
  expect(h.registrations.length).toBe(0);
  expect(h.intervals.size).toBe(0);
});

it("inbound transport messages do not use a disposed SDK context before the next probe", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.state.stale = true;
  expect(() =>
    h.fake.push({ type: "deliver", body: "late", message_id: "late" }),
  ).not.toThrow();
  await flush();
  expect(h.pi.injections).toEqual([]);
  expect(h.intervals.size).toBe(0);
  await h.emit("session_shutdown", {});
});

it("queued old-context turns and commands cannot rebind or poison a fresh lifecycle", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  const oldCtx = h.ctx;
  await h.emit("session_shutdown");
  const freshCtx = { ...oldCtx, sessionManager: { getSessionId: () => "fresh" } };
  await h.emit("session_start", {}, freshCtx);
  // A valid-looking old ID still cannot replace the explicit current session.
  await h.emit("turn_start", {}, oldCtx);
  expect(h.registrations.at(-1)!.id).toBe("fresh");
  h.state.stale = true;
  await h.pi.commands.get("rename")!.handler("stale name", oldCtx);
  await h.emit("turn_start", {}, oldCtx);
  [...h.intervals][0]!.callback();
  await flush();
  expect(h.registrations.length).toBe(2);
  expect(h.registrations.at(-1)!.id).toBe("fresh");
  await h.emit("session_shutdown", {}, freshCtx);
});
