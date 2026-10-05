/**
 * RunRegistry (design doc Appendix B) plus the M3 runtime limits (§4.6):
 *
 * - run/child id generation ("run_xxxxxxxx" / "ch_xxxxxxxx", crypto random);
 * - child status transitions with onTransition callbacks (fleet widget,
 *   notifications);
 * - global concurrency admission (default 8 across all runs; excess children
 *   queue instead of being rejected);
 * - session spawn budget (default 32 child sessions per hour);
 * - lineage: v1 = same runId.
 *
 * Wiring contract: the runner must be constructed with
 * `acquire: (req) => registry.admitChild(req.childId)` so every generation
 * (initial start and resume) passes through admission. startChild() wires the
 * first generation's result; admitChild() wires subsequent generations.
 *
 * Zero pi dependency.
 */
import { randomBytes } from "node:crypto";
import { realClock, type Clock } from "../clock";
import type {
  ChildHandle,
  ChildResult,
  ChildRunRequest,
  ChildRunner,
  ChildStatus,
  DisposableChildHandle,
} from "./types";

// ---------------------------------------------------------------------------
// Appendix B public record shape
// ---------------------------------------------------------------------------

export interface RunRecord {
  runId: string;
  kind: "tasks" | "chain";
  children: {
    childId: string;
    name: string;
    agent: string;
    model?: string;
    status: ChildStatus;
    /** User-authored prompt (after chain interpolation), without injected preamble. */
    prompt?: string;
    /** Agent-authored instructions prepended to the task prompt. */
    preamble?: string;
    result?: ChildResult;
    startedAt: number;
    endedAt?: number;
  }[];
  status: "running" | "completed" | "partial" | "failed" | "interrupted";
  createdAt: number;
}

export interface RunRegistry {
  createRun(kind: RunRecord["kind"]): RunRecord;
  get(runId: string): RunRecord | undefined;
  list(): RunRecord[];
  handle(childId: string): ChildHandle | undefined; // active and finished (not disposed)
  findChild(runId: string, childIdOrName: string): ChildHandle | undefined;
  lineage(runIdA: string, runIdB: string): boolean; // v1: same runId
  onTransition(cb: (run: RunRecord) => void): void; // fleet widget / notifications
  disposeRun(runId: string): void;
}

// ---------------------------------------------------------------------------
// Registry implementation
// ---------------------------------------------------------------------------

export interface SubagentRegistryOptions {
  /** Global cap on concurrently running children (default 8). */
  maxConcurrentChildren?: number;
  /** Max child sessions created per hour (default 32). */
  spawnBudgetPerHour?: number;
  /** Shared time source. */
  clock?: Clock;
}

export interface StartChildOptions {
  /**
   * Re-checked after admission, just before the session is spawned. Returning
   * false cancels the child as interrupted (used for fail_fast while queued).
   * Cleared once the child reaches a terminal state.
   */
  shouldStart?: () => boolean;
}

export interface ActiveChildInfo {
  childId: string;
  runId: string;
  name: string;
  agent: string;
  model?: string;
  status: ChildStatus;
  startedAt: number;
  lastEventAt: number;
}

interface InternalChild {
  childId: string;
  runId: string;
  name: string;
  agent: string;
  /** Best-effort model id for fleet / ls (set when startChild runs). */
  model?: string;
  status: ChildStatus;
  prompt?: string;
  preamble?: string;
  result?: ChildResult;
  startedAt: number;
  endedAt?: number;
  handle?: ChildHandle;
  shouldStart?: () => boolean;
}

interface InternalRun {
  runId: string;
  kind: "tasks" | "chain";
  status: RunRecord["status"];
  createdAt: number;
  children: InternalChild[];
}

/** Error thrown by admission when a queued child is cancelled (fail_fast). */
export class ChildCancelledError extends Error {
  constructor(message = "cancelled (fail_fast)") {
    super(message);
    this.name = "ChildCancelledError";
  }
}

/** Synthetic handle for children that never got a session (e.g. spawn budget). */
class FailedChildHandle implements ChildHandle {
  constructor(
    readonly childId: string,
    private readonly failure: ChildResult,
    private readonly at: number,
  ) {}

  get result(): Promise<ChildResult> {
    return Promise.resolve(this.failure);
  }

