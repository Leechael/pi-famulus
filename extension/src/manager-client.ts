/**
 * pi-famulus unix socket client (design doc §4.1, protocol §3.3).
 *
 * - Implements the §3.1 startup flow: connect -> spawn via lock -> zombie cleanup.
 * - Request/response multiplexing over a single long-lived connection.
 * - Server events dispatched to registered handlers.
 * - On unexpected disconnect: immediate then exponential-backoff reconnect for 22s,
 *   re-hello after reconnect. If all attempts fail the client is marked unavailable
 *   and callers are expected to degrade (bash falls back to local execution).
 */
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import net from "node:net";
import { famulusPaths } from "./config";
import { realClock, type Clock, type ClockTimer } from "./clock";

const MAX_FRAME_BYTES = 4 * 1024 * 1024; // 4 MiB (§3.3)
const EXTENSION_VERSION = "0.1.2";
// 4: speaks machine-wide agent admission, as well as in-place upgrade.
const PROTOCOL = 4;
const HELLO_TIMEOUT_MS = 5000;
const RECONNECT_HELLO_TIMEOUT_MS = 25_000;
const DEFAULT_REQUEST_TIMEOUT_MS = 30000;
const SOCKET_READY_TIMEOUT_MS = 2000;
const SOCKET_READY_POLL_MS = 50;
const RECONNECT_WINDOW_MS = 27_000;
const RETRY_COOLDOWN_MS = 30000;
const MANAGER_SHUTDOWN_WAIT_MS = 5000;
const MANAGER_SHUTTING_DOWN = "manager is shutting down";

// ---------------------------------------------------------------------------
// Protocol types (field names are contractual, see design doc §3.3)
// ---------------------------------------------------------------------------

export type TaskOrigin =
  | { via: "bash-fg" | "bash-bg" | "monitor" }
  | { via: "child-bash"; child_id: string; run_id: string };

export type StopReason = "tui" | "cli" | "tool" | "timeout" | "rate-limit" | "session-end";

export interface StartRequest {
  kind: "shell" | "monitor";
  command: string;
  cwd: string;
  env: Record<string, string>;
  run_in_background?: boolean;
  timeout_ms?: number | null;
  origin?: TaskOrigin;
}

export interface StartResponse {
  task_id: string;
  pid: number;
}

export interface WaitResponse {
  done: boolean;
  exit_code?: number | null;
}

export interface OutputResponse {
  chunk: string;
  next_cursor: number;
  status: string;
  exit_code: number | null;
  total_size: number;
}

export interface TaskRecord {
  task_id: string;
  session_id: string;
  kind: string;
  command: string;
  cwd: string;
  pid: number;
  status: string;
  exit_code: number | null;
  signal: string | null;
  started_at: number;
  ended_at: number | null;
  output_path: string;
  output_size: number;
  origin?: TaskOrigin;
  backgrounded_at?: number;
  end_reason?: string;
}

/** Server-pushed event (§3.3). Fields beyond `event` depend on the event kind. */
export interface ManagerEvent {
  event: "task_started" | "output" | "task_exited" | "session_rebound" | string;
  task_id?: string;
  kind?: string;
  command?: string;
  pid?: number;
  chunk?: string;
  next_cursor?: number;
  exit_code?: number | null;
  signal?: string | null;
  duration_ms?: number;
  output_path?: string;
  output_size?: number;
  end_reason?: string;
  ts?: number;
}

export class ManagerError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "ManagerError";
    this.code = code;
  }
}

export interface ManagerClientOptions {
  home: string;
  sessionId: string;
  managerPath: string | null;
  piPid?: number;
  /** Session working directory, sent on hello (optional; older managers ignore it). */
  cwd?: string;
  log?: (message: string) => void;
  clock?: Clock;
}

export interface LastUpgrade {
  at: number;
  ok: boolean;
  from_version: string;
  to_version?: string;
  error?: string;
  trigger: string;
}

