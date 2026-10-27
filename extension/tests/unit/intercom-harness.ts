/**
 * Test harness: a fake pi-famulus MANAGER modelling the real contract the
 * extension talks through ManagerClient + protocol 7:
 *
 * - one manager CONNECTION per ManagerClient (a new, cold connection per
 *   session switch, matching famulus creating a new ManagerClient);
 * - one port per connection, bound to the hello session id on register
 *   (identity mismatch refused), with the manager-port op surface (before
 *   register only register/list/message_status; the plugin's ops after);
 * - bridge-wide disabled/down/up state; coming back up models the
 *   ManagerClient reconnect (ports die, onReconnect fires);
 * - `resetBridge()` kills every port and pushes intercom_reset per
 *   connection;
 * - held ops and per-request reply overrides like the old harness.
 *
 * The extension's real `createManagerTransport` runs over the client
 * facades, so transport behavior is under test too.
 */

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import type { ManagerClient } from "../../src/manager-client";
import { IntercomError, type IntercomTransport } from "../../src/intercom/transport";

export interface FakeWaiter {
  req: Record<string, any>;
  conn: FakeConnection;
  respond: (extra?: Record<string, unknown>) => void;
  fail: (error: Error) => void;
}

interface FakeConnection {
  id: number;
  sessionId: string;
  up: boolean;
  /** The registered bridge id (`pi/<sessionId>`), null before register. */
  bound: string | null;
  /** The first op after a silent reset is refused with E_INTERCOM_RESET. */
  pendingResetError: boolean;
  /** The next send op is refused "session not registered" (a send racing
   * a dead port). */
  failNextSend: boolean;
  frameHandlers: Set<(frame: Record<string, unknown>) => void>;
  resetHandlers: Set<() => void>;
}

export interface FakeManager {
  /** Pass to registerIntercom as getClient. */
  getClient: () => ManagerClient | null;
  /** Every op any connection issued, in order. */
  requests: Record<string, any>[];
  /** Every session record sent with op register. */
  registrations: Record<string, any>[];
  /** Ops whose response is held back until released (by name). */
  held: Set<string>;
  /** Held requests with manual responders. */
  waiters: FakeWaiter[];
  /** Push one bridge frame (deliver/receipt/idle_notice) to the connection
   * whose port owns the frame's session_id. */
  push: (frame: Record<string, unknown>) => void;
  /** The bridge restarted: every port dies, `intercom_reset` reaches each
   * connection. */
  resetBridge: () => void;
  /** The current connection's ManagerClient reconnected (port dies,
   * onReconnect fires). */
  reconnect: () => void;
  /** A session switch: the old connection dies, a new cold one (no port)
   * becomes current. */
  switchTo: (sessionId: string) => void;
  /** Kill the current connection's port WITHOUT any reset notice (a lost
   * intercom_reset): the next op gets "session not registered". */
  killPortSilently: () => void;
  /** Like killPortSilently, but the daemon answers the next op with
   * E_INTERCOM_RESET (queued across a bridge restart). */
  resetBridgeSilently: () => void;
  /** The next send op is refused "session not registered" (a send racing a
   * dead port), regardless of registration state. */
  failNextSend: () => void;
  /** The manager connection (re)opened or dropped. Coming back up models a
   * reconnect: ports die, onReconnect fires. */
  setUp: (up: boolean) => void;
  /** Model a connect in flight (famulus's background connect): the next
   * setUp(true) resolves it. */
  beginConnect: () => void;
  /** The manager connection failed for real (transport level). */
  markConnectFailed: () => void;
  setDisabled: (disabled: boolean) => void;
  setDown: (reason: string | null) => void;
  rejectHeld: (message?: string) => void;
  /** Per-request response overrides, like the old harness's `reply`. */
  reply?: (req: Record<string, any>) => Record<string, unknown> | undefined;
  isUp: () => boolean;
  /** Test probe. */
  debugConnections: () => { id: number; up: boolean; bound: string | null; handlers: number }[];
}

