/**
 * ChildSessionAdapter over a daemon-hosted `pi --mode rpc` process.
 * The runner's timers stay; only the session lives in another process.
 */
import { randomUUID } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ManagerClient, ManagerEvent } from "../manager-client";
import { parentResourceArgs, resolvePiArgv } from "../pi-command";
import { resolveChildModel, type PiRuntimeDeps } from "./pi-runtime";
import type {
  ChildRunRequest,
  ChildSessionAdapter,
  ChildSessionEvent,
  ChildTokenUsage,
  CreateSessionFn,
} from "./types";

export interface HostedSessionDeps extends PiRuntimeDeps {
  getClient: () => ManagerClient | null;
  getHome: () => string;
  /** This extension's entry file, passed to the child with -e. */
  extensionEntry: string;
  piCommand?: string | null;
  trustProject?: boolean;
}

interface HostingView {
  residency: string;
  activity: string;
  driver: string;
  cacheExpiresAt?: number;
  sessionFile?: string;
  sessionId?: string;
}

/**
 * One prompt whose turn the parent supervises.
 *
 * A prompt is not the same as its RPC acknowledgement: the daemon's cold
 * `get_state` handshake for a respawned parked child reports `idle` before the
 * prompt has run, and events may arrive before the RPC response. Completion is
 * therefore fenced by this turn's own `agent_start`/`agent_settled` lifecycle,
 * never by transport readiness.
 */
interface ActiveTurn {
  /** Resolves when this prompt's run lifecycle ends (or it was handled). */
  done: Promise<void>;
  resolve: () => void;
  /** The daemon's RPC response for the dispatch was processed. */
  accepted: boolean;
  /** `disposition` from that response (`started` | `queued` | `handled`). */
  disposition: string | undefined;
  /** A run for this turn is active (observed `agent_start`, or joined live). */
  sawStart: boolean;
  /** An `agent_settled` was observed and no newer run started since. */
  settled: boolean;
  /** The hosted process reported failure before the turn ended. */
  processFailed: boolean;
  /** The prompt was steered into a run already in flight (out-of-band work). */
  joinedRun: boolean;
}

export function createHostedSessionFn(deps: HostedSessionDeps): CreateSessionFn {
  return async (req) => {
    const client = deps.getClient();
    if (!client || !(await client.ensureAvailable())) {
      throw new Error("pi-famulus daemon is not connected; a subagent needs it to run pi --mode rpc.");
    }
    client.requireHostedProtocol();
    const pi = resolvePiArgv(deps.piCommand);
    if ("error" in pi) throw new Error(pi.error);
    const model = resolveChildModel(deps, req);
    const thinking =
      model.thinkingOverride ?? req.agent.thinking ?? deps.getParentThinkingLevel() ?? "off";
    const cwd = deps.getCwd();
    const promptPath = join(deps.getHome(), "agents", "prompts");
    mkdirSync(promptPath, { recursive: true });
    const promptFile = join(promptPath, `${req.childId}.md`);
    writeFileSync(promptFile, req.agent.systemPrompt);
    const inherited = parentResourceArgs();
    const extensionPaths = inherited.filter((_, i) => inherited[i - 1] === "-e");
    const alreadyLoadsUs = extensionPaths.some((path) => path === deps.extensionEntry);
    const trustInherited = inherited.some((arg) => arg === "-a" || arg === "--approve" || arg === "-na" || arg === "--no-approve");
    const argv = [
      ...pi.argv,
      "--mode", "rpc",
      "--session-id", req.childId,
      "--name", `${req.agent.name}: ${req.name}`,
      ...inherited,
      ...(alreadyLoadsUs ? [] : ["-e", deps.extensionEntry]),
      ...(model.model ? ["--model", `${model.model.provider}/${model.model.id}`] : []),
      "--thinking", String(thinking),
      "--append-system-prompt", promptFile,
      ...(trustInherited ? [] : [deps.getProjectTrusted?.() ? "-a" : "-na"]),
      ...(req.agent.tools ? ["--tools", hostedChildTools(req.agent.tools).join(",")] : []),
    ];
    const env = { ...process.env } as Record<string, string>;
    if (req.agent.tools) env.PI_FAMULUS_CHILD_TOOLS = hostedChildTools(req.agent.tools).join(",");
    env.PI_FAMULUS_CHILD_RUN = req.runId;
    env.PI_FAMULUS_CHILD_NAME = req.name;
    const spawned = await client.agentSpawn({
      childId: req.childId,
      runId: req.runId,
      name: req.name,
      argv,
      cwd,
      env,
    });
    const warning = model.warning;
    await client.agentWatch(req.childId);
    const session = new HostedRpcSession(client, req.childId, {
      warning,
      resolvedModel: model.model ? `${model.model.provider}/${model.model.id}` : undefined,
      effectiveThinkingLevel: typeof thinking === "string" ? thinking : undefined,
      sessionFile: typeof spawned.session_file === "string" ? spawned.session_file : undefined,
      sessionId: typeof spawned.session_id === "string" ? spawned.session_id : req.childId,
    });
    return session;
  };
}

