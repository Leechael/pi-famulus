/**
 * Lifecycle tests for the pi-famulus intercom port, ported from
 * pi-famulus @ 98183fb plugin/pi lifecycle.test.mjs (node:test → vitest,
 * mock Unix socket → fake intercom transport + mocked timers).
 *
 * Models the manager-connection boundary, not plugin internals: requests and
 * connection-owned registrations are the same public boundary the port has.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { registerIntercom } from "../../src/intercom/intercom";
import type { MessagingStatus } from "../../src/manager-client";
import {
  createFakeCtx,
  createFakeManager,
  createFakePi,
  flush,
  makeTempDir,
} from "./intercom-harness";

const cleanups: (() => void)[] = [];
afterEach(() => {
  while (cleanups.length) cleanups.pop()!();
  vi.restoreAllMocks();
});

function lifecycleHarness(options: { autoUp?: boolean; messaging?: MessagingStatus } = {}) {
  const { autoUp = true } = options;
  const { dir, cleanup } = makeTempDir("fam-ic-life-");
  cleanups.push(cleanup);
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
  const fake = createFakeManager({ sessionId: state.sessionId });
  if (!autoUp) fake.setUp(false);
  const registrations = fake.registrations;
  let messaging: MessagingStatus = options.messaging ?? { state: "connected", name: "primary" };
  const status = vi.fn(async () => ({ sessions: [], messaging }));
  registerIntercom(pi.pi as any, {
    enabled: true,
    getClient: () => Object.assign(fake.getClient()!, { status }),
    extensionDirs: ["/nonexistent-extensions"],
  });
  const emit = (name: string, event: any = {}, context: any = ctx) => pi.handlers.get(name)?.(event, context);
  const afterShutdown: (() => void)[] = [];
  const notices: string[] = [];
  const ctxWithNotify: any = {
    ...ctx,
    ui: { notify: (message: string) => notices.push(message) },
  };
  // Keep the live sessionManager getter (a spread would freeze its value
  // and break stale-context simulation).
  Object.defineProperty(
    ctxWithNotify,
    "sessionManager",
    Object.getOwnPropertyDescriptor(ctx, "sessionManager")!,
  );
  return {
    fake, pi, ctx: ctxWithNotify, state, registrations, intervals, retiredIntervals,
    emit: (name: string, event: any = {}, context: any = ctxWithNotify) =>
      pi.handlers.get(name)?.(event, context),
    notices: () => [...notices],
    status,
    setMessaging: (value: MessagingStatus) => { messaging = value; },
    afterShutdown,
  };
}

it.each([undefined, false])("does not register any messaging surface without opt-in (%s)", (enabled) => {
  const pi = createFakePi();
  const getClient = vi.fn(() => null);
  registerIntercom(pi.pi as any, { enabled, getClient });
  expect(pi.tools.size).toBe(0);
  expect(pi.commands.size).toBe(0);
  expect(pi.handlers.size).toBe(0);
  expect(pi.messageRenderers.size).toBe(0);
  expect(getClient).not.toHaveBeenCalled();
});

it("preserves the environment disable override even with opt-in", () => {
  vi.stubEnv("PI_FAMULUS_NO_INTERCOM", "1");
  try {
    const pi = createFakePi();
    registerIntercom(pi.pi as any, { enabled: true });
    expect(pi.tools.size).toBe(0);
    expect(pi.commands.size).toBe(0);
    expect(pi.handlers.size).toBe(0);
  } finally {
    vi.unstubAllEnvs();
  }
});

it("reports startup relay errors despite a healthy local messaging port", async () => {
  const h = lifecycleHarness({ messaging: { state: "disconnected", name: "primary", error: "enrollment required: run pi-famulus messaging enroll" } });
  await h.emit("session_start");
  expect(h.notices().filter((m) => m.includes("enrollment required"))).toHaveLength(1);
  expect(h.registrations).toHaveLength(1);
  await h.emit("session_shutdown");
});

it("polls relay errors while locally registered, deduplicates, and resets on recovery", async () => {
  const h = lifecycleHarness();
  const setStatus = vi.fn();
  h.ctx.ui.setStatus = setStatus;
  await h.emit("session_start");
  const tick = async () => { [...h.intervals][0].callback(); await flush(); };
  h.setMessaging({ state: "disconnected", name: "primary", error: "relay certificate rejected" });
  await tick();
  await tick();
  expect(h.notices().filter((m) => m.includes("relay certificate rejected"))).toHaveLength(1);
  expect(setStatus).toHaveBeenLastCalledWith("famulus-messaging", expect.stringContaining("relay certificate rejected"));
  h.setMessaging({ state: "disconnected", name: "primary", error: "relay authentication failed" });
  await tick();
  expect(h.notices().filter((m) => m.includes("relay authentication failed"))).toHaveLength(1);
  h.setMessaging({ state: "connected", name: "primary" });
  await tick();
  expect(setStatus).toHaveBeenLastCalledWith("famulus-messaging", undefined);
  h.setMessaging({ state: "disconnected", name: "primary", error: "relay authentication failed" });
  await tick();
  expect(h.notices().filter((m) => m.includes("relay authentication failed"))).toHaveLength(2);
  expect(h.registrations).toHaveLength(1);
  await h.emit("session_shutdown");
});

it("announces recovery once after a reported relay outage, and only then", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  const tick = async () => { [...h.intervals][0].callback(); await flush(); };
  const reconnected = () => h.notices().filter((m) => m.includes("messaging reconnected"));
  await tick();
  expect(reconnected()).toHaveLength(0);
  h.setMessaging({ state: "disconnected", name: "primary", error: "IO error: Connection reset by peer (os error 104)" });
  await tick();
  h.setMessaging({ state: "connected", name: "primary" });
  await tick();
  await tick();
  expect(reconnected()).toEqual(["pi-famulus messaging reconnected"]);
  // Disabling during an outage ends it: turning messaging back on is not a recovery.
  h.setMessaging({ state: "disconnected", name: "primary", error: "relay disconnected" });
  await tick();
  h.setMessaging({ state: "disabled", name: "" });
  await tick();
  h.setMessaging({ state: "connected", name: "primary" });
  await tick();
  expect(reconnected()).toHaveLength(1);
  h.setMessaging({ state: "disconnected", name: "primary", error: "relay disconnected" });
  await tick();
  await h.emit("session_shutdown");
  expect(reconnected()).toHaveLength(1);
});

it("ignores a pending relay status response after shutdown", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  let respond!: (value: any) => void;
  h.status.mockImplementationOnce(() => new Promise((resolve) => { respond = resolve; }));
  [...h.intervals][0].callback();
  await flush();
  await h.emit("session_shutdown");
  respond({ sessions: [], messaging: { state: "disconnected", name: "primary", error: "late relay error" } });
  await flush();
  expect(h.notices().some((m) => m.includes("late relay error"))).toBe(false);
});

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

it("session_start rebinds a changed session id on a new connection", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  // famulus replaces the ManagerClient on /new: a cold connection with no
  // port; the extension re-registers the new session on it.
  h.fake.switchTo("replacement");
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
  // famulus replaces the ManagerClient on the switch: a cold connection
  // bound to the new session id.
  h.fake.switchTo("replacement");
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
  // The reconnect fires onReset, which re-arms the probe: the newest timer
  // belongs to the new lifecycle, retired ones stay inert.
  const retry2 = [...h.intervals].at(-1)!;
  retry2.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(2);
  expect(h.fake.registrations.map((r) => r.status)).toEqual(["idle", "busy"]);
  await h.emit("session_shutdown");
  await h.emit("session_start");
  expect(h.fake.registrations.length).toBe(3);
  h.fake.setUp(false);
  await flush();
  retry2.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(3);
  h.fake.setUp(true);
  const retry3 = [...h.intervals].at(-1)!;
  retry3.callback();
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

it("a cold start announces nothing while the first connect is in flight", async () => {
  const h = lifecycleHarness({ autoUp: false });
  // famulus's background connect is still in flight when session_start
  // runs: no outage may be announced for that. Session hooks do not block
  // on the connect either.
  h.fake.beginConnect();
  await h.emit("session_start");
  expect(h.fake.registrations.length).toBe(0);
  expect(h.notices().filter((m) => /unavailable|not running/.test(m))).toEqual([]);
  h.fake.setUp(true);
  // The reconnect re-arms the probe; its tick re-registers, quietly.
  const retry = [...h.intervals].at(-1)!;
  retry.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(1);
  expect(h.notices().some((m) => /intercom active/.test(m))).toBe(true);
  await h.emit("session_shutdown", {});
});

it("a genuinely failed startup connect reports once and still retries", async () => {
  const h = lifecycleHarness({ autoUp: false });
  h.fake.markConnectFailed();
  await h.emit("session_start");
  expect(h.notices().filter((m) => /messaging bridge unavailable/.test(m)).length).toBe(1);
  h.fake.beginConnect();
  h.fake.setUp(true);
  const retry = [...h.intervals].at(-1)!;
  retry.callback();
  await flush();
  expect(h.fake.registrations.length).toBe(1);
  expect(h.notices().filter((m) => /messaging bridge unavailable/.test(m)).length).toBe(1);
  await h.emit("session_shutdown", {});
});

it("unread peer messages are reported dropped over the old connection on a session switch", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.fake.push({ type: "deliver", session_id: "pi/primary", from: "chief", reply_to: "claude/origin", body: "orphan", message_id: "orphan" });
  await flush();
  expect(h.pi.injections.length).toBe(1);
  await h.emit("session_shutdown");
  // /new: famulus replaces the ManagerClient before the extension learns
  // about the switch.
  h.fake.switchTo("next-session");
  h.state.sessionId = "next-session";
  await h.emit("session_start", { reason: "new" });
  await flush();
  const receipts = h.fake.requests
    .filter((r: any) => r.op === "receipt" && r.message_id === "orphan")
    .map((r: any) => r.status);
  expect(receipts).toEqual(["dropped"]);
  expect(h.fake.registrations.at(-1)!.id).toBe("next-session");
  await h.emit("session_shutdown", {});
});

it("a reset event fences an in-flight register from the dead port", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.fake.held.add("register");
  const renaming = h.emit("session_info_changed", { name: "renamed" });
  await flush();
  const held = h.fake.waiters.at(-1)!;
  expect(held.req.op).toBe("register");
  // The bridge restarts while the rename register is in flight.
  h.fake.resetBridge();
  held.respond();
  await renaming;
  await flush();
  h.fake.held.delete("register");
  // The stale response must not revive the dead port: no "active" notice
  // for it, and the next activity re-registers from scratch.
  const actives = h.notices().filter((m) => /intercom active as "renamed"/.test(m));
  expect(actives).toEqual([]);
  // pi itself now reports the renamed session, so the re-register carries it.
  h.pi.sessionName = "renamed";
  await h.emit("turn_start");
  expect(h.fake.registrations.at(-1)!.name).toBe("renamed");
  await h.emit("session_shutdown", {});
});

it("a lost reset is recovered: the session re-registers transparently", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  expect(h.fake.registrations.length).toBe(1);
  // The intercom_reset event never reached us (full queue); the port died
  // silently underneath a still-registered session. An idempotent op
  // recovers; a body is never auto-retried (asserted in its own test).
  h.fake.killPortSilently();
  const listed = await h.pi.tools.get("ListSessions").execute("list", {});
  expect(listed.isError).not.toBe(true);
  expect(h.fake.registrations.length).toBe(2);
  const sent = await h.pi.tools.get("SendMessage").execute("send", { to: "claude/peer", message: "hello" });
  expect(sent.isError).not.toBe(true);
  expect(h.fake.requests.filter((r: any) => r.op === "send").length).toBe(1);
  await h.emit("session_shutdown", {});
});

it("a queued idempotent op interrupted by a bridge reset re-registers and retries", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  expect(h.fake.registrations.length).toBe(1);
  h.fake.resetBridgeSilently();
  const result = await h.pi.tools.get("ListSessions").execute("list", {});
  expect(result.isError).not.toBe(true);
  // Re-registered transparently, then the list was retried and answered.
  expect(h.fake.registrations.length).toBe(2);
  expect(h.fake.requests.filter((r: any) => r.op === "list").length).toBe(2);
  await h.emit("session_shutdown", {});
});

it("a send interrupted by a dead port is never auto-retried", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  // The port dies (bridge restart / lost reset): the tool's ops land on a
  // fresh unbound port. Idempotent ops recover transparently; the body is
  // never resent — the daemon refuses to promise it did not run.
  h.fake.failNextSend();
  const result = await h.pi.tools.get("SendMessage").execute("send", { to: "claude/peer", message: "hello" });
  expect(result.isError).toBe(true);
  // The body was NOT resent; the re-register still happened.
  expect(h.fake.requests.filter((r: any) => r.op === "send").length).toBe(1);
  expect(h.fake.registrations.length).toBe(2);
  // A later send works again without any further ceremony.
  const again = await h.pi.tools.get("SendMessage").execute("send-2", { to: "claude/peer", message: "again" });
  expect(again.isError).not.toBe(true);
  await h.emit("session_shutdown", {});
});

it("inbound transport messages do not use a disposed SDK context before the next probe", async () => {
  const h = lifecycleHarness();
  await h.emit("session_start");
  h.state.stale = true;
  expect(() =>
    h.fake.push({ type: "deliver", session_id: "pi/primary", body: "late", message_id: "late" }),
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
  h.fake.switchTo("fresh");
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
