/**
 * InProcessRunner (design doc §4.6): ChildRunner backed by an in-process
 * child session created through an injected CreateSessionFn.
 *
 * Zero pi dependency — the real session factory lives in pi-runtime.ts and
 * tests inject fakes.
 *
 * Lifecycle per child ("generation" = one prompt..settle cycle; resume()
 * starts a new generation on the same session):
 * - a per-generation admission slot is acquired via the optional `acquire`
 *   hook (the registry uses it for the global concurrency cap);
 * - `timeoutMs` is a SOFT deadline per user turn: reaching it calls
 *   `onOverrun` (the parent gets a subagent-overrun wake) and repeats every
 *   `overrunRepeatMs`; the child is not aborted and its shell keeps running.
 *   `extend()` re-arms the deadline; steer/followUp after a reminder postpone
 *   the next one. See design.md §4.6 "Child lifecycle".
 * - `hardTimeoutMs` (opt-in, default off) is the abort ceiling per turn:
 *   abort -> result {status:"interrupted", error:"timeout"};
 * - a stall watchdog aborts the child after `stallMs` (default 10min) without
 *   any session event. A stall is treated as transient (a silently dropped
 *   provider stream): the child is aborted and auto-resumed on the SAME
 *   session with a continuation prompt, up to `stallRetries` times
 *   (default 1). Only when the retries are exhausted does the run settle as
 *   {status:"failed", error:"stalled"}. Retries keep the admission slot and
 *   the result promise; the transcript so far is preserved.
 * - the retry waits for the abort to finish before re-prompting: real pi
 *   rejects prompt() while a run is still active ("Agent is already
 *   processing"). The wait is bounded by `stallMs` — an abort that never
 *   completes means the stream ignored it and the session is dead.
 * - deadlines belong to the whole user turn: stall retries re-arm the soft
 *   deadline / reminder schedule and the hard ceiling from turn-level fields,
 *   never from the retry. A reminder landing during a retry delay is sent and
 *   the retry still runs; a hard ceiling landing there still settles.
 * - session/prompt exceptions -> {status:"failed", error}.
 *
 * Note on pi semantics: AgentSession.prompt() resolves only after the whole
 * agent run settles, so the prompt promise itself is the completion signal;
 * waitForIdle() is awaited afterwards as belt-and-braces for queued
 * steer/followUp processing.
 */
import { realClock, TimerScope, type Clock, type ClockTimer } from "../clock";
import type { OverrunTick } from "./overrun";
import type {
  ChildResult,
  ChildRunRequest,
  ChildRunner,
  ChildSessionAdapter,
  ChildStatus,
  CreateSessionFn,
  DisposableChildHandle,
} from "./types";

/** Inactivity abort. Paused while a tool is executing or a need_decision is pending. */
export const DEFAULT_STALL_MS = 5 * 60 * 1000;
/** Auto-resumes per stall before the run settles as failed. 0 disables retries. */
export const DEFAULT_STALL_RETRIES = 1;
/** Pause between the stall abort and the retry prompt (ms). */
export const DEFAULT_STALL_RETRY_DELAY_MS = 5_000;
/** Overrun reminder interval while a child stays past its soft budget (ms). */
export const DEFAULT_OVERRUN_REPEAT_MS = 10 * 60 * 1000;

/** Continuation prompt for a stall retry: the transcript holds the context. */
export function stallRetryPrompt(stallMs: number): string {
  return (
    `[system: the previous attempt stalled with no activity for over ${Math.round(stallMs / 1000)}s ` +
    "and was interrupted mid-run; the transcript so far is preserved. Continue from where you left off.]"
  );
}

/** Identity of one generation's slot request (see InProcessRunnerOptions.acquire). */
export interface AdmissionTicket {
  /** False once the requesting generation settled or was superseded. */
  current: () => boolean;
}