export function createFakeManager(options: {
  reply?: (req: Record<string, any>) => Record<string, unknown> | undefined;
  /** The hello session id of the initial connection (what famulus's first
   * ManagerClient hellos with); later connections come from switchTo. */
  sessionId?: string;
} = {}): FakeManager {
  const requests: Record<string, any>[] = [];
  const registrations: Record<string, any>[] = [];
  const held = new Set<string>();
  const waiters: FakeWaiter[] = [];
  const connections: FakeConnection[] = [];
  let current: FakeConnection | null = null;
  let up = true;
  let disabled = false;
  let down: string | null = null;
  let connecting: { resolve: (ok: boolean) => void } | null = null;
  let connectFailed = false;
  let seq = 0;

  // famulus connects the first ManagerClient before the extension's
  // session_start: model that initial connection.
  current = {
    id: ++seq,
    sessionId: options.sessionId ?? "target-session",
    up,
    bound: null,
    pendingResetError: false,
    failNextSend: false,
    frameHandlers: new Set(),
    resetHandlers: new Set(),
  };
  connections.push(current);

  const makeConnection = (sessionId: string): FakeConnection => ({
    id: ++seq,
    sessionId,
    up,
    bound: null,
    pendingResetError: false,
    failNextSend: false,
    frameHandlers: new Set(),
    resetHandlers: new Set(),
  });

  const rows = () =>
    connections
      .filter((c) => c.up && c.bound)
      .map((c) => {
        const registration = registrations.filter((r: any) => `pi/${r.id}` === c.bound).at(-1);
        return { id: c.bound, name: registration?.name ?? "", harness: "pi", status: registration?.status ?? "idle", cwd: registration?.cwd ?? "" };
      });

  const defaultReply = (conn: FakeConnection, req: Record<string, any>): Record<string, unknown> => {
    switch (req.op) {
      case "register":
        registrations.push(req.session);
        // The daemon forces pi/<hello session id> whatever the op sent.
        conn.bound = typeof req.session?.id === "string" && req.session.id.startsWith("pi/")
          ? req.session.id
          : `pi/${req.session?.id}`;
        return { session_id: conn.bound };
      case "unregister":
        conn.bound = null;
        return {};
      case "list":
        return { sessions: rows() };
      case "send":
        return { message_id: `m-${++seq}`, status: "delivered" };
      case "receipt":
        return {};
      case "subscribe_idle":
        return { subscription_id: `sub-${++seq}`, status: "subscribed" };
      case "message_status":
        return { message_id: req.message_id, status: "unknown" };
      case "session_name":
        return {};
      default:
        return {};
    }
  };

  /** The manager-port op surface, mirroring Port::handle. */
  const route = async (conn: FakeConnection, op: Record<string, unknown>): Promise<Record<string, unknown>> => {
    const req = op as Record<string, any>;
    requests.push(req);
    if (!conn.up) throw new IntercomError("pi-famulus is not connected", "E_TRANSPORT", "unavailable");
    if (disabled) throw new IntercomError("disabled by PI_FAMULUS_NO_INTERCOM", "E_INTERCOM_DISABLED", "disabled");
    if (down) throw new IntercomError(down, "E_INTERCOM_UNAVAILABLE", "unavailable");
    const opName = req.op as string;
    if (conn.pendingResetError) {
      // An op queued when the bridge restarted, answered before it ran —
      // ahead of the (unbound) allowlist, like the daemon's reset drain.
      conn.pendingResetError = false;
      return { error: "intercom reset: re-register and retry idempotent ops", code: "E_INTERCOM_RESET" };
    }
    if (!conn.bound) {
      const allowedUnbound = ["register", "list"];
      if (!allowedUnbound.includes(opName)) return { error: "session not registered" };
    } else {
      const allowed = ["register", "unregister", "list", "send", "receipt", "subscribe_idle"];
      if (!allowed.includes(opName)) return { error: `op ${opName} is not available to a pi session over the manager connection` };
    }
    if (conn.failNextSend && opName === "send") {
      conn.failNextSend = false;
      return { error: "session not registered" };
    }
    if (opName === "register") {
      // Identity binding, like the daemon: the raw hello key or the forced
      // pi/<key> id both register this connection's session.
      if (req.session?.id !== conn.sessionId && req.session?.id !== conn.bound) {
        return { error: `session id mismatch: this connection is bound to pi/${conn.sessionId}` };
      }
    }
    if (held.has(opName)) {
      return await new Promise<Record<string, unknown>>((resolve, reject) => {
        let settled = false;
        waiters.push({
          req,
          conn,
          respond: (extra) => {
            if (settled) return;
            settled = true;
            resolve({ ...defaultReply(conn, req), ...extra });
          },
          fail: (error) => {
            if (settled) return;
            settled = true;
            reject(error);
          },
        });
      });
    }
    return { ...defaultReply(conn, req), ...options.reply?.(req) };
  };

  const clientFor = (conn: FakeConnection): ManagerClient =>
    ({
      isAvailable: () => conn.up,
      protocolLevel: () => 7,
      managerInfo: () => (conn.up ? { version: "fake", pid: 0 } : null),
      lastError: () => (conn.up ? "" : connectFailed ? "connect failed" : ""),
      ensureAvailable: async () => {
        if (conn.up) return true;
        if (connecting) await new Promise<boolean>((resolve) => (connecting!.resolve = resolve));
        return conn.up;
      },
      status: async () => ({ sessions: [], messaging: { state: "connected", name: "fake" } }),
      intercom: (op: Record<string, unknown>) => route(conn, op),
      onEvent: (handler: (event: any) => void) => {
        const wrapped = (event: any) => {
          if (event && typeof event === "object") handler(event);
        };
        conn.frameHandlers.add(wrapped);
        return () => conn.frameHandlers.delete(wrapped);
      },
      onReconnect: (handler: () => void) => {
        conn.resetHandlers.add(handler);
        return () => conn.resetHandlers.delete(handler);
      },
    }) as unknown as ManagerClient;

  const killPort = (conn: FakeConnection, fireReset: boolean) => {
    conn.bound = null;
    if (fireReset) for (const handler of [...conn.resetHandlers]) handler();
  };

  const manager: FakeManager = {
    getClient: () => (current ? clientFor(current) : null),
    requests,
    registrations,
    held,
    waiters,
    push: (frame) => {
      const target = connections.find((c) => c.up && c.bound && c.bound === frame.session_id);
      if (!target) return;
      for (const handler of [...target.frameHandlers]) handler({ event: "intercom", frame });
    },
    resetBridge: () => {
      for (const conn of connections) if (conn.up) killPort(conn, true);
    },
    reconnect: () => {
      if (current) killPort(current, true);
    },
    switchTo: (sessionId) => {
      // famulus replaces the ManagerClient but does not close the old
      // connection synchronously: it stays alive (and its port with it)
      // until famulus's teardown or GC closes it.
      current = makeConnection(sessionId);
      connections.push(current);
    },
    killPortSilently: () => {
      if (current) current.bound = null;
    },
    resetBridgeSilently: () => {
      // The intercom_reset event was lost (full out-queue); the port died
      // and the next op is answered with E_INTERCOM_RESET.
      if (current) {
        current.bound = null;
        current.pendingResetError = true;
      }
    },
    failNextSend: () => {
      if (current) current.failNextSend = true;
    },
    setUp: (value) => {
      const reconnected = value && !up;
      up = value;
      for (const conn of connections) conn.up = value;
      if (connecting) {
        connecting.resolve(value);
        connecting = null;
      }
      if (reconnected) for (const conn of connections) killPort(conn, true);
    },
    beginConnect: () => {
      connecting = { resolve: () => {} };
    },
    markConnectFailed: () => {
      connectFailed = true;
    },
    setDisabled: (value) => {
      disabled = value;
    },
    setDown: (reason) => {
      down = reason;
    },
    rejectHeld: (message = "pi-famulus connection lost") => {
      const pending = waiters.splice(0);
      for (const waiter of pending) waiter.fail(new IntercomError(message, "E_TRANSPORT", "unavailable"));
    },
    isUp: () => up,
    debugConnections: () => connections.map((c) => ({ id: c.id, up: c.up, bound: c.bound, handlers: c.frameHandlers.size })),
  };
  return manager;
}