export interface ManagerStatusResponse {
  sessions: SessionInfo[];
  generation?: number;
  last_upgrade?: LastUpgrade | null;
}

export interface SessionInfo {
  session_id: string;
  pi_pid: number;
  connected: boolean;
  cwd?: string;
  extension_version?: string;
  protocol?: number;
}

type ClientState = "disconnected" | "connected" | "unavailable";

type EventHandler = (event: ManagerEvent) => void;

interface PendingRequest {
  resolve: (value: Record<string, unknown>) => void;
  reject: (err: Error) => void;
  timer: ClockTimer;
  message: Record<string, unknown>;
  timeoutMs: number;
  retryable: boolean;
}

/** Reassembles `u32 BE length + JSON` frames from a byte stream. */
class FrameDecoder {
  private buf: Buffer = Buffer.alloc(0);

  push(data: Buffer): Record<string, unknown>[] {
    this.buf = this.buf.length === 0 ? data : Buffer.concat([this.buf, data]);
    const messages: Record<string, unknown>[] = [];
    while (this.buf.length >= 4) {
      const len = this.buf.readUInt32BE(0);
      if (len > MAX_FRAME_BYTES) {
        throw new Error(`pi-famulus frame too large: ${len} bytes`);
      }
      if (this.buf.length < 4 + len) break;
      const payload = this.buf.subarray(4, 4 + len).toString("utf8");
      this.buf = this.buf.subarray(4 + len);
      messages.push(JSON.parse(payload) as Record<string, unknown>);
    }
    return messages;
  }

  reset(): void {
    this.buf = Buffer.alloc(0);
  }
}

function encodeFrame(message: Record<string, unknown>): Buffer {
  const payload = Buffer.from(JSON.stringify(message), "utf8");
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length, 0);
  return Buffer.concat([header, payload]);
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM means the process exists but we cannot signal it.
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * O_EXCL pid-file lock used by the extension while spawning the daemon.
 * The Rust CLI uses an fd-lock on the same path and leaves an empty file after
 * release — treat empty / non-pid / dead-pid contents as stale and break them.
 * Exported for unit tests.
 */
export function tryAcquireSpawnLockFile(lockPath: string, pid: number = process.pid): boolean {
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      writeFileSync(lockPath, String(pid), { flag: "wx" });
      return true;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code !== "EEXIST") return false;
      let stale = false;
      try {
        const raw = readFileSync(lockPath, "utf8").trim();
        const holderPid = Number.parseInt(raw, 10);
        // Empty (Rust leftover), unparseable, or dead holder → reclaim.
        stale = raw === "" || !Number.isFinite(holderPid) || !pidAlive(holderPid);
      } catch {
        stale = true;
      }
      if (!stale) return false;
      try {
        unlinkSync(lockPath);
      } catch {
        return false;
      }
    }
  }
  return false;
}

export function releaseSpawnLockFile(lockPath: string): void {
  try {
    unlinkSync(lockPath);
  } catch {
    // already gone
  }
}

function delay(clock: Clock, ms: number): Promise<void> {
  // These awaited timers intentionally remain ref'd; an unref'd handshake
  // timer can let print-mode pi exit before the manager connection completes.
  return clock.sleep(ms);
}

export class ManagerClient {
  private readonly home: string;
  private readonly sessionId: string;
  private readonly managerPath: string | null;
  private readonly piPid: number;
  private readonly cwd: string | undefined;
  private readonly log: (message: string) => void;
  private readonly clock: Clock;

  private socket: net.Socket | null = null;
  private decoder = new FrameDecoder();
  private pending = new Map<string, PendingRequest>();
  private queuedRequests = new Set<PendingRequest>();
  private helloWaiter: { resolve: () => void; reject: (err: Error) => void } | null = null;
  private eventHandlers = new Set<EventHandler>();
  private reconnectHandlers = new Set<() => void>();
  private protocolLevel_ = 0;
  private state: ClientState = "disconnected";
  private intentionalClose = false;
  private rebound = false;
  private reconnecting: Promise<void> | null = null;
  /** One in-flight connect shared by session_start and ensureAvailable. */
  private connecting: Promise<boolean> | null = null;
  private lastFailureAt = 0;
  private lastFailureMessage = "";