export interface InProcessRunnerOptions {
  createSession: CreateSessionFn;
  /** Stall watchdog timeout (ms). Default 10 minutes. */
  stallMs?: number;
  /** Auto-resume attempts after a stall. Default 1; 0 settles stalled at once. */
  stallRetries?: number;
  /** Delay between stall abort and the retry prompt (ms). Default 5s. */
  stallRetryDelayMs?: number;
  /** Called on every stall detection (before any retry), with the 1-based attempt. */
  onStall?: (childId: string, attempt: number) => void;
  /** Overrun reminder interval (ms). Default 10 minutes. */
  overrunRepeatMs?: number;
  /** Abort ceiling per user turn (ms). 0/absent = off: the soft deadline never aborts. */
  hardTimeoutMs?: number;
  /**
   * The child passed its soft deadline (first reminder) or is still past it
   * (later reminders). The child keeps running; the parent decides.
   */
  onOverrun?: (tick: OverrunTick) => void;
  /** Shared time source and scheduler. */
  clock?: Clock;
  /**
   * Per-generation admission hook. Awaited before each (re)start; the
   * releaser gets terminal=false for a resumable interruption so its local
   * slot can be returned while the child-owned machine permit is retained.
   * Rejecting cancels the generation as {status:"interrupted", error}.
   */
  acquire?: (req: ChildRunRequest, ticket: AdmissionTicket) => Promise<(terminal?: boolean) => void>;
  /**
   * Called after the child's conversation may have changed (a message or
   * tool finished, or the generation settled). Used to persist transcripts.
   */
  onActivity?: (childId: string) => void;
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export class InProcessRunner implements ChildRunner {
  private readonly opts: InProcessRunnerOptions;

  constructor(opts: InProcessRunnerOptions) {
    this.opts = opts;
  }

  async start(req: ChildRunRequest): Promise<DisposableChildHandle> {
    const handle = new InProcessChildHandle(req, this.opts);
    await handle.launch();
    return handle;
  }
}

class InProcessChildHandle implements DisposableChildHandle {
  private readonly req: ChildRunRequest;
  private readonly createSession: CreateSessionFn;
  private readonly stallMs: number;
  private readonly stallRetries: number;
  private readonly stallRetryDelayMs: number;
  private readonly onStall?: (childId: string, attempt: number) => void;
  private readonly overrunRepeatMs: number;
  private readonly hardTimeoutMs: number;
  private readonly onOverrun?: (tick: OverrunTick) => void;
  private readonly clock: Clock;
  private readonly acquire?: (req: ChildRunRequest, ticket: AdmissionTicket) => Promise<(terminal?: boolean) => void>;
  private readonly onActivity?: (childId: string) => void;

  private session: ChildSessionAdapter | null = null;
  private resolvedModel_: string | undefined;
  private unsubscribe: (() => void) | null = null;
  private status_: ChildStatus = "pending";
  private lastEvent: number;
  /** Generation start (reset per generation; durationMs is relative to runStartedAt). */
  private startedAt: number;
  /** User-turn start: reset on launch and user resume(), not on stall retries. */
  private runStartedAt: number;
  /**
   * When the turn's deadlines start counting. Set after admission and session
   * creation (queue wait must not eat the budget), and NOT reset by stall
   * retries — a stall already consumes budget time by definition.
   */
  private turnBudgetStart: number;
  /** This turn's soft budget: spawn timeoutMs, or resume()'s timeoutMs. */
  private turnBudgetMs: number;
  /** Soft deadline (absolute). Moved by extend(). Null = no soft deadline. */
  private softDeadlineAt: number | null = null;
  /** When the soft timer fires next: softDeadlineAt, then each reminder. */
  private nextReminderAt: number | null = null;
  /** Overrun reminders sent this turn. */
  private reminders = 0;
  /** Abort kicked off by the latest stall detection; awaited (bounded) by the retry. */
  private abortPromise: Promise<void> | null = null;
  private generation = 0;
  private settledFlag = false;
  private resolveResult!: (result: ChildResult) => void;
  private resultPromise: Promise<ChildResult>;
  /**
   * One-shot waiters resolved by the NEXT settle() — an interrupt/dispose/
   * timeout landing during an abort wait unwinds the waiter instead of
   * leaking it when the abort hangs forever. Waiters are per-wait: a settle
   * that already happened must not poison a later wait (resume runs after
   * the previous turn's settle by definition).
   */
  private settleWaiters: Array<() => void> = [];
  private softTimer: ClockTimer | null = null;
  private hardTimer: ClockTimer | null = null;
  private stallTimer: ClockTimer | null = null;
  private retryTimer: { scope: TimerScope; id: ClockTimer } | null = null;
  private timerScope: TimerScope | null = null;
  /** Stall detections so far (across generations, per handle). */
  private stallAttempts = 0;
  /** Generation retired by a stall detection awaiting its retry. */
  private retiredGen: number | null = null;
  private releaseSlot: ((terminal?: boolean) => void) | null = null;
  /** Whether the current generation's settled result is terminal for this child. */
  private terminalSettle = false;
  private disposed = false;
  /** Nested tool_execution_start/end. Stall stays paused while > 0. */
  private toolDepth = 0;
  /** contact_supervisor need_decision. Stall stays paused while true. */
  private decisionPaused = false;

