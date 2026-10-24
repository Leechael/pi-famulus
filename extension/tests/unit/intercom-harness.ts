/**
 * Test harness: a fake intercom transport modelling the manager-side bridge
 * (and a fake pi API), so the extension module can be driven without a
 * manager process. Ports the socket harness of agent-intercom @ 98183fb,
 * plugin/pi index.test.mjs / lifecycle.test.mjs.
 */

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { IntercomError, type IntercomTransport } from "../../src/intercom/transport";

export interface FakeTransport {
  transport: IntercomTransport;
  /** Every op the extension issued, in order. */
  requests: Record<string, any>[];
  /** Every session record sent with op register. */
  registrations: Record<string, any>[];
  /** Ops whose response is held back until released (by name). */
  held: Set<string>;
  /** Held requests, with manual responders. */
  waiters: {
    req: Record<string, any>;
    respond: (extra?: Record<string, unknown>) => void;
    fail: (error: Error) => void;
  }[];
  /** Push one bridge frame (deliver/receipt/idle_notice) to the extension. */
  push: (frame: Record<string, unknown>) => void;
  /** The bridge restarted: intercom_reset semantics. */
  resetBridge: () => void;
  /** The manager connection (re)opened or dropped. */
  setUp: (up: boolean) => void;
  /** Bridge disabled (PI_FAMULUS_NO_INTERCOM). */
  setDisabled: (disabled: boolean) => void;
  /** Bridge down with a reason (startup failure, Windows, …). */
  setDown: (reason: string | null) => void;
  /** Fail every held op (what ManagerClient.close does to pending ops). */
  rejectHeld: (message?: string) => void;
  /** Per-op response overrides, like the plugin harness's `reply`. */
  reply?: (req: Record<string, any>) => Record<string, unknown> | undefined;
  isUp: () => boolean;
}

export function createFakeTransport(options: { reply?: (req: Record<string, any>) => Record<string, unknown> | undefined } = {}): FakeTransport {
  const requests: Record<string, any>[] = [];
  const registrations: Record<string, any>[] = [];
  const frameHandlers = new Set<(frame: Record<string, unknown>) => void>();
  const resetHandlers = new Set<() => void>();
  const held = new Set<string>();
  const waiters: FakeTransport["waiters"] = [];
  let up = true;
  let disabled = false;
  let down: string | null = null;
  let seq = 0;

  const defaultReply = (req: Record<string, any>): Record<string, unknown> => {
    switch (req.op) {
      case "register":
        registrations.push(req.session);
        return {
          session_id: typeof req.session?.id === "string" && req.session.id.startsWith("pi/")
            ? req.session.id
            : `pi/${req.session?.id}`,
        };
      case "list":
        return { sessions: [] };
      case "send":
        return { message_id: `m-${++seq}`, status: "delivered" };
      case "receipt":
      case "unregister":
        return {};
      case "subscribe_idle":
        return { subscription_id: `sub-${++seq}`, status: "subscribed" };
      default:
        return {};
    }
  };

  const transport: IntercomTransport = {
    async call(op) {
      const req = { ...op } as Record<string, any>;
      requests.push(req);
      if (!up) throw new IntercomError("pi-famulus is not connected", "E_TRANSPORT", "unavailable");
      if (disabled) throw new IntercomError("disabled by PI_FAMULUS_NO_INTERCOM", "E_INTERCOM_DISABLED", "disabled");
      if (down) throw new IntercomError(down, "E_INTERCOM_UNAVAILABLE", "unavailable");
      if (held.has(req.op)) {
        return await new Promise<Record<string, unknown>>((resolve, reject) => {
          let settled = false;
          waiters.push({
            req,
            respond: (extra) => {
              if (settled) return;
              settled = true;
              resolve({ ...defaultReply(req), ...extra });
            },
            fail: (error) => {
              if (settled) return;
              settled = true;
              reject(error);
            },
          });
        });
      }
      // Per-request overrides merge like the plugin harness's Object.assign;
      // an `error` key models the daemon's op-level failure frame.
      return { ...defaultReply(req), ...options.reply?.(req) };
    },
    onFrame(handler) {
      frameHandlers.add(handler);
      return () => frameHandlers.delete(handler);
    },
    onReset(handler) {
      resetHandlers.add(handler);
      return () => resetHandlers.delete(handler);
    },
    isUp: () => up,
  };

  return {
    transport,
    requests,
    registrations,
    held,
    waiters,
    push: (frame) => {
      for (const handler of [...frameHandlers]) handler(frame);
    },
    resetBridge: () => {
      for (const handler of [...resetHandlers]) handler();
    },
    setUp: (value) => {
      // Coming back up models the ManagerClient reconnect: it re-hellos and
      // fires its reconnect handlers, which invalidate the registration.
      const reconnected = value && !up;
      up = value;
      if (reconnected) {
        for (const handler of [...resetHandlers]) handler();
      }
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
  };
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