  private dead(): Error {
    return new Error(`subagent ${this.childId} never started: ${this.failure.error ?? "failed"}`);
  }

  steer(): Promise<void> {
    return Promise.reject(this.dead());
  }

  followUp(): Promise<void> {
    return Promise.reject(this.dead());
  }

  resume(): Promise<void> {
    return Promise.reject(this.dead());
  }

  extend(): never {
    throw this.dead();
  }

  interrupt(): Promise<void> {
    return Promise.resolve();
  }

  status(): ChildStatus {
    return "failed";
  }

  lastEventAt(): number {
    return this.at;
  }

  resolvedModel(): string | undefined {
    return undefined;
  }

  conversation() {
    return [];
  }
}

export class SubagentRegistry implements RunRegistry {
  private readonly maxChildren: number;
  private readonly spawnBudget: number;
  private readonly clock: Clock;

  private runner: ChildRunner | null = null;
  private readonly runs = new Map<string, InternalRun>();
  private readonly children = new Map<string, InternalChild>();
  private readonly transitionCbs = new Set<(run: RunRecord) => void>();
  private activeSlots = 0;
  private readonly slotWaiters: (() => void)[] = [];
  private spawnTimes: number[] = [];

  constructor(opts: SubagentRegistryOptions = {}) {
    this.maxChildren = opts.maxConcurrentChildren ?? 8;
    this.spawnBudget = opts.spawnBudgetPerHour ?? 32;
    this.clock = opts.clock ?? realClock;
  }

  private now(): number {
    return this.clock.now();
  }

  /** Late-bound to break the registry <-> runner construction cycle. */
  setRunner(runner: ChildRunner): void {
    this.runner = runner;
  }

  // -------------------------------------------------------------------------
  // RunRegistry interface
  // -------------------------------------------------------------------------

  createRun(kind: RunRecord["kind"]): RunRecord {
    const run: InternalRun = {
      runId: this.newId("run_"),
      kind,
      status: "running",
      createdAt: this.now(),
      children: [],
    };
    this.runs.set(run.runId, run);
    this.emit(run);
    return snapshot(run);
  }

  get(runId: string): RunRecord | undefined {
    const run = this.runs.get(runId);
    return run ? snapshot(run) : undefined;
  }

  list(): RunRecord[] {
    return [...this.runs.values()].map(snapshot);
  }

  handle(childId: string): ChildHandle | undefined {
    return this.children.get(childId)?.handle;
  }

  findChild(runId: string, childIdOrName: string): ChildHandle | undefined {
    const run = this.runs.get(runId);
    if (!run) return undefined;
    const child =
      run.children.find((c) => c.childId === childIdOrName) ??
      run.children.find((c) => c.name === childIdOrName);
    return child?.handle;
  }

  lineage(runIdA: string, runIdB: string): boolean {
    return runIdA === runIdB;
  }

  onTransition(cb: (run: RunRecord) => void): void {
    this.transitionCbs.add(cb);
  }

  disposeRun(runId: string): void {
    const run = this.runs.get(runId);
    if (!run) return;
    const now = this.now();
    // Settle while children are still in the map. interrupt() calls settleChild,
    // which no-ops once the child has been deleted — that used to emit a final
    // "running" record (ghost agents in ls).
    for (const child of run.children) {
      if (child.status === "pending" || child.status === "running") {
        child.status = "interrupted";
        child.endedAt = now;
        child.result = {
          status: "interrupted",
          text: child.result?.text ?? "",
          error: "disposed",
          durationMs: Math.max(0, now - child.startedAt),
        };
      }
    }
    this.recomputeRunStatus(run);
    this.emit(run);
    this.runs.delete(runId);
    for (const child of run.children) {
      this.children.delete(child.childId);
      const handle = child.handle as DisposableChildHandle | undefined;
      if (!handle) continue;
      void handle.interrupt().catch(() => {});
      try {
        handle.dispose?.();
      } catch {
        // ignore
      }
    }
  }

  // -------------------------------------------------------------------------
  // M3 extensions beyond Appendix B
  // -------------------------------------------------------------------------