  constructor(req: ChildRunRequest, opts: InProcessRunnerOptions) {
    this.req = req;
    this.createSession = opts.createSession;
    this.stallMs = opts.stallMs ?? DEFAULT_STALL_MS;
    const rawRetries = opts.stallRetries ?? DEFAULT_STALL_RETRIES;
    this.stallRetries = Number.isFinite(rawRetries)
      ? Math.max(0, Math.floor(rawRetries))
      : DEFAULT_STALL_RETRIES;
    const rawDelay = opts.stallRetryDelayMs ?? DEFAULT_STALL_RETRY_DELAY_MS;
    this.stallRetryDelayMs = Number.isFinite(rawDelay)
      ? Math.max(0, Math.floor(rawDelay))
      : DEFAULT_STALL_RETRY_DELAY_MS;
    this.onStall = opts.onStall;
    const rawRepeat = opts.overrunRepeatMs ?? DEFAULT_OVERRUN_REPEAT_MS;
    this.overrunRepeatMs =
      Number.isFinite(rawRepeat) && rawRepeat > 0
        ? Math.max(1, Math.floor(rawRepeat))
        : DEFAULT_OVERRUN_REPEAT_MS;
    const rawHard = opts.hardTimeoutMs ?? 0;
    this.hardTimeoutMs = Number.isFinite(rawHard) && rawHard > 0 ? Math.floor(rawHard) : 0;
    this.onOverrun = opts.onOverrun;
    this.turnBudgetMs = req.timeoutMs;
    this.clock = opts.clock ?? realClock;
    this.acquire = opts.acquire;
    this.onActivity = opts.onActivity;
    this.startedAt = this.clock.now();
    this.runStartedAt = this.startedAt;
    this.turnBudgetStart = this.startedAt;
    this.lastEvent = this.startedAt;
    this.resultPromise = new Promise((resolve) => {
      this.resolveResult = resolve;
    });
  }

  get childId(): string {
    return this.req.childId;
  }

  /** Current generation's result promise (see Appendix B note on resume). */
  get result(): Promise<ChildResult> {
    return this.resultPromise;
  }

  status(): ChildStatus {
    return this.status_;
  }

  lastEventAt(): number {
    return this.lastEvent;
  }

  resolvedModel(): string | undefined {
    return this.resolvedModel_ ?? this.session?.resolvedModel;
  }

  /**
   * need_decision pending: pause the stall watchdog (nested with tool
   * execution) and hold overrun reminders. The soft budget keeps counting.
   */
  pauseStall(): void {
    this.decisionPaused = true;
    this.clearStall();
  }

  resumeStall(): void {
    this.decisionPaused = false;
    this.lastEvent = this.clock.now();
    if (this.status_ === "running" && this.toolDepth === 0) this.armStall(this.generation);
    // Deliver a reminder held during the decision (due now, so it fires at
    // once); otherwise this re-arms the unchanged schedule.
    if (this.status_ === "running" && !this.settledFlag) this.armSoftDeadline(this.generation);
  }

  conversation() {
    return this.session?.getConversation() ?? [];
  }

  /** Launch generation 1. Resolves once the prompt is issued (not completed). */
  async launch(): Promise<void> {
    await this.beginGeneration(this.req.prompt, true);
  }

  async steer(message: string): Promise<void> {
    if (this.status_ !== "running" || !this.session) {
      throw new Error(`subagent ${this.req.childId} is not running (status: ${this.status_})`);
    }
    if (this.retiredGen !== null) {
      // The previous generation was aborted; delivery would be undefined.
      throw new Error(`subagent ${this.req.childId} is restarting after a stall; retry shortly`);
    }
    const gen = this.generation;
    await this.session.steer(message);
    this.parentActed(gen);
  }

