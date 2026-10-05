/**
 * Shared fakes for subagent tests: an event-driven controllable
 * ChildSessionAdapter and a CreateSessionFn factory.
 */
import type { ChildRunRequest, ChildSessionAdapter, CreateSessionFn } from "../../src/subagent/types";
import type { AgentDefinition } from "../../src/subagent/types";

export const WORKER_AGENT: AgentDefinition = {
  name: "worker",
  description: "test worker",
  tools: ["read", "bash", "edit", "write"],
  systemPrompt: "",
  source: "builtin",
};

/**
 * Fake child session. prompt() mimics real pi semantics: it resolves only
 * once the run settles. With `autoComplete` set, the run settles immediately;
 * otherwise the test drives completion via complete()/abort().
 */
export class FakeChildSession implements ChildSessionAdapter {
  readonly prompts: string[] = [];
  readonly steers: string[] = [];
  readonly followUps: string[] = [];
  aborts = 0;
  disposed = false;
  lastText: string | undefined;
  lastAssistantFailure: { stopReason: "error" | "aborted"; errorMessage?: string } | undefined;
  streaming = false;
  /** When set, runner prepends "You are running as model …" on first prompt. */
  resolvedModel?: string;
  /** Non-fatal caveat the runner copies onto the child result. */
  warning?: string;
  /** When non-null, prompt() completes immediately with this text. */
  autoComplete: string | null = null;
  /** When set, prompt() rejects with this error. */
  promptError: Error | null = null;
  /**
   * Mirrors real pi (agent-session): prompt() rejects while a run is still
   * active. Disable only for tests that specifically exercise queueing.
   */
  rejectPromptWhileStreaming = true;
  /** When true, abort() never takes the session idle (hung stream ignores abort). */
  hungAbort = false;
  /** When closed, abort() blocks until openAbortGate() (async abort unwind). */
  abortGateOpen = true;

  private readonly listeners = new Set<(e: { type: string }) => void>();
  private idleWaiters: (() => void)[] = [];
  private abortGateWaiters: (() => void)[] = [];
  /** Signals of tools still executing; abort() aborts them (pi semantics). */
  private readonly runningTools = new Set<AbortController>();

  /**
   * Start a tool call, as pi does: tool_execution_start, and the tool gets an
   * AbortSignal that session.abort() fires. Child bash stops its manager
   * shell on that signal (`task.stop reason=tool`), so `signal.aborted` is the
   * observable for "the child's foreground shell was stopped".
   */
  runTool(): { signal: AbortSignal; end: () => void } {
    const controller = new AbortController();
    this.runningTools.add(controller);
    this.emit({ type: "tool_execution_start" });
    return {
      signal: controller.signal,
      end: () => {
        if (!this.runningTools.delete(controller)) return;
        this.emit({ type: "tool_execution_end" });
      },
    };
  }

  async prompt(text: string): Promise<void> {
    this.prompts.push(text);
    if (this.promptError) throw this.promptError;
    if (this.rejectPromptWhileStreaming && this.streaming) {
      throw new Error(
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
      );
    }
    this.streaming = true;
    this.emit({ type: "message_start" });
    if (this.autoComplete !== null) {
      this.complete(this.autoComplete);
      return;
    }
    await this.waitForIdle();
  }

  /** Finish the current run, optionally setting the last assistant text. */
  complete(text?: string): void {
    if (text !== undefined) this.lastText = text;
    this.streaming = false;
    this.emit({ type: "agent_end" });
    const waiters = this.idleWaiters.splice(0);
    for (const w of waiters) w();
  }

  /** Emit a mid-run event (stall watchdog food). */
  event(): void {
    this.emit({ type: "message_update" });
  }

  private emit(e: { type: string }): void {
    for (const l of [...this.listeners]) l(e);
  }

  async steer(text: string): Promise<void> {
    this.steers.push(text);
  }

  async followUp(text: string): Promise<void> {
    this.followUps.push(text);
  }

  async abort(): Promise<void> {
    this.aborts++;
    for (const tool of this.runningTools) tool.abort();
    this.runningTools.clear();
    if (this.hungAbort) {
      // The hung stream never unwinds: abort() never resolves.
      await new Promise<never>(() => {});
      return;
    }
    if (!this.abortGateOpen) {
      await new Promise<void>((resolve) => this.abortGateWaiters.push(resolve));
    }
    this.streaming = false;
    const waiters = this.idleWaiters.splice(0);
    for (const w of waiters) w();
  }

  /** Release a pending abort() blocked by abortGateOpen = false. */
  openAbortGate(): void {
    this.abortGateOpen = true;
    const waiters = this.abortGateWaiters.splice(0);
    for (const w of waiters) w();
  }

  async waitForIdle(): Promise<void> {
    if (!this.streaming) return;
    await new Promise<void>((resolve) => this.idleWaiters.push(resolve));
  }

  getLastAssistantText(): string | undefined {
    return this.lastText;
  }

  getLastAssistantFailure() {
    return this.lastAssistantFailure;
  }

  getConversation() {
    const turns = this.prompts.map((text) => ({ role: "user", text }));
    if (this.lastText !== undefined) turns.push({ role: "assistant", text: this.lastText });
    return turns;
  }

  isStreaming(): boolean {
    return this.streaming;
  }

  subscribe(listener: (e: { type: string }) => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  dispose(): void {
    this.disposed = true;
  }
}

/** CreateSessionFn factory recording every created session. */
export class SessionFactory {
  readonly sessions: FakeChildSession[] = [];
  readonly requests: ChildRunRequest[] = [];
  /** Default autoComplete applied to new sessions (null = manual). */
  autoComplete: string | null = "done";
  /** When set, session creation fails with this error. */
  createError: Error | null = null;
  /** Per-request customization hook. */
  configure?: (session: FakeChildSession, req: ChildRunRequest) => void;

  readonly fn: CreateSessionFn = async (req) => {
    this.requests.push(req);
    if (this.createError) throw this.createError;
    const session = new FakeChildSession();
    session.autoComplete = this.autoComplete;
    this.sessions.push(session);
    this.configure?.(session, req);
    return session;
  };
}

/** Flush pending microtasks (let floating promises settle). */
export async function tick(rounds = 20): Promise<void> {
  for (let i = 0; i < rounds; i++) {
    await Promise.resolve();
  }
}