  /** Allocate a pending child inside a run. Returns the child id. */
  addChild(runId: string, info: { name: string; agent: string }): string {
    const run = this.runs.get(runId);
    if (!run) throw new Error(`unknown run ${runId}`);
    const child: InternalChild = {
      childId: this.newId("ch_"),
      runId,
      name: info.name,
      agent: info.agent,
      status: "pending",
      startedAt: this.now(),
    };
    run.children.push(child);
    this.children.set(child.childId, child);
    this.emit(run);
    return child.childId;
  }

  /**
   * Start a previously added child: consume spawn budget, launch via the
   * runner (which passes through admission), and wire status transitions.
   * Never throws for expected failures — they are recorded as child results.
   */
  async startChild(req: ChildRunRequest, opts?: StartChildOptions): Promise<ChildHandle> {
    const child = this.children.get(req.childId);
    if (!child) throw new Error(`unknown child ${req.childId} (addChild first)`);
    child.shouldStart = opts?.shouldStart;
    child.prompt = req.taskPrompt ?? req.prompt;
    child.preamble = req.agent.systemPrompt || undefined;
    child.model = req.model ?? req.agent.model;

    const runner = this.runner;
    if (!runner) {
      return this.failWithoutSession(child, "subagent runner is not initialized");
    }
    try {
      this.consumeSpawnBudget();
    } catch (err) {
      return this.failWithoutSession(child, (err as Error).message);
    }

    const handle = await runner.start(req);
    child.handle = handle;
    // Prefer the actually resolved provider/id over the request-time spec
    // (inherits parent model when neither param nor agent.model is set).
    const resolved = handle.resolvedModel();
    if (resolved) child.model = resolved;
    // Generation-1 result wiring (later generations are wired in admitChild,
    // where the handle is already visible).
    handle.result.then((result) => this.settleChild(child.childId, result));
    // Reflect "running" when the runner had no admission hook wired; terminal
    // states are left to the result wiring above so the result is recorded.
    if (handle.status() === "running" && child.status === "pending") {
      this.transitionChild(child, "running");
    } else if (resolved) {
      // Model became known after session create — refresh observers/disk.
      const run = this.runs.get(child.runId);
      if (run) this.emit(run);
    }
    return handle;
  }

