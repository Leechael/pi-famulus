/**
 * pi runtime bridge (design doc §4.6 "pi runtime isolation rules").
 *
 * This is the ONLY module that loads `@earendil-works/pi-coding-agent` at
 * runtime, and only via dynamic `await import(...)`. Everything else in
 * src/subagent/ is pi-free and testable with fakes.
 *
 * `createPiSessionFn(deps)` returns the CreateSessionFn injected into
 * InProcessRunner: it resolves the model spec against the parent's model
 * registry, creates an in-memory child AgentSession with the agent's tool
 * allowlist plus injected custom tools (child bash, M4 comms), and wraps it
 * into a ChildSessionAdapter.
 *
 * Child sessions discover user resources and built-in extensions normally.
 * A session-local event bus identifies child loaders before the famulus
 * extension initializes, preventing recursive parent-tool registration.
 */
import type {
  ExtensionAPI,
  ExtensionContext,
  ModelRegistry,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { CHILD_BEHAVIOR_GUIDELINES } from "../behavior-guidelines";
import {
  modelResolutionError,
  resolveModelSpec,
  type ModelCandidate,
} from "./model-spec";
import { turnsFromMessages } from "./conversation";
import { createTokenUsageAccumulator, normalizedTokenUsage } from "./usage";
import type { ChildRunRequest, ChildSessionAdapter, ChildSessionEvent, CreateSessionFn } from "./types";

/** Structural subset of the pi module namespace we rely on. */
type PiModule = typeof import("@earendil-works/pi-coding-agent");

type PiAgentSession = Awaited<ReturnType<PiModule["createAgentSession"]>>["session"];

type Model = NonNullable<ExtensionContext["model"]>;

export function createPiUsageAdapter() {
  const totals = createTokenUsageAccumulator();
  return {
    tokenUsage: () => totals.snapshot(),
    adapt(event: {
      type: string;
      toolCallId?: string;
      message?: {
        role?: string;
        usage?: { input?: unknown; output?: unknown; cacheRead?: unknown; cacheWrite?: unknown };
      };
    }): ChildSessionEvent {
      const message = event.message;
      let usage: ReturnType<typeof normalizedTokenUsage> | undefined;
      if (event.type === "message_end" && message?.usage) {
        usage = normalizedTokenUsage(message.usage);
        totals.add(usage);
      }
      return {
        type: event.type,
        ...(typeof event.toolCallId === "string" ? { toolCallId: event.toolCallId } : {}),
        ...(typeof message?.role === "string" ? { role: message.role } : {}),
        ...(usage ? { usage } : {}),
      };
    },
  };
}

export interface PiRuntimeDeps {
  /** Parent session model registry (ctx.modelRegistry). */
  getModelRegistry: () => ModelRegistry | null;
  /**
   * Parent ModelRuntime, including providers registered by other extensions.
   * createAgentSession without this builds a fresh runtime and fails those
   * providers with "No API key found".
   */
  getModelRuntime?: () => unknown;
  /** Parent session current model (ctx.model) — default for children. */
  getParentModel: () => Model | undefined;
  /** Parent session thinking level (ctx.thinkingLevel). */
  getParentThinkingLevel: () => ExtensionContext["thinkingLevel"];
  /**
   * Parent session scoped models (ctx.scopedModels) — the user's whitelist
   * (settings enabledModels / --models). When non-empty, children may only
   * select from these.
   */
  getScopedModels?: () => readonly { model: Model; thinkingLevel?: string }[];
  /** Working directory for child sessions. */
  getCwd: () => string;
  /** Global resource directory; defaults to pi.getAgentDir(). */
  getAgentDir?: () => string;
  /** Preserve the parent's project trust decision. */
  getProjectTrusted?: () => boolean;
  /**
   * Custom tools injected into every child session (e.g. the no-background
   * bash variant, M4 contact_supervisor). Called per child request.
   */
  customTools?: (req: ChildRunRequest) => Array<ToolDefinition<any, any, any>>;
}

/**
 * Candidate set for model selection (§4.6): the scoped whitelist when the
 * user configured one, otherwise every available model. `scoped` marks the
 * source so UIs/errors can say where the list came from.
 */
export function modelCandidates(
  deps: Pick<PiRuntimeDeps, "getModelRegistry" | "getScopedModels">,
): (ModelCandidate & { scoped: boolean })[] {
  const scoped = deps.getScopedModels?.() ?? [];
  if (scoped.length > 0) {
    return scoped.map((s) => ({
      provider: s.model.provider,
      id: s.model.id,
      name: (s.model as { name?: string }).name,
      scoped: true,
    }));
  }
  const registry = deps.getModelRegistry();
  if (!registry) return [];
  return registry.getAvailable().map((m) => ({
    provider: m.provider,
    id: m.id,
    name: (m as { name?: string }).name,
    scoped: false,
  }));
}

let piModulePromise: Promise<PiModule> | null = null;

/** Dynamically import the pi package, with a clear error when unavailable. */
async function importPi(): Promise<PiModule> {
  if (!piModulePromise) {
    piModulePromise = import("@earendil-works/pi-coding-agent").catch((err: unknown) => {
      piModulePromise = null; // allow retry on next call
      const message = err instanceof Error ? err.message : String(err);
      throw new Error(
        `Failed to load @earendil-works/pi-coding-agent for a subagent session: ${message}. ` +
          "The subagent tool requires the pi package to be resolvable at runtime.",
      );
    });
  }
  return piModulePromise;
}

interface ResolvedModel {
  model: Model | undefined;
  /** Thinking level from a ":<level>" spec suffix (overrides agent thinking). */
  thinkingOverride?: string;
  /** Non-fatal caveat to attach to the child result. */
  warning?: string;
}

/**
 * Model priority: subagent() param override > agent definition > parent model.
 * Failure semantics differ by source (§4.6): a bad tool-parameter spec is a
 * hard error (the LLM retries with the candidate list); a bad agent-file spec
 * falls back to the parent model with a warning (user-authored files must not
 * hard-fail across machines).
 */
function resolveModel(deps: PiRuntimeDeps, req: ChildRunRequest): ResolvedModel {
  const candidates = modelCandidates(deps);
  const findModel = (provider: string, id: string): Model | undefined =>
    deps.getModelRegistry()?.find(provider, id) ??
    (deps.getScopedModels?.() ?? []).find(
      (s) => s.model.provider === provider && s.model.id === id,
    )?.model;

  if (req.model !== undefined) {
    const res = resolveModelSpec(req.model, candidates);
    if (!res.ok) throw new Error(modelResolutionError(req.model, res));
    const model = findModel(res.provider, res.id);
    if (!model) throw new Error(`model ${res.provider}/${res.id} is not usable in this session`);
    return { model, thinkingOverride: res.thinking, warning: res.warning };
  }
  if (req.agent.model !== undefined) {
    const res = resolveModelSpec(req.agent.model, candidates);
    if (res.ok) {
      const model = findModel(res.provider, res.id);
      if (model) return { model, thinkingOverride: res.thinking, warning: res.warning };
    }
    return {
      model: deps.getParentModel(),
      warning: `agent "${req.agent.name}" model "${req.agent.model}" unavailable; fell back to the parent model`,
    };
  }
  return { model: deps.getParentModel() };
}

/** Wrap a real AgentSession into the pi-free ChildSessionAdapter. */
function wrapSession(
  session: PiAgentSession,
  extras: { warning?: string; resolvedModel?: string; effectiveThinkingLevel?: string } = {},
): ChildSessionAdapter {
  const usageAdapter = createPiUsageAdapter();
  let disposal: Promise<void> | undefined;
  // Children receive all queued steering messages in one drain/turn rather
  // than the interactive one-at-a-time default, which can strand wake bursts.
  session.setSteeringMode("all");
  return {
    ...(extras.warning !== undefined ? { warning: extras.warning } : {}),
    ...(extras.resolvedModel !== undefined ? { resolvedModel: extras.resolvedModel } : {}),
    ...(extras.effectiveThinkingLevel !== undefined ? { effectiveThinkingLevel: extras.effectiveThinkingLevel } : {}),
    // pi ≥0.99 returns QueuedInputDisposition ("handled" | "queued") from
    // input methods; the adapter contract stays Promise<void>.
    prompt: async (text) => { await session.prompt(text); },
    steer: async (text) => { await session.steer(text); },
    followUp: async (text) => { await session.followUp(text); },
    abort: () => session.abort(),
    waitForIdle: () => session.waitForIdle(),
    getLastAssistantText: () => session.getLastAssistantText(),
    getLastAssistantFailure: () => {
      const lastAssistant = [...session.messages].reverse().find((message) => message.role === "assistant") as
        | { stopReason?: string; errorMessage?: string }
        | undefined;
      if (lastAssistant?.stopReason !== "error" && lastAssistant?.stopReason !== "aborted") return undefined;
      return {
        stopReason: lastAssistant.stopReason,
        ...(lastAssistant.errorMessage ? { errorMessage: lastAssistant.errorMessage } : {}),
      };
    },
    getConversation: () => turnsFromMessages(session.messages),
    tokenUsage: () => usageAdapter.tokenUsage(),
    getActiveToolNames: () => session.getActiveToolNames(),
    getSystemPrompt: () => session.systemPrompt,
    isStreaming: () => session.isStreaming,
    getSteeringMode: () => session.steeringMode,
    subscribe: (listener) => session.subscribe((event) => {
      listener(usageAdapter.adapt(event as Parameters<typeof usageAdapter.adapt>[0]));
    }),
    dispose: () => disposal ??= (async () => {
      try {
        await session.abort();
        await session.extensionRunner.emit({ type: "session_shutdown", reason: "quit" });
      } finally {
        session.dispose();
      }
    })(),
  };
}

const CHILD_ROLE_CHANNEL = "pi-famulus:child-role";

/** The loader's bus answers synchronously, before any extension initialization. */
export function isFamulusChildSession(pi: Pick<ExtensionAPI, "events">): boolean {
  const probe = { child: false };
  pi.events?.emit(CHILD_ROLE_CHANNEL, probe);
  return probe.child;
}

/** Keep configuration scopes and paths intact; never persist child overrides. */
export async function createChildResources(input: {
  cwd: string;
  agentDir: string;
  projectTrusted: boolean;
}) {
  const pi = await importPi();
  const settingsManager = pi.SettingsManager.create(input.cwd, input.agentDir, {
    projectTrusted: input.projectTrusted,
  });
  const eventBus = pi.createEventBus();
  eventBus.on(CHILD_ROLE_CHANNEL, (probe) => {
    if (probe && typeof probe === "object" && "child" in probe) probe.child = true;
  });
  const resourceLoader = new pi.DefaultResourceLoader({
    cwd: input.cwd,
    agentDir: input.agentDir,
    settingsManager,
    eventBus,
    extensionFactories: [
      { name: "codemode", factory: pi.createCodemodeExtension(), builtin: true, replaceable: true },
      { name: "tool-search", factory: pi.createToolSearchExtension(), builtin: true, replaceable: true },
      { name: "mcp", factory: pi.createMcpExtension(), builtin: true, replaceable: true },
    ],
  });
  await resourceLoader.reload();
  return { resourceLoader, settingsManager };
}

export function childSessionCreateOptions(input: {
  cwd: string;
  model: unknown;
  thinkingLevel: unknown;
  tools?: string[];
  customTools?: unknown[];
  modelRuntime?: unknown;
  resourceLoader?: unknown;
}): {
  cwd: string;
  model: unknown;
  thinkingLevel: unknown;
  tools?: string[];
  excludeTools?: string[];
  customTools?: unknown[];
  modelRuntime?: unknown;
  resourceLoader?: unknown;
} {
  const tools = input.tools !== undefined ? [...input.tools] : undefined;
  const customTools = input.customTools?.filter((customTool) => {
    const name = (customTool as { name?: unknown }).name;
    return name !== "bash" || tools === undefined || tools.includes("bash");
  }).map((customTool) => {
    const tool = customTool as { name?: unknown; promptGuidelines?: string[] };
    if (tool.name === "bash") return { ...tool, defaultActive: false };
    if (tool.name === "contact_supervisor") {
      return {
        ...tool,
        promptGuidelines: [...(tool.promptGuidelines ?? []), CHILD_BEHAVIOR_GUIDELINES],
      };
    }
    return customTool;
  });
  if (tools && customTools) {
    for (const customTool of customTools) {
      const name = (customTool as { name?: unknown }).name;
      if (typeof name === "string" && !tools.includes(name)) tools.push(name);
    }
  }
  return {
    cwd: input.cwd,
    model: input.model,
    thinkingLevel: input.thinkingLevel,
    ...(tools !== undefined ? { tools } : {}),
    // pi 1.0.4+ otherwise retains MCP tools behind non-MCP allowlists.
    ...(tools !== undefined && !tools.some((name) => name.startsWith("mcp__"))
      ? { excludeTools: ["mcp__*"] }
      : {}),
    ...(customTools && customTools.length > 0 ? { customTools } : {}),
    ...(input.modelRuntime ? { modelRuntime: input.modelRuntime } : {}),
    ...(input.resourceLoader ? { resourceLoader: input.resourceLoader } : {}),
  };
}

/**
 * Build the CreateSessionFn for InProcessRunner. All deps are getters so
 * model/cwd changes in the parent session are picked up per child.
 */
export function createPiSessionFn(deps: PiRuntimeDeps): CreateSessionFn {
  return async (req: ChildRunRequest): Promise<ChildSessionAdapter> => {
    const pi = await importPi();
    const resolved = resolveModel(deps, req);
    const thinkingLevel =
      (resolved.thinkingOverride as ExtensionContext["thinkingLevel"]) ??
      req.agent.thinking ??
      deps.getParentThinkingLevel();
    const cwd = deps.getCwd();
    const agentDir = deps.getAgentDir?.() ?? pi.getAgentDir();
    const { resourceLoader, settingsManager } = await createChildResources({
      cwd,
      agentDir,
      projectTrusted: deps.getProjectTrusted?.() ?? false,
    });
    const customTools = deps.customTools?.(req) ?? [];
    const modelRuntime = deps.getModelRuntime?.();
    const options = childSessionCreateOptions({
      cwd,
      model: resolved.model,
      thinkingLevel,
      tools: req.agent.tools,
      customTools: customTools.length > 0 ? customTools : undefined,
      modelRuntime,
      resourceLoader,
    });
    const { session } = await pi.createAgentSession({
      ...options,
      agentDir,
      settingsManager,
      sessionManager: pi.SessionManager.inMemory(cwd),
    } as never);
    await session.bindExtensions({});
    const resolvedModel = resolved.model
      ? `${resolved.model.provider}/${resolved.model.id}`
      : undefined;
    return wrapSession(session, {
      ...(resolved.warning !== undefined ? { warning: resolved.warning } : {}),
      ...(resolvedModel !== undefined ? { resolvedModel } : {}),
      ...(typeof thinkingLevel === "string" ? { effectiveThinkingLevel: thinkingLevel } : {}),
    });
  };
}