/** The fake pi API the plugin tests built, with per-test hooks. */
export interface FakePi {
  handlers: Map<string, (event: any, ctx: any) => any>;
  tools: Map<string, any>;
  commands: Map<string, any>;
  messageRenderers: Map<string, (message: any, options: any, theme: any) => any>;
  entryRenderers: Map<string, (entry: any, options: any, theme: any) => any>;
  injections: { message: any; options?: any }[];
  notices: { message: string; level?: string }[];
  sessionName: string;
  named: string[];
  pi: Record<string, any>;
}

export function createFakePi(): FakePi {
  const handlers = new Map<string, (event: any, ctx: any) => any>();
  const tools = new Map<string, any>();
  const commands = new Map<string, any>();
  const messageRenderers = new Map<string, any>();
  const entryRenderers = new Map<string, any>();
  const injections: { message: any; options?: any }[] = [];
  const notices: { message: string; level?: string }[] = [];
  const named: string[] = [];
  const state: FakePi = {
    handlers,
    tools,
    commands,
    messageRenderers,
    entryRenderers,
    injections,
    notices,
    sessionName: "",
    named,
    pi: {},
  };
  state.pi = {
    on: (name: string, handler: any) => handlers.set(name, handler),
    registerTool: (tool: any) => tools.set(tool.name, tool),
    registerCommand: (name: string, options: any) => commands.set(name, { name, ...options }),
    registerMessageRenderer: (type: string, renderer: any) => messageRenderers.set(type, renderer),
    registerEntryRenderer: (type: string, renderer: any) => entryRenderers.set(type, renderer),
    getSessionName: () => state.sessionName,
    setSessionName: (name: string) => {
      state.sessionName = name;
      named.push(name);
    },
    sendMessage: (message: any, options?: any) => injections.push({ message, options }),
  };
  return state;
}

/** The fake extension context: mutable activity like the plugin harness. */
export interface FakeCtxState {
  sessionId: string;
  idle: boolean;
  pending: boolean;
  stale: boolean;
}

export function createFakeCtx(dir: string, sessionId = "target-session"): { ctx: any; state: FakeCtxState } {
  const state: FakeCtxState = { sessionId, idle: true, pending: false, stale: false };
  const assertLive = () => {
    if (state.stale) throw new Error("This extension ctx is stale after session replacement or reload");
  };
  const ctx = {
    cwd: dir,
    hasUI: false,
    mode: "print",
    get sessionManager() {
      assertLive();
      return { getSessionId: () => state.sessionId, getEntries: () => [] };
    },
    isIdle() {
      assertLive();
      return state.idle;
    },
    hasPendingMessages() {
      assertLive();
      return state.pending;
    },
    ui: { notify: () => {} },
  };
  return { ctx, state };
}

export function makeTempDir(prefix: string): { dir: string; cleanup: () => void } {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  return { dir, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

export const flush = () => new Promise((resolve) => setImmediate(resolve));