  async followUp(message: string): Promise<void> {
    if (this.status_ !== "running" || !this.session) {
      throw new Error(`subagent ${this.req.childId} is not running (status: ${this.status_})`);
    }
    if (this.retiredGen !== null) {
      throw new Error(`subagent ${this.req.childId} is restarting after a stall; retry shortly`);
    }
    const gen = this.generation;
    await this.session.followUp(message);
    this.parentActed(gen);
  }

  /**
   * Parent action on an overrun: move the soft deadline to now + timeoutMs
   * (default: the spawn budget). The reminder count continues; the hard
   * ceiling (if any) does not move.
   */
  extend(timeoutMs?: number): { deadlineAt: number; hardDeadlineAt: number | null } {
    if (this.status_ !== "running") {
      throw new Error(`subagent ${this.req.childId} is not running (status: ${this.status_})`);
    }
    const ms = timeoutMs ?? this.req.timeoutMs;
    if (!(ms > 0)) throw new Error("extend needs a positive timeout_ms");
    this.softDeadlineAt = this.now() + ms;
    this.nextReminderAt = this.softDeadlineAt;
    this.armSoftDeadline(this.generation);
    return {
      deadlineAt: this.softDeadlineAt,
      hardDeadlineAt: this.hardTimeoutMs > 0 ? this.turnBudgetStart + this.hardTimeoutMs : null,
    };
  }

  /**
   * steer/followUp after a reminder: the parent is handling the overrun, so
   * the next reminder waits a full interval. Before the first reminder the
   * deadline is untouched, and so is a deadline that extend() moved into the
   * future: the child is inside its budget again, and the next wake belongs
   * at that deadline, not one repeat from now.
   */
  private parentActed(gen: number): void {
    // The delivery was awaited: if its turn settled (and maybe a resume
    // started a new one) meanwhile, it must not move the new schedule.
    if (gen !== this.generation || this.settledFlag) return;
    if (this.reminders === 0 || this.status_ !== "running") return;
    const now = this.now();
    if (this.softDeadlineAt !== null && now < this.softDeadlineAt) return;
    this.nextReminderAt = now + this.overrunRepeatMs;
    this.armSoftDeadline(this.generation);
  }

  /**
   * Start a new user turn. Resolves once the request is accepted, not when
   * the turn starts: draining a still-unwinding abort and waiting for an
   * admission slot run in the background, so the parent's tool call never
   * waits for a slot. Until admission the child is "pending" (queued).
   * Failures after acceptance settle the new turn instead of rejecting.
   */
  async resume(message: string, opts: { timeoutMs?: number } = {}): Promise<void> {
    if (this.status_ === "running" || this.status_ === "pending") {
      throw new Error(
        `subagent ${this.req.childId} is still ${this.status_}; use steer for a running subagent`,
      );
    }
    if (this.disposed) {
      throw new Error(`subagent ${this.req.childId} has been disposed`);
    }
    if (!this.session) {
      throw new Error(`subagent ${this.req.childId} has no session to resume`);
    }
    // A new user turn gets a fresh stall budget and a fresh soft budget:
    // the one passed with resume, else the spawn budget.
    this.stallAttempts = 0;
    this.turnBudgetMs =
      opts.timeoutMs !== undefined && opts.timeoutMs > 0 ? opts.timeoutMs : this.req.timeoutMs;
    this.reminders = 0;
    this.runStartedAt = this.clock.now();
    // The turn's generation exists from here: interrupt()/dispose() while it
    // is queued settle it, and its result promise is observable through
    // registry.getResult() before admission.
    const gen = ++this.generation;
    this.settledFlag = false;
    this.retiredGen = null;
    this.status_ = "pending";
    this.startedAt = this.runStartedAt;
    this.lastEvent = this.runStartedAt;
    this.resultPromise = new Promise((resolve) => {
      this.resolveResult = resolve;
    });
    const pendingAbort = this.abortPromise;
    this.abortPromise = null;
    void this.startResumedTurn(gen, message, pendingAbort);
  }

  /**
   * Background half of resume(): drain a stall/timeout abort that is still
   * unwinding (real pi rejects prompt() while the aborted run is active),
   * then admission and prompt. Bounded: a hung abort means the session is
   * dead, and the turn settles failed.
   */
  private async startResumedTurn(gen: number, message: string, pendingAbort: Promise<void> | null): Promise<void> {
    if (pendingAbort && !(await this.awaitAbortBounded(pendingAbort))) {
      this.settle(gen, {
        status: "failed",
        text: "",
        error: `subagent ${this.req.childId} session did not go idle after the abort; cannot resume`,
        durationMs: this.now() - this.runStartedAt,
      });
      return;
    }
    if (this.disposed || this.isSettled(gen)) return;
    await this.beginGeneration(message, false, false, gen);
  }

