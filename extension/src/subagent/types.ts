/**
 * M3 subagent core types (design doc Appendix B).
 *
 * This module is pi-free: no runtime dependency on
 * `@earendil-works/pi-coding-agent` (and no type imports either), so every
 * consumer stays testable with plain fakes.
 */

/**
 * Mirror of the M5 `AgentDefinition` (design doc Appendix B, src/agents/).
 * Structurally compatible with the M5 definition, so the real agent loader
 * can be wired in at integration time without changes here.
 */
export interface AgentDefinition {
  name: string; // ^[a-z][a-z0-9-]*$
  description: string; // required, non-empty
  tools: string[]; // default ["read","bash","edit","write"]
  model?: string; // "provider:id" | bare id
  // Mirror of src/agents/definition.ts — keep in sync with
  // src/thinking-levels.ts (VALID_THINKING_LEVELS), the single source.
  thinking?: "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
  systemPrompt: string; // frontmatter body, trimmed
  source: "builtin" | "user" | "project";
  path?: string; // builtins have no path
}

export type ChildStatus = "pending" | "running" | "completed" | "failed" | "interrupted";

export const AGENT_WORK_KINDS = [
  "test-suite",
  "test",
  "build",
  "lint/type",
  "other",
  "git",
  "read/search",
] as const;
export type AgentWorkKind = (typeof AGENT_WORK_KINDS)[number];

export interface ChildResult {
  status: "completed" | "failed" | "interrupted";
  text: string; // getLastAssistantText() or "(no output)"
  error?: string;
  /** Non-fatal caveat, e.g. agent-definition model fell back to parent model. */
  warning?: string;
  /** Set when the model itself stopped with an error (provider failure). */
  endReason?: "model-error";
  /**
   * Total generations run (1 + resumes + stall retries). Omitted when 1.
   * Note: a user resume() also increments this; use `stalls` to count
   * stall retries specifically.
   */
  attempts?: number;
  /** Stall detections in the user turn that produced this result. */
  stalls?: number;
  /** Time waiting for local and machine-wide admission in this turn. */
  queueMs?: number;
  durationMs: number;
}

/** One turn of a child session, for the /tasks conversation view. */
export interface ConversationTurn {
  role: string;
  text: string;
}

export interface ChildRunRequest {
  childId: string; // assigned by the registry: "ch_" + 8
  runId: string; // "run_" + 8
  name: string; // display name (tasks[].name or agent name or ordinal)
  prompt: string; // actual first prompt sent to the session, including agent preamble
  /** User-authored task prompt without agent or model preamble. */
  taskPrompt?: string;
  agent: AgentDefinition; // already resolved
  model?: string; // subagent() parameter-level override
  /** Explicit planned workload class; natural-language prompts are not classified. */
  workKind?: AgentWorkKind;
  /** Soft budget per turn: reaching it wakes the parent; it does not abort. */
  timeoutMs: number;
  depth: number; // main session = 0, child = 1
}

export interface ChildHandle {
  readonly childId: string;
  readonly result: Promise<ChildResult>; // resolves exactly once per generation;
  // after resume() the new generation's promise is exposed via
  // registry.getResult() (and via this getter, which always returns the
  // current generation's promise).
  steer(message: string): Promise<void>; // while running; terminal -> throw
  followUp(message: string): Promise<void>; // same, queued delivery
  /** terminal -> continue running; opts.timeoutMs is the new turn's soft budget (default: spawn budget). */
  resume(message: string, opts?: { timeoutMs?: number }): Promise<void>;
  /**
   * running -> soft deadline = now + timeoutMs (default: spawn budget);
   * otherwise throws. Returns the new deadline and the unmoved hard ceiling.
   */
  extend(timeoutMs?: number): { deadlineAt: number; hardDeadlineAt: number | null };
  interrupt(): Promise<void>; // abort; result resolves as interrupted
  status(): ChildStatus;
  lastEventAt(): number; // for the stall watchdog / status display
  /** Resolved `provider/id` once the child session has been constructed. */
  resolvedModel(): string | undefined;
  /** Live child transcript. Empty when the session never started. */
  conversation(): ConversationTurn[];
  /** Cumulative provider-reported input/output tokens for this child. */
  tokenUsage(): { input: number; output: number };
}

/**
 * Child session adapter — the dynamic-import product of pi-runtime.ts is
 * wrapped into this interface; tests use fakes.
 */
export interface ChildSessionAdapter {
  /** Non-fatal setup caveat (e.g. model fallback); copied to ChildResult. */
  readonly warning?: string;
  /**
   * Resolved model label (`provider/id`) after session construction.
   * Used for fleet/ls persistence and so the child prompt can name its model.
   */
  readonly resolvedModel?: string;
  prompt(text: string): Promise<void>;
  steer(text: string): Promise<void>;
  followUp(text: string): Promise<void>;
  abort(): Promise<void>;
  waitForIdle(): Promise<void>;
  getLastAssistantText(): string | undefined;
  /** Provider/model failure outcome of the last assistant turn, when any. */
  getLastAssistantFailure?(): { stopReason: "error" | "aborted"; errorMessage?: string } | undefined;
  getConversation(): ConversationTurn[];
  tokenUsage(): { input: number; output: number };
  /** Introspection for child-session orchestration and contract tests. */
  getActiveToolNames?(): string[];
  getSystemPrompt?(): string;
  isStreaming(): boolean;
  subscribe(listener: (event: { type: string; usage?: { input: number; output: number } }) => void): () => void;
  dispose(): void;
}

export type CreateSessionFn = (req: ChildRunRequest) => Promise<ChildSessionAdapter>;

export interface ChildRunner {
  start(req: ChildRunRequest): Promise<ChildHandle>;
}

/**
 * Extension beyond Appendix B: handles produced by InProcessRunner own a
 * live session that must be disposed on run/session teardown. The registry
 * duck-types this to release sessions in disposeRun().
 */
export interface DisposableChildHandle extends ChildHandle {
  dispose(): void;
}
