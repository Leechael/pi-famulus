/**
 * A ManagerClient stand-in that speaks the hosted-subagent protocol (level 6).
 * Tests mock `../../src/manager-client` with it, so the extension runs its
 * production child path: createHostedSessionFn and HostedRpcSession. Child
 * activity reaches the extension the way the daemon sends it, as
 * `agent_state` / `agent_rpc` / `agent_ui` / `agent_comms` events.
 *
 * State lives on globalThis: tests that call vi.resetModules() load a new
 * copy of this module through the mock, and both copies must share it.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export interface SpawnCall {
  childId: string;
  runId: string;
  name: string;
  argv: string[];
  cwd: string;
  env: Record<string, string>;
}

export interface InputCall {
  childId: string;
  op: string;
  text: string;
  source: string;
}

export interface UiResponseCall {
  childId: string;
  rpcId: string;
  value: Record<string, unknown>;
  cancelled: boolean;
}

interface HostedFakeState {
  instances: FakeHostedManager[];
  /** When set, connect() resolves only after the promise does. */
  connectGate: Promise<void> | null;
  /** What list() returns. */
  tasks: Array<Record<string, unknown>>;
  /** When false, tools degrade to the local bash fallback. */
  available: boolean;
}

const KEY = Symbol.for("pi-famulus.test.hosted-fakes");

export function hostedFakeState(): HostedFakeState {
  const scope = globalThis as typeof globalThis & { [KEY]?: HostedFakeState };
  scope[KEY] ??= { instances: [], connectGate: null, tasks: [], available: true };
  return scope[KEY];
}

export function resetHostedFakes(): void {
  const state = hostedFakeState();
  state.instances.length = 0;
  state.connectGate = null;
  state.tasks = [];
  state.available = true;
}

export class FakeHostedManager {
  readonly spawns: SpawnCall[] = [];
  readonly inputs: InputCall[] = [];
  readonly uiResponses: UiResponseCall[] = [];
  readonly commsReplies: Array<{ requestId: string; childId: string; message: string }> = [];
  readonly stops: Array<{ taskId: string; reason?: string }> = [];
  shutdownCalls = 0;
  closeCalls = 0;
  private connected: boolean;
  private connecting: Promise<boolean> | null = null;
  private readonly handlers: Array<(event: Record<string, unknown>) => void> = [];
  private readonly reconnectHandlers: Array<() => void> = [];

  constructor(readonly opts: Record<string, unknown> = {}) {
    const state = hostedFakeState();
    // Without a gate the daemon counts as already connected.
    this.connected = state.connectGate === null;
    state.instances.push(this);
  }

  connect(): Promise<boolean> {
    this.connecting ??= (async () => {
      const gate = hostedFakeState().connectGate;
      if (gate) await gate;
      this.connected = true;
      return true;
    })();
    return this.connecting;
  }

  async ensureAvailable(): Promise<boolean> {
    if (hostedFakeState().available === false) return false;
    if (this.connected) return true;
    return this.connect();
  }

  isAvailable(): boolean {
    return this.connected && hostedFakeState().available !== false;
  }

  protocolLevel(): number {
    return this.connected ? 6 : 0;
  }

  requireHostedProtocol(): void {
    if (this.protocolLevel() < 6) throw new Error(`this pi-famulus daemon does not host subagents (protocol ${this.protocolLevel()}; need 6).`);
  }

  lastError(): string {
    return "";
  }

  useManagerPath(): void {}

  onEvent(handler: (event: Record<string, unknown>) => void): () => void {
    this.handlers.push(handler);
    return () => {
      const i = this.handlers.indexOf(handler);
      if (i >= 0) this.handlers.splice(i, 1);
    };
  }

  onReconnect(handler: () => void): void {
    this.reconnectHandlers.push(handler);
  }

  emit(event: Record<string, unknown>): void {
    for (const handler of [...this.handlers]) handler(event);
  }

  /** One JSONL record from the hosted pi, as the daemon forwards it. */
  rpc(childId: string, record: Record<string, unknown>): void {
    this.emit({ event: "agent_rpc", child_id: childId, record });
  }

  state(childId: string, activity: string, residency = "warm"): void {
    this.emit({ event: "agent_state", child_id: childId, residency, activity, driver: "parent", pid: 4242 });
  }

  /** The child's run: agent_start, one assistant message, then agent_settled. */
  finish(childId: string, message: Record<string, unknown>): void {
    this.rpc(childId, { type: "agent_start" });
    this.rpc(childId, { type: "message_end", message: { role: "assistant", ...message } });
    this.rpc(childId, { type: "agent_settled" });
  }

  async list(): Promise<Array<Record<string, unknown>>> {
    return hostedFakeState().tasks;
  }

  async start(): Promise<{ task_id: string; pid: number }> {
    return { task_id: "sh_late", pid: 1 };
  }

  async markBackground(): Promise<void> {}

  async wait(): Promise<{ done: boolean; exit_code: number | null }> {
    return { done: false, exit_code: null };
  }

  async output(): Promise<Record<string, unknown>> {
    return { chunk: "", next_cursor: 0, status: "running", exit_code: null, total_size: 0 };
  }

  async watch(): Promise<void> {}

  async stop(taskId: string, reason?: string): Promise<void> {
    this.stops.push({ taskId, reason });
  }

  async shutdownSession(): Promise<unknown[]> {
    this.shutdownCalls++;
    return [];
  }