  async interrupt(): Promise<void> {
    if (this.status_ !== "running" && this.status_ !== "pending") return;
    this.settle(this.generation, {
      status: "interrupted",
      // A pending turn has produced nothing yet; the last assistant text
      // belongs to the previous turn.
      text: this.status_ === "pending" ? "" : this.partialText(),
      durationMs: this.now() - this.startedAt,
    });
    try {
      await this.session?.abort();
    } catch {
      // abort is best-effort; the result is already settled
    }
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimers();
    if (this.unsubscribe) {
      try {
        this.unsubscribe();
      } catch {
        // ignore
      }
      this.unsubscribe = null;
    }
    const session = this.session;
    this.session = null;
    if (session) {
      try {
        session.dispose();
      } catch {
        // ignore
      }
    }
    // Never leave result waiters hanging.
    this.settle(this.generation, { status: "interrupted", text: "", error: "disposed", durationMs: 0 });
    // settle() can be a no-op if the child was already interrupted; disposal
    // is terminal and must still return its retained machine permit.
    this.release(true);
  }

  // -------------------------------------------------------------------------
  // Generation machinery
  // -------------------------------------------------------------------------

  /**
   * Start a generation. `reuseSlot` is used by stall retries: the admission
   * slot acquired by the stalled generation is still held (no settle happened),
   * so admission is not re-run and the slot is not double-counted.
   */
  private async beginGeneration(
    prompt: string,
    first: boolean,
    reuseSlot = false,
    resumedGen?: number,
  ): Promise<void> {
    // A resumed turn allocated its generation when it was requested.
    const gen = resumedGen ?? ++this.generation;
    this.retiredGen = null;
    this.clearRetryTimer();
    this.timerScope?.dispose();
    this.timerScope = new TimerScope(this.clock);
    this.settledFlag = false;
    this.terminalSettle = false;
    this.status_ = "pending";
    this.startedAt = this.clock.now();
    this.lastEvent = this.startedAt;
    // A tool_execution_end from the previous generation may have been dropped
    // after settle. Don't carry that depth (or a pending decision) into this one.
    this.toolDepth = 0;
    this.decisionPaused = false;

    if (this.acquire && !reuseSlot) {
      try {
        // The ticket ties the slot request to THIS generation: a request
        // whose generation settled while queued (interrupt, then a new
        // resume) must not admit the child's next generation.
        this.releaseSlot = await this.acquire(this.req, {
          current: () => !this.disposed && !this.isSettled(gen),
        });
      } catch (err) {
        // Admission denied (e.g. fail_fast cancellation while queued).
        this.settle(gen, {
          status: "interrupted",
          text: "",
          error: errorMessage(err),
          durationMs: this.now() - this.startedAt,
        });
        return;
      }
    }
    if (this.disposed || this.isSettled(gen)) {
      this.release(this.terminalSettle);
      return;
    }
    this.status_ = "running";

    if (first) {
      try {
        this.session = await this.createSession(this.req);
        this.resolvedModel_ = this.session.resolvedModel;
      } catch (err) {
        this.settle(gen, {
          status: "failed",
          text: "",
          error: errorMessage(err),
          durationMs: this.now() - this.startedAt,
        });
        return;
      }
      if (this.disposed || this.isSettled(gen)) {
        this.release();
        return;
      }
      this.unsubscribe = this.session.subscribe((event) => {
        if (event.type === "message_end" || event.type === "tool_execution_end" || event.type === "agent_end") {
          this.notifyActivity();
        }
        // Track depth even after settle. A late tool_execution_end must not
        // leak into the next resume, and must not rearm a stale generation.
        if (event.type === "tool_execution_start") {
          this.toolDepth++;
          if (this.status_ === "running") this.clearStall();
          return;
        }
        if (event.type === "tool_execution_end") {
          this.toolDepth = Math.max(0, this.toolDepth - 1);
          this.lastEvent = this.now();
          if (this.status_ === "running" && this.toolDepth === 0 && !this.decisionPaused) {
            this.armStall(this.generation);
          }
          return;
        }
        if (this.status_ !== "running") return;
        this.lastEvent = this.now();
        if (this.toolDepth === 0 && !this.decisionPaused) this.armStall(this.generation);
      });
    }

    const session = this.session;
    if (!session) {
      this.settle(gen, {
        status: "failed",
        text: "",
        error: "no session available",
        durationMs: this.now() - this.startedAt,
      });
      return;
    }

    // The turn's deadlines start only after admission and session creation:
    // time spent queued for an admission slot is not the child's budget.
    // Stall retries (reuseSlot) keep the turn's deadlines and schedule.
    if (!reuseSlot) {
      this.turnBudgetStart = this.now();
      this.softDeadlineAt = this.turnBudgetMs > 0 ? this.turnBudgetStart + this.turnBudgetMs : null;
      this.nextReminderAt = this.softDeadlineAt;
    }
    this.armHardCeiling(gen);
    // A stall can spend the whole hard budget before this generation starts;
    // armHardCeiling settles in that case and prompting must not proceed.
    if (this.isSettled(gen)) return;
    this.armSoftDeadline(gen);
    this.armStall(gen);
    // Tell the child which model it is — otherwise only the parent/fleet knows.
    const prompted =
      first && session.resolvedModel
        ? `You are running as model ${session.resolvedModel}.\n\n${prompt}`
        : prompt;
    // Floating: prompt() resolves when the whole run settles (pi semantics).
    session.prompt(prompted).then(
      () => {
        void this.finishGeneration(gen);
      },
      (err) => {
        if (!this.isCurrent(gen)) return;
        this.settle(gen, {
          status: "failed",
          text: this.partialText(),
          error: errorMessage(err),
          durationMs: this.now() - this.startedAt,
        });
      },
    );
  }