class HostedRpcSession implements ChildSessionAdapter {
  readonly warning?: string;
  readonly resolvedModel?: string;
  readonly effectiveThinkingLevel?: string;
  private readonly listeners = new Set<(event: ChildSessionEvent) => void>();
  private readonly unsubscribe: () => void;
  private streaming = false;
  private lastText = "";
  private lastFailure: { stopReason: "error" | "aborted"; errorMessage?: string } | undefined;
  private turns: { role: string; text: string }[] = [];
  private usage: ChildTokenUsage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  /** The prompt turn awaiting its own completion; null between turns. */
  private activeTurn: ActiveTurn | null = null;
  private hostingView: HostingView;
  private disposed = false;

  constructor(
    private readonly client: ManagerClient,
    private readonly childId: string,
    init: { warning?: string; resolvedModel?: string; effectiveThinkingLevel?: string; sessionFile?: string; sessionId?: string },
  ) {
    this.warning = init.warning;
    this.resolvedModel = init.resolvedModel;
    this.effectiveThinkingLevel = init.effectiveThinkingLevel;
    this.hostingView = {
      residency: "warm",
      activity: "starting",
      driver: "parent",
      sessionFile: init.sessionFile,
      sessionId: init.sessionId,
    };
    this.unsubscribe = this.client.onEvent((event) => this.onManagerEvent(event as ManagerEvent & Record<string, unknown>));
  }

  hosting(): HostingView {
    return { ...this.hostingView };
  }

  async prompt(text: string): Promise<void> {
    // A run already in flight is not ours: a user/UI attach or a sibling drove
    // it. Steering delivers this prompt into that run, and its settle is this
    // turn's completion — returning early would report the child finished while
    // the run is still executing.
    const joinedRun = this.streaming;
    // A new prompt supersedes any turn still armed (defensive: the runner
    // serializes generations, so this should not happen).
    if (this.activeTurn) this.finishTurn(this.activeTurn);
    const turn = this.beginTurn(joinedRun);
    try {
      const response = await this.client.agentInput(
        this.childId,
        joinedRun ? "prompt_steer" : "prompt",
        text,
        "parent",
      );
      this.acceptPrompt(turn, response);
    } catch (err) {
      this.finishTurn(turn);
      throw err;
    }
    await turn.done;
  }

  async steer(text: string): Promise<void> {
    await this.client.agentInput(this.childId, "prompt_steer", text, "parent");
  }

  async followUp(text: string): Promise<void> {
    await this.client.agentInput(this.childId, "prompt_follow", text, "parent");
  }

  async abort(): Promise<void> {
    try {
      await this.client.agentInput(this.childId, "abort", "", "parent");
    } finally {
      // The abort response is sent after the child became idle, so the turn can
      // end now; its `agent_settled` is already drained or no longer relevant.
      // A failed abort (dead child) must not leave the turn supervised forever.
      this.streaming = false;
      if (this.activeTurn) this.finishTurn(this.activeTurn);
    }
  }

  async park(): Promise<void> {
    await this.client.agentPark(this.childId);
    this.hostingView.residency = "parked";
  }

