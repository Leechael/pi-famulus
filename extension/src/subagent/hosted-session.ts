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

export function createHostedSessionFn(deps: HostedSessionDeps): CreateSessionFn {
  return async (req) => {
    const client = deps.getClient();
    if (!client || !client.isAvailable()) {
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
    let warning = model.warning;
    try {
      await client.agentInput(req.childId, "set_steering_mode", "all", "parent");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const note = `Steering mode was not set (${message}); queued input may wait until the current turn ends.`;
      warning = warning ? `${warning} ${note}` : note;
    }
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
  private settle: (() => void) | null = null;
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
    const done = this.armSettle();
    const op = this.streaming ? "prompt_steer" : "prompt";
    await this.client.agentInput(this.childId, op, text, "parent");
    if (!this.streaming) await done;
  }

  async steer(text: string): Promise<void> {
    await this.client.agentInput(this.childId, "prompt_steer", text, "parent");
  }

  async followUp(text: string): Promise<void> {
    await this.client.agentInput(this.childId, "prompt_follow", text, "parent");
  }

  async abort(): Promise<void> {
    await this.client.agentInput(this.childId, "abort", "", "parent");
    this.streaming = false;
    this.settle?.();
    this.settle = null;
  }

  async park(): Promise<void> {
    await this.client.agentPark(this.childId);
    this.hostingView.residency = "parked";
  }

  async waitForIdle(): Promise<void> {
    if (!this.streaming) return;
    await this.armSettle();
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
    this.settle?.();
    this.settle = null;
  }

  private armSettle(): Promise<void> {
    return new Promise((resolve) => {
      this.settle = resolve;
    });
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
      if (event.activity === "idle" || event.activity === "failed") {
        if (event.activity === "failed" && !this.lastFailure) {
          this.lastFailure = { stopReason: "error", errorMessage: "hosted child process failed" };
        }
        this.streaming = false;
        this.settle?.();
        this.settle = null;
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
    if (type === "agent_start") this.streaming = true;
    if (type === "agent_settled") {
      this.streaming = false;
      this.settle?.();
      this.settle = null;
    }
    if (type === "message_end" && record.message?.role === "assistant") {
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
      }
      const usage = record.message.usage;
      if (usage) {
        this.usage.input += num(usage.input);
        this.usage.output += num(usage.output);
        this.usage.cacheRead += num(usage.cacheRead);
        this.usage.cacheWrite += num(usage.cacheWrite);
      }
    }
    if (type === "message_end" && record.message?.role === "user") {
      const text = (record.message.content ?? []).map((part) => part.text ?? "").join("");
      if (text) this.turns.push({ role: "user", text });
    }
    const adapted: ChildSessionEvent = {
      type,
      ...(record.message?.role ? { role: record.message.role } : {}),
      ...(record.toolCallId ? { toolCallId: record.toolCallId } : {}),
    };
    for (const listener of this.listeners) listener(adapted);
  }
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

const SUPERVISOR_TOOLS = ["contact_supervisor", "agent_message"];

/** Agent allowlists still keep the two supervisor tools (I-55). */
export function hostedChildTools(tools: string[]): string[] {
  const kept = new Set(tools);
  for (const name of SUPERVISOR_TOOLS) kept.add(name);
  return [...kept];
}

/** Request id for a child→parent decision. */
export function newCommsRequestId(): string {
  return `rq_${randomUUID().slice(0, 8)}`;
}