  async close(): Promise<void> {
    this.closeCalls++;
    this.handlers.length = 0;
    this.reconnectHandlers.length = 0;
  }

  async hostedLookup(): Promise<null> {
    return null;
  }

  async acquireAgent(): Promise<{ granted: boolean }> {
    return { granted: true };
  }

  async releaseAgent(): Promise<void> {}

  async agentSpawn(input: SpawnCall): Promise<Record<string, unknown>> {
    this.requireHostedProtocol();
    this.spawns.push(input);
    return { child_id: input.childId, residency: "warm", activity: "idle", session_id: input.childId };
  }

  async agentInput(childId: string, op: string, text: string, source = "parent"): Promise<Record<string, unknown>> {
    this.requireHostedProtocol();
    this.inputs.push({ childId, op, text, source });
    return {};
  }

  async agentWatch(): Promise<void> {}

  async agentUnwatch(): Promise<void> {}

  async agentPark(): Promise<void> {}

  async agentPeers(): Promise<unknown[]> {
    return [];
  }

  async agentUiResponse(childId: string, rpcId: string, value: Record<string, unknown>, cancelled: boolean): Promise<void> {
    this.uiResponses.push({ childId, rpcId, value, cancelled });
  }

  async agentCommsReply(requestId: string, childId: string, message: string): Promise<void> {
    this.commsReplies.push({ requestId, childId, message });
  }

  async agentCache(): Promise<void> {}
}

export type Handler = (event: unknown, ctx: unknown) => unknown;

export type ExecutableTool = {
  name: string;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: undefined,
    ctx: unknown,
  ) => Promise<{ content?: Array<{ type: string; text?: string }>; details?: unknown }>;
};

export interface SentMessage {
  message: { customType?: string; content?: string; details?: unknown };
  options: { triggerTurn?: boolean; deliverAs?: string } | undefined;
}

/** The pi surface the extension registers on, recording what it sends. */
export function fakeHostPi() {
  const handlers = new Map<string, Handler[]>();
  const tools = new Map<string, ExecutableTool>();
  const sent: SentMessage[] = [];
  const api = {
    events: { emit: () => {}, on: () => () => {} },
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: (tool: ExecutableTool) => tools.set(tool.name, tool),
    registerCommand: () => {},
    registerMessageRenderer: () => {},
    registerShortcut: () => {},
    registerFlag: () => {},
    registerProvider: () => {},
    sendMessage: (message: SentMessage["message"], options?: SentMessage["options"]) => {
      sent.push({ message, options });
    },
  };
  const emit = async (name: string, event: Record<string, unknown>, ctx: unknown = {}) => {
    for (const handler of handlers.get(name) ?? []) await handler({ type: name, ...event }, ctx);
  };
  const tool = (name: string): ExecutableTool => {
    const found = tools.get(name);
    if (!found) throw new Error(`tool ${name} was not registered`);
    return found;
  };
  return { api: api as unknown as ExtensionAPI, sent, emit, tool };
}

export function fakeHostCtx(sessionId: string, cwd: string, options: { idle?: () => boolean } = {}) {
  return {
    cwd,
    hasUI: false,
    ui: { notify: () => {}, setWidget: () => {} },
    isIdle: options.idle ?? (() => true),
    sessionManager: { getSessionId: () => sessionId, getSessionFile: () => undefined },
    modelRegistry: null,
    scopedModels: [],
    isProjectTrusted: () => false,
  };
}

/**
 * Load the extension on a fake pi and start a session. The calling test file
 * must mock `../../src/manager-client` with FakeHostedManager.
 */
export async function startHostedExtension(opts: { sessionId: string; cwd: string; idle?: () => boolean; reason?: string }) {
  const pi = fakeHostPi();
  const ctx = fakeHostCtx(opts.sessionId, opts.cwd, { idle: opts.idle });
  const { default: extension } = await import("../../src/index");
  extension(pi.api);
  await pi.emit("session_start", { reason: opts.reason ?? "startup" }, ctx);
  const state = hostedFakeState();
  const manager = state.instances[state.instances.length - 1];
  const run = async (params: Record<string, unknown>) => pi.tool("subagent").execute("call", params, undefined, undefined, ctx);
  /** Start one background child and wait until the daemon was asked to prompt it. */
  const spawnChild = async (prompt = "work", name = "worker"): Promise<string> => {
    const before = manager.spawns.length;
    await run({ async: true, tasks: [{ prompt, name }] });
    await until(() => manager.spawns.length > before && manager.inputs.some((input) => input.op === "prompt" && input.childId === manager.spawns[before]?.childId), "the hosted child to be prompted");
    return manager.spawns[before].childId;
  };
  const shutdown = async () => {
    try {
      await pi.emit("session_shutdown", { reason: "quit" }, ctx);
    } catch {
      // best-effort cleanup
    }
  };
  return { pi, ctx, manager, run, spawnChild, shutdown };
}

export function wakeDetails(sent: SentMessage[], kind: string): Array<Record<string, unknown>> {
  return sent
    .map((entry) => entry.message.details as Record<string, unknown> | undefined)
    .filter((details): details is Record<string, unknown> => details?.kind === kind);
}

export function textOf(result: { content?: Array<{ type: string; text?: string }> }): string {
  return (result.content ?? []).map((part) => part.text ?? "").join("\n");
}

/** Resolve floating promises and timers that are already due. */
export async function settleAsync(rounds = 5): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

export async function until(predicate: () => boolean, label: string, timeoutMs = 2000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((resolve) => setImmediate(resolve));
  }
}