  /**
   * Admission hook invoked by the runner once per generation. Waits for a
   * global concurrency slot, re-checks shouldStart, marks the child running,
   * and wires result settlement for resumed generations. Returns the slot
   * releaser.
   */
  async admitChild(childId: string): Promise<() => void> {
    await this.acquireSlot();
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      this.releaseSlot();
    };
    const child = this.children.get(childId);
    if (!child) {
      release();
      throw new Error(`unknown child ${childId}`);
    }
    if (child.shouldStart && !child.shouldStart()) {
      release();
      throw new ChildCancelledError();
    }
    this.transitionChild(child, "running");
    const handle = child.handle;
    if (handle) {
      // Resume generations: the handle (and its fresh result promise) already
      // exists, so wire settlement here. Generation 1 is wired by startChild.
      handle.result.then((result) => this.settleChild(childId, result));
    }
    return release;
  }

  /** Current generation's result promise for a child (post-resume aware). */
  getResult(childId: string): Promise<ChildResult> | undefined {
    return this.children.get(childId)?.handle?.result;
  }

  /**
   * After a pool/chain finishes, mark children that were never submitted
   * (still pending, no handle) as interrupted. Submitted-but-queued children
   * settle through their own handle wiring.
   */
  finalizeRun(runId: string, error: string): void {
    const run = this.runs.get(runId);
    if (!run) return;
    let changed = false;
    for (const child of run.children) {
      if (child.status === "pending" && !child.handle) {
        child.status = "interrupted";
        child.result = { status: "interrupted", text: "", error, durationMs: 0 };
        child.endedAt = this.now();
        changed = true;
      }
    }
    if (changed) {
      this.recomputeRunStatus(run);
      this.emit(run);
    }
  }

  /** Active (pending or running) children across all runs, oldest first. */
  activeChildren(): ActiveChildInfo[] {
    const out: ActiveChildInfo[] = [];
    for (const child of this.children.values()) {
      if (child.status !== "pending" && child.status !== "running") continue;
      out.push({
        childId: child.childId,
        runId: child.runId,
        name: child.name,
        agent: child.agent,
        model: child.model,
        status: child.status,
        startedAt: child.startedAt,
        lastEventAt: child.handle?.lastEventAt() ?? child.startedAt,
      });
    }
    out.sort((a, b) => a.startedAt - b.startedAt);
    return out;
  }

  /** Dispose every run (session shutdown). */
  disposeAll(): void {
    for (const runId of [...this.runs.keys()]) {
      this.disposeRun(runId);
    }
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  private failWithoutSession(child: InternalChild, error: string): ChildHandle {
    const result: ChildResult = { status: "failed", text: "", error, durationMs: 0 };
    const handle = new FailedChildHandle(child.childId, result, this.now());
    child.handle = handle;
    this.settleChild(child.childId, result);
    return handle;
  }

  private settleChild(childId: string, result: ChildResult): void {
    const child = this.children.get(childId);
    if (!child) return;
    if (child.status !== "pending" && child.status !== "running") return; // already terminal
    child.status = result.status;
    child.result = result;
    child.endedAt = this.now();
    child.shouldStart = undefined;
    const run = this.runs.get(child.runId);
    if (run) {
      this.recomputeRunStatus(run);
      this.emit(run);
    }
  }

  private transitionChild(child: InternalChild, status: ChildStatus): void {
    if (child.status === status) return;
    child.status = status;
    if (status === "running") {
      // (Re-)start: elapsed time and any previous generation's result reset.
      child.startedAt = this.now();
      child.result = undefined;
      child.endedAt = undefined;
    }
    const run = this.runs.get(child.runId);
    if (run) {
      this.recomputeRunStatus(run);
      this.emit(run);
    }
  }

  private recomputeRunStatus(run: InternalRun): void {
    if (run.children.length === 0) {
      run.status = "running";
      return;
    }
    const terminal = (s: ChildStatus) => s === "completed" || s === "failed" || s === "interrupted";
    if (run.children.some((c) => !terminal(c.status))) {
      run.status = "running";
      return;
    }
    const statuses = run.children.map((c) => c.status);
    if (statuses.every((s) => s === "completed")) run.status = "completed";
    else if (statuses.every((s) => s === "interrupted")) run.status = "interrupted";
    else if (statuses.every((s) => s === "failed")) run.status = "failed";
    else run.status = "partial";
  }

  private emit(run: InternalRun): void {
    if (this.transitionCbs.size === 0) return;
    const record = snapshot(run);
    for (const cb of this.transitionCbs) {
      try {
        cb(record);
      } catch {
        // observer errors must not break the registry
      }
    }
  }

  private newId(prefix: string): string {
    for (;;) {
      const id = prefix + randomBytes(4).toString("hex");
      if (!this.runs.has(id) && !this.children.has(id)) return id;
    }
  }

  private consumeSpawnBudget(): void {
    const cutoff = this.now() - 3_600_000;
    this.spawnTimes = this.spawnTimes.filter((t) => t > cutoff);
    if (this.spawnTimes.length >= this.spawnBudget) {
      throw new Error(
        `subagent spawn budget exhausted (${this.spawnBudget} sessions per hour); try again later`,
      );
    }
    this.spawnTimes.push(this.now());
  }

  private acquireSlot(): Promise<void> {
    if (this.activeSlots < this.maxChildren) {
      this.activeSlots++;
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.slotWaiters.push(resolve);
    });
  }

  private releaseSlot(): void {
    const next = this.slotWaiters.shift();
    if (next) {
      // Hand the slot off directly; the count stays at the cap.
      next();
    } else {
      this.activeSlots = Math.max(0, this.activeSlots - 1);
    }
  }
}

function snapshot(run: InternalRun): RunRecord {
  return {
    runId: run.runId,
    kind: run.kind,
    status: run.status,
    createdAt: run.createdAt,
    children: run.children.map((c) => ({
      childId: c.childId,
      name: c.name,
      agent: c.agent,
      ...(c.model !== undefined ? { model: c.model } : {}),
      status: c.status,
      ...(c.prompt !== undefined ? { prompt: c.prompt } : {}),
      ...(c.preamble !== undefined ? { preamble: c.preamble } : {}),
      result: c.result,
      startedAt: c.startedAt,
      endedAt: c.endedAt,
    })),
  };
}