  constructor(options: ManagerClientOptions) {
    this.home = options.home;
    this.sessionId = options.sessionId;
    this.managerPath = options.managerPath;
    this.piPid = options.piPid ?? process.pid;
    this.cwd = options.cwd;
    this.log = options.log ?? (() => {});
    this.clock = options.clock ?? realClock;
  }

  private now(): number {
    return this.clock.now();
  }

  isAvailable(): boolean {
    return this.state === "connected";
  }

  /** Maximum protocol level reported by the connected manager; 0 means unknown/legacy. */
  protocolLevel(): number {
    return this.protocolLevel_;
  }

  /** Last connect/reconnect failure reason (empty when never failed / currently connected). */
  lastError(): string {
    return this.lastFailureMessage;
  }

  /**
   * Ensure a live connection, used by tools before issuing requests.
   * When previously marked unavailable, a single retry is allowed after a
   * cooldown so a recovered manager is picked up without hot-looping.
   */
  async ensureAvailable(): Promise<boolean> {
    if (this.state === "connected") return true;
    if (this.connecting) return this.connecting;
    if (this.reconnecting) {
      await this.reconnecting;
      return this.isAvailable();
    }
    if (this.state === "unavailable" && this.now() - this.lastFailureAt < RETRY_COOLDOWN_MS) {
      return false;
    }
    return this.connect();
  }

  onEvent(handler: EventHandler): () => void {
    this.eventHandlers.add(handler);
    return () => this.eventHandlers.delete(handler);
  }

  /** Fired after a successful reconnect (post re-hello); used to re-subscribe watches. */
  onReconnect(handler: () => void): () => void {
    this.reconnectHandlers.add(handler);
    return () => this.reconnectHandlers.delete(handler);
  }

  /**
   * Connect to the manager, spawning it if necessary (§3.1 flow).
   * Returns true when connected and hello completed.
   */
  async connect(): Promise<boolean> {
    if (this.state === "connected") return true;
    if (this.connecting) return this.connecting;
    this.intentionalClose = false;
    let run!: Promise<boolean>;
    run = (async (): Promise<boolean> => {
      try {
        await this.connectFlow(true);
        this.state = "connected";
        return true;
      } catch (err) {
        this.log(`connect failed: ${(err as Error).message}`);
        this.state = "unavailable";
        this.lastFailureAt = this.now();
        this.lastFailureMessage = (err as Error).message;
        return false;
      } finally {
        if (this.connecting === run) this.connecting = null;
      }
    })();
    this.connecting = run;
    return run;
  }

  /** Graceful session shutdown: stop all tasks of this session, then disconnect. */
  async shutdownSession(): Promise<string[]> {
    const res = await this.request({ type: "shutdown_session" });
    return (res.stopped as string[] | undefined) ?? [];
  }

  async start(req: StartRequest): Promise<StartResponse> {
    const res = await this.request({ type: "start", ...req, key: randomUUID() });
    return { task_id: res.task_id as string, pid: res.pid as number };
  }

  async wait(taskId: string, budgetMs: number): Promise<WaitResponse> {
    const res = await this.request(
      { type: "wait", task_id: taskId, budget_ms: budgetMs },
      budgetMs + 15000,
    );
    return { done: res.done === true, exit_code: (res.exit_code as number | null) ?? null };
  }

  async output(taskId: string, cursor: number, maxBytes: number): Promise<OutputResponse> {
    const res = await this.request({
      type: "output",
      task_id: taskId,
      cursor,
      max_bytes: maxBytes,
    });
    return {
      chunk: (res.chunk as string) ?? "",
      next_cursor: (res.next_cursor as number) ?? cursor,
      status: (res.status as string) ?? "unknown",
      exit_code: (res.exit_code as number | null) ?? null,
      total_size: (res.total_size as number) ?? 0,
    };
  }

