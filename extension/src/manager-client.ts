/**
 * pi-famulus unix socket client (design doc §4.1, protocol §3.3).
 *
 * - Implements §3.1 startup: connect -> spawn via lock; only the daemon's
 *   lifetime-lock holder cleans up stale socket/pid files.
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
import { StringDecoder } from "node:string_decoder";
import { famulusPaths } from "./config";
import { realClock, type Clock, type ClockTimer } from "./clock";

const MAX_FRAME_BYTES = 4 * 1024 * 1024; // 4 MiB (§3.3)
const EXTENSION_VERSION = "0.1.2";
// 5: speaks per-kind agent admission and queued acquire.
const PROTOCOL = 5;
const HELLO_TIMEOUT_MS = 5000;
const RECONNECT_HELLO_TIMEOUT_MS = 25_000;
const DEFAULT_REQUEST_TIMEOUT_MS = 30000;
// One bounded startup budget: keep waiting for a live child, but only
// respawn when it explicitly reports losing the daemon's lifetime lock.
const STARTUP_WINDOW_MS = 10_000;
const SOCKET_READY_POLL_MS = 50;
const RECONNECT_WINDOW_MS = 27_000;
const RETRY_COOLDOWN_MS = 30000;
const SHUTDOWN_PROBE_TIMEOUT_MS = 2000;
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

interface SpawnAttempt {
  failure?: Error;
  contended: boolean;
  output: string;
  dispose(): void;
}

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
        await this.connectFlow();
        this.checkStartupOpen();
        this.state = "connected";
        this.lastFailureMessage = "";
        return true;
      } catch (err) {
        this.state = this.intentionalClose ? "disconnected" : "unavailable";
        this.lastFailureAt = this.now();
        this.lastFailureMessage = this.startupFailureDetails((err as Error).message);
        this.log(`connect failed: ${this.lastFailureMessage}`);
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

  private checkStartupOpen(): void {
    if (this.intentionalClose) throw new Error("manager client closed");
  }

  private async connectFlow(helloTimeoutMs = HELLO_TIMEOUT_MS, outerDeadline = Infinity): Promise<void> {
    const paths = famulusPaths(this.home);
    const deadline = Math.min(outerDeadline, this.now() + Math.max(STARTUP_WINDOW_MS, helloTimeoutMs));
    let shutdownObserved = false;
    let retryDelayMs = 100;
    let lastReason = "";
    for (;;) {
      this.checkStartupOpen();
      if (this.now() >= deadline) throw new Error(`pi-famulus startup deadline exhausted: ${lastReason}`);
      const probeMs = shutdownObserved ? SHUTDOWN_PROBE_TIMEOUT_MS : helloTimeoutMs;
      try {
        await this.connectAndHello(paths.socket, Math.min(probeMs, deadline - this.now()));
        return;
      } catch (err) {
        lastReason = (err as Error).message;
        if (!(err instanceof HelloError)) {
          // Nobody listening: the daemon's lifetime lock safely arbitrates
          // any spawn, including a retiring owner whose files are gone.
          await this.ensureSocketReady(paths.socket, paths.spawnLock, deadline);
          continue;
        }
        if (err.message === `E_INTERNAL: ${MANAGER_SHUTTING_DOWN}`) shutdownObserved = true;
        else if (!shutdownObserved || !err.transportFailure) throw err;
        // Only a known shutdown permits retrying a stalled/lost handshake.
        // Re-probe instead of trusting pid liveness: that pid may be stale,
        // and another client may already have started a healthy successor.
      }
      await delay(this.clock, Math.min(retryDelayMs, Math.max(0, deadline - this.now())));
      retryDelayMs = Math.min(retryDelayMs * 2, 1000);
    }
  }

  private async ensureSocketReady(socketPath: string, spawnLock: string, deadline: number): Promise<void> {
    // The home directory may not exist yet on a fresh machine.
    mkdirSync(this.home, { recursive: true });
    let contentionDelayMs = 100;
    let lastContention = "";
    for (;;) {
      this.checkStartupOpen();
      if (this.now() >= deadline) throw new Error(`pi-famulus startup deadline exhausted${lastContention ? ` after contention: ${lastContention}` : ""}`);
      const acquired = this.tryAcquireSpawnLock(spawnLock);
      let attempt: SpawnAttempt | undefined;
      try {
        if (acquired) attempt = this.spawnManager();
        await this.waitForSocket(socketPath, deadline - this.now(), attempt);
        break;
      } catch (err) {
        // A daemon that lost manager.lock exits successfully, but did NOT
        // start our successor. Retry only this explicit contention state.
        if (!attempt?.contended || this.now() >= deadline) throw err;
        lastContention = attempt.failure?.message ?? "daemon lifetime lock held";
        this.log(`startup contention: ${lastContention}; retrying after ${contentionDelayMs}ms`);
      } finally {
        attempt?.dispose();
        if (acquired) this.releaseSpawnLock(spawnLock);
      }
      await delay(this.clock, Math.min(contentionDelayMs, deadline - this.now()));
      contentionDelayMs = Math.min(contentionDelayMs * 2, 1000);
    }
  }

  private startupFailureDetails(reason: string): string {
    const paths = famulusPaths(this.home);
    let managerPid = "missing/unreadable";
    let spawnHolder = "missing/unreadable";
    const describePid = (pid: number) => `${pid} (${pidAlive(pid) ? "alive" : "dead"})`;
    try {
      const info = JSON.parse(readFileSync(paths.pidFile, "utf8")) as { pid?: number };
      if (typeof info.pid === "number") managerPid = describePid(info.pid);
    } catch { /* diagnostic only */ }
    try {
      const raw = readFileSync(paths.spawnLock, "utf8").trim();
      spawnHolder = /^\d+$/.test(raw) ? describePid(Number(raw)) : "empty/non-pid";
    } catch { /* diagnostic only */ }
    return `${reason}; home=${this.home}; binary=${this.managerPath ?? "missing"}; socket=${existsSync(paths.socket) ? "present" : "missing"}; manager.pid=${managerPid}; spawn.lock=${spawnHolder}`;
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

  private spawnManager(): SpawnAttempt {
    if (!this.managerPath) {
      throw new Error("pi-famulus binary not found (set managerPath in config.json or PI_FAMULUS_MANAGER_PATH)");
    }
    // Pass --home explicitly: relying on PI_FAMULUS_HOME env inheritance breaks when
    // this.home came from an explicit override rather than the environment.
    const child = spawn(this.managerPath, ["--home", this.home, "daemon"], {
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
    const streams = [child.stdout, child.stderr];
    // Each pipe owns its partial UTF-8 bytes; interleaved streams cannot share carry.
    const decoders = streams.map(() => new StringDecoder("utf8"));
    const attempt: SpawnAttempt = {
      contended: false,
      output: "",
      dispose: () => {
        // Keep draining detached daemon pipes without keeping pi alive.
        // Closing a live child's reader would turn a later write into SIGPIPE.
        streams.forEach((stream, index) => {
          stream?.off("data", captures[index]);
          stream?.resume();
          (stream as net.Socket | null)?.unref?.();
        });
      },
    };
    const append = (text: string) => {
      const tail = (attempt.output + text).slice(-2048);
      // A UTF-16 bound must not retain just the low half of a surrogate pair.
      const first = tail.charCodeAt(0);
      attempt.output = first >= 0xdc00 && first <= 0xdfff ? tail.slice(1) : tail;
    };
    const captures = decoders.map((decoder) => (data: Buffer) => append(decoder.write(data)));
    streams.forEach((stream, index) => stream?.on("data", captures[index]));
    child.unref();
    child.on("error", (err) => { attempt.failure = new Error(`manager spawn failed: ${err.message}`); });
    // 'close' follows stdio EOF; 'exit' alone may precede the contention line.
    child.on("close", (code, signal) => {
      for (const decoder of decoders) append(decoder.end());
      attempt.contended = code === 0 && attempt.output.includes("pi-famulus already running");
      attempt.failure = new Error(`manager exited before socket ready (code=${code}, signal=${signal}): ${attempt.output.trim()}`);
    });
    return attempt;
  }

  private async waitForSocket(socketPath: string, timeoutMs: number, attempt?: SpawnAttempt): Promise<void> {
    const deadline = this.now() + timeoutMs;
    for (;;) {
      this.checkStartupOpen();
      if (existsSync(socketPath)) {
        const ok = await this.openSocket(socketPath, Math.min(SOCKET_READY_POLL_MS, Math.max(0, deadline - this.now())))
          .then((probe) => { probe.destroy(); return true; }, () => false);
        if (ok) return;
      }
      if (attempt?.failure) throw attempt.failure;
      if (this.now() >= deadline) {
        throw new Error(`timed out waiting for pi-famulus socket (spawn=${attempt ? "running" : "other holder"}${attempt?.output ? `, output=${attempt.output.trim()}` : ""})`);
      }
      await delay(this.clock, Math.min(SOCKET_READY_POLL_MS, deadline - this.now()));
    }
  }

  // -------------------------------------------------------------------------
  // Connection / protocol internals
  // -------------------------------------------------------------------------

  private openSocket(socketPath: string, timeoutMs: number): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
      const socket = net.connect(socketPath);
      const timer = this.clock.setTimeout(() => {
        socket.destroy();
        reject(new Error("timed out connecting to pi-famulus socket"));
      }, Math.max(0, timeoutMs));
      socket.once("connect", () => { this.clock.clearTimeout(timer); resolve(socket); });
      socket.once("error", (err) => {
        this.clock.clearTimeout(timer);
        socket.destroy();
        reject(err);
      });
    });
  }

  private async connectAndHello(socketPath: string, helloTimeoutMs = HELLO_TIMEOUT_MS): Promise<void> {
    const deadline = this.now() + Math.max(0, helloTimeoutMs);
    const socket = await this.openSocket(socketPath, helloTimeoutMs);
    if (this.intentionalClose) {
      socket.destroy();
      this.checkStartupOpen();
    }
    this.attachSocket(socket);
    try {
      await this.hello(Math.max(0, deadline - this.now()));
    } catch (err) {
      this.detachSocket();
      const reason = err instanceof ManagerError ? `${err.code}: ${err.message}` : (err as Error).message;
      throw new HelloError(reason, !(err instanceof ManagerError));
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
      // Preserve the actual handshake failure before onClose replaces it
      // with a generic connection-lost error.
      this.helloWaiter?.reject(new ManagerError("E_PROTOCOL", (err as Error).message));
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
        await this.connectFlow(RECONNECT_HELLO_TIMEOUT_MS, deadline);
        if (this.intentionalClose || this.rebound) return;
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

class HelloError extends Error {
  constructor(message: string, readonly transportFailure: boolean) { super(message); }
}

function errorFromResponse(msg: Record<string, unknown>): ManagerError {
  const err = msg.error as { code?: string; message?: string } | undefined;
  return new ManagerError(err?.code ?? "E_INTERNAL", err?.message ?? "unknown manager error");
}