  private async finishGeneration(gen: number): Promise<void> {
    if (!this.isCurrent(gen)) return;
    const session = this.session;
    if (session) {
      try {
        await session.waitForIdle();
      } catch {
        // A waitForIdle failure after a resolved prompt is not actionable;
        // fall through and report whatever text we have.
      }
    }
    if (!this.isCurrent(gen)) return;
    const failure = session?.getLastAssistantFailure?.();
    if (failure) {
      this.settle(gen, {
        status: "failed",
        text: this.partialText(),
        error: failure.errorMessage?.trim() || `Model stopped with ${failure.stopReason}`,
        endReason: "model-error",
        durationMs: this.now() - this.startedAt,
      });
      return;
    }
    this.settle(gen, {
      status: "completed",
      text: this.partialText(),
      durationMs: this.now() - this.startedAt,
    });
  }

  private partialText(): string {
    return this.session?.getLastAssistantText() || "(no output)";
  }

  private now(): number {
    return this.clock.now();
  }

  private isSettled(gen: number): boolean {
    return gen !== this.generation || this.settledFlag;
  }

  private isCurrent(gen: number): boolean {
    // A stall-retired generation is neither settled nor current: its aborted
    // prompt resolves shortly after detection and must not be reported as
    // the child's completion while the retry is still pending.
    return gen === this.generation && !this.settledFlag && !this.disposed && gen !== this.retiredGen;
  }

  private settle(gen: number, result: ChildResult): void {
    if (gen !== this.generation || this.settledFlag) return;
    this.settledFlag = true;
    this.clearTimers();
    // Generations run: 1 + resumes + stall retries. Forensics for the
    // incident class this exists for (a stalled child that needed retries).
    // Omitted on the first generation so a clean run keeps a lean shape.
    if (result.attempts === undefined && this.generation > 1) {
      result.attempts = this.generation;
    }
    // durationMs covers the whole user turn (launch/resume -> settle),
    // including time lost to stalls and retry delays. dispose keeps its 0.
    if (result.error !== "disposed") result.durationMs = this.now() - this.runStartedAt;
    if (result.stalls === undefined && this.stallAttempts > 0) {
      result.stalls = this.stallAttempts;
    }
    const waiters = this.settleWaiters.splice(0);
    for (const wake of waiters) wake();
    // Surface non-fatal setup caveats (e.g. agent-def model fallback) once.
    if (result.warning === undefined && this.session?.warning) {
      result.warning = this.session.warning;
    }
    this.status_ = result.status;
    this.terminalSettle = result.status !== "interrupted" || result.error === "disposed";
    this.release(this.terminalSettle);
    this.notifyActivity();
    this.resolveResult(result);
  }