  async markBackground(taskId: string): Promise<void> {
    await this.request({ type: "mark_background", task_id: taskId });
  }

  async stop(taskId: string, reason: StopReason = "tool"): Promise<void> {
    await this.request({ type: "stop", task_id: taskId, reason });
  }

  async list(all = false): Promise<TaskRecord[]> {
    const res = await this.request({ type: "list", all });
    return ((res.tasks as TaskRecord[] | undefined) ?? []) as TaskRecord[];
  }

  async acquireAgent(childId: string): Promise<{ granted: boolean; rejection?: string }> {
    const res = await this.request({ type: "acquire_agent", child_id: childId });
    return { granted: res.granted === true, ...(typeof res.rejection === "string" ? { rejection: res.rejection } : {}) };
  }

  async releaseAgent(childId: string): Promise<void> {
    await this.request({ type: "release_agent", child_id: childId });
  }

  /** Daemon status, including optional in-place upgrade metadata. */
  async status(): Promise<ManagerStatusResponse> {
    const res = await this.request({ type: "status" });
    return {
      sessions: (res.sessions as SessionInfo[] | undefined) ?? [],
      ...(typeof res.generation === "number" ? { generation: res.generation } : {}),
      ...(res.last_upgrade && typeof res.last_upgrade === "object"
        ? { last_upgrade: res.last_upgrade as LastUpgrade }
        : {}),
    };
  }

  /** Connected sessions (status). Older managers may reject this for extension clients. */
  async sessions(): Promise<SessionInfo[]> {
    return (await this.status()).sessions;
  }

  async watch(taskId: string): Promise<void> {
    await this.request({ type: "watch", task_id: taskId });
  }

  async unwatch(taskId: string): Promise<void> {
    await this.request({ type: "unwatch", task_id: taskId });
  }