  async waitForIdle(): Promise<void> {
    const turn = this.activeTurn;
    if (!turn || !this.streaming) return;
    await turn.done;
  }

  getLastAssistantText(): string | undefined {
    return this.lastText || undefined;
  }

  getLastAssistantFailure(): { stopReason: "error" | "aborted"; errorMessage?: string } | undefined {
    return this.lastFailure;
  }

  getConversation() {
    return this.turns.map((turn) => ({ ...turn }));
  }

  tokenUsage(): ChildTokenUsage {
    return { ...this.usage };
  }

  isStreaming(): boolean {
    return this.streaming;
  }

  subscribe(listener: (event: ChildSessionEvent) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.unsubscribe();
    if (this.activeTurn) this.finishTurn(this.activeTurn);
  }

  private beginTurn(joinedRun: boolean): ActiveTurn {
    let resolve!: () => void;
    const done = new Promise<void>((res) => {
      resolve = res;
    });
    // Failure/result state is turn-scoped: the previous turn's abort error must
    // not describe this one.
    this.lastFailure = undefined;
    const turn: ActiveTurn = {
      done,
      resolve,
      accepted: false,
      disposition: undefined,
      sawStart: joinedRun,
      settled: false,
      processFailed: false,
      joinedRun,
    };
    this.activeTurn = turn;
    return turn;
  }

  /** Apply the prompt RPC's disposition, then wait or finish accordingly. */
  private acceptPrompt(turn: ActiveTurn, response: Record<string, unknown>): void {
    if (this.activeTurn !== turn) return;
    turn.accepted = true;
    turn.disposition = typeof response.disposition === "string" ? response.disposition : undefined;
    if (turn.disposition === "handled" && !turn.joinedRun) {
      // An extension command or input handler consumed the prompt: no run
      // starts for it, so there is nothing to wait for. A handled prompt that
      // was steered into a live run still waits for that run to end.
      this.streaming = false;
      this.finishTurn(turn);
      return;
    }
    if (turn.disposition === "queued") {
      // The prompt joined a run that was already streaming (our view said
      // idle). That run's `agent_settled` is this turn's completion.
      turn.sawStart = true;
    }
    if (turn.processFailed) {
      this.lastFailure = { stopReason: "error", errorMessage: "hosted child process failed" };
      this.finishTurn(turn);
      return;
    }
    this.maybeFinishTurn(turn);
  }

  private maybeFinishTurn(turn: ActiveTurn): void {
    if (this.activeTurn !== turn || !turn.accepted || !turn.settled) return;
    // `started` (or a legacy response without a disposition) must have shown a
    // run start after this turn was armed: a stale `agent_settled` from the
    // previous generation is not this prompt's completion.
    if (turn.disposition !== "queued" && !turn.sawStart) return;
    this.finishTurn(turn);
  }

  private finishTurn(turn: ActiveTurn): void {
    if (this.activeTurn !== turn) return;
    this.activeTurn = null;
    this.streaming = false;
    turn.resolve();
  }