  private notifyActivity(): void {
    try {
      this.onActivity?.(this.req.childId);
    } catch {
      // persistence observers must not break the child lifecycle
    }
  }

  private release(terminal = true): void {
    const release = this.releaseSlot;
    if (terminal) this.releaseSlot = null;
    if (release) {
      try {
        release(terminal);
      } catch {
        // ignore
      }
    }
  }

  /**
   * Soft deadline / reminder timer for the current generation scope, due at
   * nextReminderAt (turn-level, so a stall retry re-arms the same schedule).
   */
  private armSoftDeadline(gen: number): void {
    if (this.softTimer !== null) {
      this.timerScope?.clearTimeout(this.softTimer);
      this.softTimer = null;
    }
    if (this.nextReminderAt === null) return;
    const delay = Math.max(0, this.nextReminderAt - this.now());
    this.softTimer = this.timerScope?.setTimeout(() => {
      this.softTimer = null;
      // Deliberately NOT isCurrent(): during a stall's retry delay the child
      // is still running from the parent's view, so the reminder is due.
      if (this.settledFlag || this.disposed || gen !== this.generation) return;
      // A pending need_decision already put a supervisor-request wake in
      // front of the parent: hold the reminder (nextReminderAt stays due)
      // until resumeStall() re-arms it when the decision resolves.
      if (this.decisionPaused) return;
      this.fireOverrun(gen);
    }, delay) ?? null;
  }

  private fireOverrun(gen: number): void {
    const now = this.now();
    this.reminders++;
    this.nextReminderAt = now + this.overrunRepeatMs;
    try {
      this.onOverrun?.({
        childId: this.req.childId,
        elapsedMs: now - this.turnBudgetStart,
        budgetMs: (this.softDeadlineAt ?? now) - this.turnBudgetStart,
        reminder: this.reminders,
        nextReminderMs: this.overrunRepeatMs,
        lastEventAt: this.lastEvent,
        hardRemainingMs:
          this.hardTimeoutMs > 0 ? Math.max(0, this.turnBudgetStart + this.hardTimeoutMs - now) : null,
      });
    } catch {
      // overrun observers must not break the schedule
    }
    this.armSoftDeadline(gen);
  }

  /**
   * Opt-in abort ceiling (hardTimeoutMs). The ceiling is per user turn: a
   * stall retry arms only what the stalled generation did not already
   * consume, and the budget clock starts after admission (turnBudgetStart).
   */
  private armHardCeiling(gen: number): void {
    if (this.hardTimer !== null) {
      this.timerScope?.clearTimeout(this.hardTimer);
      this.hardTimer = null;
    }
    if (!(this.hardTimeoutMs > 0)) return;
    const remaining = this.hardTimeoutMs - (this.now() - this.turnBudgetStart);
    if (remaining <= 0) {
      this.settle(gen, {
        status: "interrupted",
        text: this.partialText(),
        error: "timeout",
        durationMs: this.now() - this.runStartedAt,
      });
      return;
    }
    this.hardTimer = this.timerScope?.setTimeout(() => {
      this.hardTimer = null;
      // Deliberately NOT isCurrent(): a ceiling landing during a stall's
      // retry delay must still fire — the retry has not started a new
      // generation, and the budget is spent.
      if (this.settledFlag || this.disposed || gen !== this.generation) return;
      this.settle(gen, {
        status: "interrupted",
        text: this.partialText(),
        error: "timeout",
        durationMs: this.now() - this.runStartedAt,
      });
      // Track the abort: a quick user resume() must wait for it before
      // re-prompting, or real pi rejects the prompt ("already processing").
      this.abortPromise = this.session ? this.session.abort().catch(() => {}) : null;
    }, remaining) ?? null;
  }

  private armStall(gen: number): void {
    if (this.stallTimer !== null) {
      this.timerScope?.clearTimeout(this.stallTimer);
      this.stallTimer = null;
    }
    if (!(this.stallMs > 0)) return;
    this.stallTimer = this.timerScope?.setTimeout(() => {
      this.stallTimer = null;
      if (!this.isCurrent(gen)) return;
      this.handleStall(gen);
    }, this.stallMs) ?? null;
  }