  /** Close the connection without reconnecting. */
  async close(): Promise<void> {
    this.intentionalClose = true;
    this.state = "disconnected";
    this.failAllPending(new Error("manager client closed"));
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.removeAllListeners();
      socket.end();
      socket.destroy();
    }
  }

  // -------------------------------------------------------------------------
  // Startup flow (§3.1)
  // -------------------------------------------------------------------------

  private async connectFlow(allowZombieRetry: boolean, helloTimeoutMs = HELLO_TIMEOUT_MS): Promise<void> {
    const paths = famulusPaths(this.home);
    try {
      await this.connectAndHello(paths.socket, helloTimeoutMs);
      return;
    } catch (err) {
      if (err instanceof HelloError) {
        if (err.message.includes(MANAGER_SHUTTING_DOWN)) {
          // A shutdown manager refuses hello while it completes its bounded
          // kill grace. Wait for its pid to exit before retrying/spawning.
          await this.waitForManagerExit(paths.pidFile);
          if (allowZombieRetry) {
            await this.connectFlow(false, helloTimeoutMs);
            return;
          }
          throw err;
        }
        // Socket exists but hello failed: possible zombie socket (§3.1 step 5).
        this.handleZombie(paths.socket, paths.pidFile);
        if (allowZombieRetry) {
          await this.connectFlow(false, helloTimeoutMs);
          return;
        }
        throw err;
      }
      // Connect failed: nobody listening. Try to spawn the manager.
    }

    // The home directory may not exist yet on a fresh machine.
    mkdirSync(this.home, { recursive: true });
    const acquired = this.tryAcquireSpawnLock(paths.spawnLock);
    if (acquired) {
      try {
        this.spawnManager();
        await this.waitForSocket(paths.socket, SOCKET_READY_TIMEOUT_MS);
      } finally {
        this.releaseSpawnLock(paths.spawnLock);
      }
    } else {
      // Someone else is spawning; just wait for the socket to appear.
      await this.waitForSocket(paths.socket, SOCKET_READY_TIMEOUT_MS);
    }
    await this.connectAndHello(paths.socket, helloTimeoutMs);
  }

  private async waitForManagerExit(pidFile: string): Promise<void> {
    let pid: number;
    try {
      const info = JSON.parse(readFileSync(pidFile, "utf8")) as { pid?: number };
      if (typeof info.pid !== "number") return;
      pid = info.pid;
    } catch {
      return;
    }
    const deadline = this.now() + MANAGER_SHUTDOWN_WAIT_MS;
    while (pidAlive(pid) && this.now() < deadline) {
      await this.clock.sleep(SOCKET_READY_POLL_MS);
    }
  }

  private handleZombie(socketPath: string, pidFile: string): void {
    let pid: number | null = null;
    try {
      const info = JSON.parse(readFileSync(pidFile, "utf8")) as { pid?: number };
      if (typeof info.pid === "number") pid = info.pid;
    } catch {
      // no readable pid file
    }
    if (pid !== null && pidAlive(pid)) {
      // Manager is alive but rejected hello; nothing to clean up.
      return;
    }
    for (const file of [socketPath, pidFile]) {
      try {
        unlinkSync(file);
      } catch {
        // already gone
      }
    }
  }

  /**
   * Exclusive create of manager.spawn.lock with our pid as contents.
   * Compatible with the Rust CLI's fd-lock on the same path: that lock leaves
   * an empty file behind after release, which must not look like a live hold.
   * Returns false only when another live holder (numeric pid still alive) owns it.
   */
  private tryAcquireSpawnLock(lockPath: string): boolean {
    return tryAcquireSpawnLockFile(lockPath, process.pid);
  }

  private releaseSpawnLock(lockPath: string): void {
    releaseSpawnLockFile(lockPath);
  }

  private spawnManager(): void {
    if (!this.managerPath) {
      throw new Error("pi-famulus binary not found (set managerPath in config.json or PI_FAMULUS_MANAGER_PATH)");
    }
    // Pass --home explicitly: relying on PI_FAMULUS_HOME env inheritance breaks when
    // this.home came from an explicit override rather than the environment.
    const child = spawn(this.managerPath, ["--home", this.home, "daemon"], {
      detached: true,
      stdio: "ignore",
      env: process.env,
    });
    child.unref();
    child.on("error", () => {}); // surfaced via waitForSocket timeout
  }

  private async waitForSocket(socketPath: string, timeoutMs: number): Promise<void> {
    const deadline = this.now() + timeoutMs;
    for (;;) {
      if (existsSync(socketPath)) {
        const ok = await new Promise<boolean>((resolve) => {
          const probe = net.connect(socketPath);
          probe.once("connect", () => {
            probe.destroy();
            resolve(true);
          });
          probe.once("error", () => resolve(false));
        });
        if (ok) return;
      }
      if (this.now() >= deadline) {
        throw new Error("timed out waiting for pi-famulus socket");
      }
      await delay(this.clock, SOCKET_READY_POLL_MS);
    }
  }

  // -------------------------------------------------------------------------
  // Connection / protocol internals
  // -------------------------------------------------------------------------

  private async connectAndHello(socketPath: string, helloTimeoutMs = HELLO_TIMEOUT_MS): Promise<void> {
    const socket = await new Promise<net.Socket>((resolve, reject) => {
      const s = net.connect(socketPath);
      s.once("connect", () => resolve(s));
      s.once("error", (err) => reject(err));
    });
    this.attachSocket(socket);
    try {
      await this.hello(helloTimeoutMs);
    } catch (err) {
      this.detachSocket();
      throw new HelloError((err as Error).message);
    }
  }

  private attachSocket(socket: net.Socket): void {
    const previous = this.socket;
    if (previous && previous !== socket) {
      // Drop the old socket's handlers before it can close and tear down the new one.
      previous.removeAllListeners();
      previous.destroy();
    }
    this.socket = socket;
    this.decoder.reset();
    socket.on("data", (data) => {
      if (this.socket !== socket) return;
      this.onData(data);
    });
    socket.on("close", () => {
      if (this.socket !== socket) return;
      this.onClose();
    });
    socket.on("error", (err) => {
      if (this.socket !== socket) return;
      this.log(`socket error: ${err.message}`);
    });
  }

  private detachSocket(): void {
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      socket.removeAllListeners();
      socket.destroy();
    }
  }

  private hello(timeoutMs = HELLO_TIMEOUT_MS): Promise<void> {
    const socket = this.socket;
    if (!socket) return Promise.reject(new Error("no socket"));
    // The id is included so a manager that echoes request ids resolves via the
    // pending map; otherwise the first unnamed response settles the waiter.
    const id = randomUUID();
    return new Promise<void>((resolve, reject) => {
      const done = (err?: Error) => {
        this.clock.clearTimeout(timer);
        this.pending.delete(id);
        this.helloWaiter = null;
        if (err) reject(err);
        else resolve();
      };
      const timer = this.clock.setTimeout(() => done(new Error("hello timed out")), timeoutMs);
      this.helloWaiter = { resolve: () => done(), reject: (err) => done(err) };
      this.pending.set(id, {
        resolve: () => done(), reject: (err) => done(err), timer,
        message: { type: "hello" }, timeoutMs, retryable: false,
      });
      socket.write(
        encodeFrame({
          v: 1,
          id,
          type: "hello",
          client_kind: "extension",
          session_id: this.sessionId,
          pi_pid: this.piPid,
          ...(this.cwd ? { cwd: this.cwd } : {}),
          extension_version: EXTENSION_VERSION,
          protocol: PROTOCOL,
        }),
      );
    });
  }

  private onData(data: Buffer): void {
    let messages: Record<string, unknown>[];
    try {
      messages = this.decoder.push(data);
    } catch (err) {
      this.log(`protocol error: ${(err as Error).message}`);
      this.detachSocket();
      this.onClose();
      return;
    }
    for (const msg of messages) {
      this.dispatch(msg);
    }
  }

  private dispatch(msg: Record<string, unknown>): void {
    if (msg.type === "event") {
      const event = msg as unknown as ManagerEvent;
      if (event.event === "session_rebound") {
        // Only the socket that is still current lost the session. A stale
        // hello's rebound must not disable reconnect on the winning socket.
        this.rebound = true;
      }
      for (const handler of this.eventHandlers) {
        try {
          handler(event);
        } catch (err) {
          this.log(`event handler error: ${(err as Error).message}`);
        }
      }
      return;
    }
    const id = msg.id as string | undefined;
    if (id && this.pending.has(id)) {
      this.settle(id, msg);
      return;
    }
    if (this.helloWaiter && typeof msg.ok === "boolean") {
      // Unnamed response: correlate by order with the outstanding hello.
      if (msg.ok === true) this.helloWaiter.resolve();
      else this.helloWaiter.reject(errorFromResponse(msg));
      this.helloWaiter = null;
      return;
    }
    this.log(`unmatched message: ${JSON.stringify(msg).slice(0, 200)}`);
  }

  private settle(id: string, msg: Record<string, unknown>): void {
    const entry = this.pending.get(id);
    if (!entry) return;
    this.pending.delete(id);
    this.clock.clearTimeout(entry.timer);
    if (msg.ok === true) {
      if (entry.message.type === "hello") {
        this.protocolLevel_ = typeof msg.protocol === "number" ? msg.protocol : 0;
      }
      entry.resolve(msg);
    } else {
      entry.reject(errorFromResponse(msg));
    }
  }

  private request(msg: Record<string, unknown>, timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS): Promise<Record<string, unknown>> {
    const type = String(msg.type);
    const retryable = ["wait", "output", "list", "watch", "status", "stop", "mark_background", "start", "acquire_agent", "release_agent"].includes(type);
    const effectiveTimeoutMs = retryable ? Math.max(timeoutMs, RECONNECT_WINDOW_MS + 1000) : timeoutMs;
    return new Promise((resolve, reject) => {
      const entry: PendingRequest = {
        resolve, reject, message: msg, timeoutMs: effectiveTimeoutMs, retryable,
        timer: this.clock.setTimeout(() => {
          for (const [id, pending] of this.pending) {
            if (pending === entry) this.pending.delete(id);
          }
          this.queuedRequests.delete(entry);
          reject(new Error(`pi-famulus request timed out: ${type}`));
        }, effectiveTimeoutMs),
      };
      if (this.socket && this.state === "connected") this.sendPending(entry);
      else if (retryable && (this.reconnecting || this.connecting)) this.queuedRequests.add(entry);
      else {
        this.clock.clearTimeout(entry.timer);
        reject(new Error("pi-famulus not connected"));
      }
    });
  }

  private sendPending(entry: PendingRequest): void {
    const socket = this.socket;
    if (!socket || this.state !== "connected") return;
    const id = randomUUID();
    this.pending.set(id, entry);
    socket.write(encodeFrame({ v: 1, id, ...entry.message }));
  }

  private resendPending(): void {
    const entries = [...new Set([...this.pending.values(), ...this.queuedRequests])].filter((entry) => entry.retryable);
    this.pending.clear();
    this.queuedRequests.clear();
    for (const entry of entries) this.sendPending(entry);
  }

  private onClose(): void {
    const wasConnected = this.state === "connected";
    this.detachSocket();
    for (const [id, entry] of [...this.pending]) {
      if (entry.retryable && !this.intentionalClose && !this.rebound) continue;
      this.pending.delete(id);
      this.clock.clearTimeout(entry.timer);
      entry.reject(new Error("pi-famulus connection lost"));
    }
    if (this.helloWaiter) {
      this.helloWaiter.reject(new Error("connection closed during hello"));
      this.helloWaiter = null;
    }
    if (this.intentionalClose || this.rebound) {
      this.state = "disconnected";
      return;
    }
    this.state = "disconnected";
    if (wasConnected) {
      let run!: Promise<void>;
      run = this.reconnectLoop().finally(() => {
        if (this.reconnecting === run) this.reconnecting = null;
      });
      this.reconnecting = run;
    }
  }

  private async reconnectLoop(): Promise<void> {
    const deadline = this.now() + RECONNECT_WINDOW_MS;
    let nextDelayMs = 0;
    while (this.now() < deadline) {
      if (nextDelayMs > 0) await delay(this.clock, Math.min(nextDelayMs, deadline - this.now()));
      if (this.intentionalClose || this.rebound) return;
      try {
        await this.connectFlow(true, RECONNECT_HELLO_TIMEOUT_MS);
        this.state = "connected";
        this.resendPending();
        this.log("reconnected to pi-famulus");
        for (const handler of this.reconnectHandlers) {
          try {
            handler();
          } catch (err) {
            this.log(`reconnect handler error: ${(err as Error).message}`);
          }
        }
        return;
      } catch (err) {
        this.log(`reconnect attempt failed: ${(err as Error).message}`);
        nextDelayMs = nextDelayMs === 0 ? 100 : Math.min(nextDelayMs * 2, 2000);
      }
    }
    this.state = "unavailable";
    this.lastFailureAt = this.now();
    this.lastFailureMessage = "reconnect exhausted";
    this.failAllPending(new Error("pi-famulus reconnect exhausted"));
    this.log("giving up on pi-famulus; bash falls back to local execution");
  }

  private failAllPending(err: Error): void {
    const entries = new Set([...this.pending.values(), ...this.queuedRequests]);
    for (const entry of entries) {
      this.clock.clearTimeout(entry.timer);
      entry.reject(err);
    }
    this.pending.clear();
    this.queuedRequests.clear();
  }
}

class HelloError extends Error {}

function errorFromResponse(msg: Record<string, unknown>): ManagerError {
  const err = msg.error as { code?: string; message?: string } | undefined;
  return new ManagerError(err?.code ?? "E_INTERNAL", err?.message ?? "unknown manager error");
}