  private onManagerEvent(event: ManagerEvent & Record<string, unknown>): void {
    if (event.child_id !== this.childId) return;
    if (event.event === "agent_state") {
      this.hostingView = {
        residency: String(event.residency ?? this.hostingView.residency),
        activity: String(event.activity ?? this.hostingView.activity),
        driver: String(event.driver ?? this.hostingView.driver),
        cacheExpiresAt: typeof event.cache_expires_at === "number" ? event.cache_expires_at : this.hostingView.cacheExpiresAt,
        sessionFile: typeof event.session_file === "string" ? event.session_file : this.hostingView.sessionFile,
        sessionId: typeof event.session_id === "string" ? event.session_id : this.hostingView.sessionId,
      };
      if (event.activity === "running") this.streaming = true;
      if (event.activity === "idle") {
        // Transport/process readiness, not turn completion: a respawned parked
        // child reports idle from its cold get_state handshake before the
        // resumed prompt has run. Completion comes from `agent_settled`.
        this.streaming = false;
        return;
      }
      if (event.activity === "failed") {
        this.streaming = false;
        // Explicit process failure is a real terminal signal even before the
        // run starts (the daemon filters stale generations). Before the RPC
        // response the failure is held on the turn and applied on acceptance,
        // so an accepted prompt cannot wait forever for a settle that will
        // never come.
        const turn = this.activeTurn;
        if (turn) {
          turn.processFailed = true;
          if (turn.accepted) {
            this.lastFailure = { stopReason: "error", errorMessage: "hosted child process failed" };
            this.finishTurn(turn);
          }
        }
      }
      return;
    }
    if (event.event !== "agent_rpc" || !event.record || typeof event.record !== "object") return;
    const record = event.record as {
      type?: string;
      message?: {
        role?: string;
        content?: { type?: string; text?: string }[];
        usage?: Record<string, unknown>;
        stopReason?: string;
        errorMessage?: string;
      };
      toolCallId?: string;
      toolName?: string;
      args?: unknown;
      isError?: boolean;
    };
    const type = record.type ?? "";
    if (type === "agent_start") {
      this.streaming = true;
      const turn = this.activeTurn;
      if (turn) {
        turn.sawStart = true;
        // A run that starts after a settle means the record was not terminal
        // for this prompt (Pi is still working), so it cannot close the turn.
        turn.settled = false;
      }
    }
    if (type === "agent_settled") {
      this.streaming = false;
      const turn = this.activeTurn;
      if (turn) {
        turn.settled = true;
        this.maybeFinishTurn(turn);
      }
    }
    // Content bookkeeping belongs only to work the parent is supervising: a
    // late record from a drained generation must not rewrite this turn.
    const owned = this.streaming || this.activeTurn?.sawStart === true;
    if (owned && type === "message_end" && record.message?.role === "assistant") {
      const text = (record.message.content ?? []).map((part) => part.text ?? "").join("");
      if (text) {
        this.lastText = text;
        this.turns.push({ role: "assistant", text });
      }
      const stopReason = record.message.stopReason;
      if (stopReason === "error" || stopReason === "aborted") {
        this.lastFailure = {
          stopReason,
          ...(record.message.errorMessage ? { errorMessage: record.message.errorMessage } : {}),
        };
        if (record.message.errorMessage) {
          this.turns.push({ role: "assistant error", text: record.message.errorMessage });
        }
      } else {
        this.lastFailure = undefined;
      }
      const usage = record.message.usage;
      if (usage) {
        this.usage.input += num(usage.input);
        this.usage.output += num(usage.output);
        this.usage.cacheRead += num(usage.cacheRead);
        this.usage.cacheWrite += num(usage.cacheWrite);
      }
    }
    if (owned && type === "message_end" && record.message?.role === "user") {
      const text = (record.message.content ?? []).map((part) => part.text ?? "").join("");
      if (text) this.turns.push({ role: "user", text });
    }
    if (owned && type === "tool_execution_end") {
      const name = record.toolName ?? "tool";
      const text = record.args === undefined ? name : `${name} ${JSON.stringify(record.args)}`;
      this.turns.push({
        role: record.isError ? `tool ${name} error` : `tool ${name}`,
        text,
      });
    }
    const adapted: ChildSessionEvent = {
      type,
      ...(record.message?.role ? { role: record.message.role } : {}),
      ...(record.toolCallId ? { toolCallId: record.toolCallId } : {}),
      ...(record.toolName ? { toolName: record.toolName } : {}),
      ...(record.args !== undefined ? { args: record.args } : {}),
      ...(record.isError !== undefined ? { isError: record.isError } : {}),
    };
    for (const listener of this.listeners) listener(adapted);
  }
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

const SUPERVISOR_TOOLS = ["contact_supervisor", "agent_message"];

/** Agent allowlists still keep the two supervisor tools. */
export function hostedChildTools(tools: string[]): string[] {
  const kept = new Set(tools);
  for (const name of SUPERVISOR_TOOLS) kept.add(name);
  return [...kept];
}

/** Request id for a child→parent decision. */
export function newCommsRequestId(): string {
  return `rq_${randomUUID().slice(0, 8)}`;
}