  /**
   * Stall detection. Abort the hung generation (frees a silently dropped
   * provider stream), then either settle failed (retries exhausted) or
   * schedule an auto-resume on the same session after `stallRetryDelayMs`.
   * The admission slot and result promise survive; the transcript is kept.
   */
  private handleStall(gen: number): void {
    this.stallAttempts++;
    // Persist whatever the stalled generation produced before it went quiet.
    this.notifyActivity();
    try {
      this.onStall?.(this.req.childId, this.stallAttempts);
    } catch {
      // stall observers must not break the retry machinery
    }
    this.retiredGen = gen;
    // Kick the abort off now so the unwind overlaps the retry delay; the
    // retry timer awaits it (bounded) before re-prompting. Real pi rejects
    // prompt() while the aborted run is still active.
    this.abortPromise = this.session ? this.session.abort().catch(() => {}) : null;
    if (this.stallAttempts > this.stallRetries) {
      this.settle(gen, {
        status: "failed",
        text: this.partialText(),
        error: "stalled",
        durationMs: this.now() - this.runStartedAt,
      });
      return;
    }
    const scope = this.timerScope;
    if (!scope) return;
    const id = scope.setTimeout(() => {
      this.retryTimer = null;
      // interrupt()/dispose() during the delay settles this generation. The
      // retiredGen exclusion in isCurrent() must NOT apply here: retrying the
      // retired generation is precisely this timer's job.
      if (this.settledFlag || this.disposed || gen !== this.generation) return;
      void this.retryAfterAbort(gen);
    }, this.stallRetryDelayMs);
    this.retryTimer = { scope, id };
  }

  /**
   * Resume after a stall: wait for the abort to finish (bounded by stallMs),
   * then re-prompt the same session. An abort that never completes means the
   * stream ignored it — the session is dead, so the retry is skipped and the
   * run settles failed (stalled).
   */
  private async retryAfterAbort(gen: number): Promise<void> {
    if (this.settledFlag || this.disposed || gen !== this.generation) return;
    const abort = this.abortPromise;
    this.abortPromise = null;
    if (abort && !(await this.awaitAbortBounded(abort))) {
      // Bound expired: the hung stream ignored the abort and the session is
      // dead, so the retry is skipped.
      this.settle(gen, {
        status: "failed",
        text: this.partialText(),
        error: "stalled",
        durationMs: this.now() - this.runStartedAt,
      });
      return;
    }
    if (this.settledFlag || this.disposed || gen !== this.generation) return;
    await this.beginGeneration(stallRetryPrompt(this.stallMs), false, true);
  }

  /**
   * Await an abort with a bound (stallMs). Resolves true when the abort
   * finished, false on bound expiry or after settle() — the settled signal
   * is raced in so an interrupt/dispose/timeout during the wait unwinds this
   * closure even when the abort hangs forever (no handle-graph leak).
   * Uses the raw clock, not a TimerScope: callers may run after settle,
   * when the generation scope is already disposed.
   */
  private async awaitAbortBounded(abort: Promise<void>): Promise<boolean> {
    let boundTimer: ClockTimer | null = null;
    const bound = new Promise<false>((resolve) => {
      boundTimer = this.clock.setTimeout(() => resolve(false), this.stallMs);
    });
    let waiter: (() => void) | null = null;
    const settledDuringWait = new Promise<false>((resolve) => {
      waiter = () => resolve(false);
      this.settleWaiters.push(waiter);
    });
    const ok = await Promise.race([abort.then(() => true as const), bound, settledDuringWait]);
    if (boundTimer !== null) this.clock.clearTimeout(boundTimer);
    if (waiter !== null) this.settleWaiters = this.settleWaiters.filter((w) => w !== waiter);
    return ok;
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== null) {
      this.retryTimer.scope.clearTimeout(this.retryTimer.id);
      this.retryTimer = null;
    }
  }

  private clearStall(): void {
    if (this.stallTimer !== null) {
      this.timerScope?.clearTimeout(this.stallTimer);
      this.stallTimer = null;
    }
  }

  private clearTimers(): void {
    if (this.softTimer !== null) {
      this.timerScope?.clearTimeout(this.softTimer);
      this.softTimer = null;
    }
    if (this.hardTimer !== null) {
      this.timerScope?.clearTimeout(this.hardTimer);
      this.hardTimer = null;
    }
    this.clearStall();
    this.clearRetryTimer();
    this.timerScope?.dispose();
    this.timerScope = null;
  }
}
