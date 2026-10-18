/**
 * pi-famulus inside a hosted subagent (`PI_FAMULUS_CHILD_ID`).
 * Registers only the child tools and reports prompt-cache expiry. A second
 * copy of the extension in the same process (installed package plus `-e`)
 * returns immediately.
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { randomUUID } from "node:crypto";
import { CHILD_BEHAVIOR_GUIDELINES } from "./behavior-guidelines";
import { createChildBashTool } from "./subagent/child-bash";
import { ChildShellTracker } from "./subagent/overrun";
import { createContactSupervisorTool, createAgentMessageTool } from "./comms/tools";
import { createComms } from "./comms/comms";
import { getFamulusHome, loadConfig, resolveManagerPath } from "./config";
import { ManagerClient } from "./manager-client";
import { realClock } from "./clock";

const MARKER = Symbol.for("pi-famulus.child-mode");

export function isHostedChildProcess(): boolean {
  return Boolean(process.env.PI_FAMULUS_CHILD_ID && process.env.PI_FAMULUS_CHILD_TOKEN);
}

export function registerChildMode(pi: ExtensionAPI): void {
  const globalState = globalThis as typeof globalThis & { [MARKER]?: boolean };
  if (globalState[MARKER]) return;
  globalState[MARKER] = true;
  const childId = process.env.PI_FAMULUS_CHILD_ID ?? "";
  const token = process.env.PI_FAMULUS_CHILD_TOKEN ?? "";
  const runId = process.env.PI_FAMULUS_CHILD_RUN ?? "run_hosted";
  const home = process.env.PI_FAMULUS_HOME || getFamulusHome();
  const config = loadConfig(home);
  const managerPath = resolveManagerPath(config, home);
  let client: ManagerClient | null = null;
  let ctx: ExtensionContext | null = null;
  let stopWarming = false;
  const shells = new ChildShellTracker();
  const allow = process.env.PI_FAMULUS_CHILD_TOOLS?.split(",").filter(Boolean);
  const bashAllowed = !allow || allow.includes("bash");

  const comms = createComms({
    getChild: () => undefined,
    listChildren: () => [],
    sameRun: () => true,
    notifySupervisor: () => {},
  });

  pi.registerTool({
    ...createContactSupervisorTool({
      ...comms,
      contactSupervisor: async (_from: string, reason: "need_decision" | "progress_update", message: string) => {
        if (!client) throw new Error("pi-famulus daemon is not connected");
        return client.agentComms(randomUUID(), reason, message);
      },
    } as never, childId),
    promptGuidelines: [CHILD_BEHAVIOR_GUIDELINES],
  });

  pi.registerTool(createAgentMessageTool(comms, { kind: "child", childId, runId }, {
    getChild: (id) => ({
      handle: {
        childId: id,
        result: Promise.resolve({ status: "completed", text: "", durationMs: 0 }),
        steer: async (message: string) => { await client?.agentInput(id, "prompt_steer", message, childId); },
        followUp: async (message: string) => { await client?.agentInput(id, "prompt_follow", message, childId); },
        resume: async (message: string) => { await client?.agentInput(id, "prompt", message, childId); },
        interrupt: async () => {},
        status: () => "running" as const,
        lastEventAt: () => Date.now(),
      },
      runId,
      name: id,
      status: "running" as const,
    }),
    listChildren: () => [],
    sameRun: () => true,
    notifySupervisor: () => {},
  }, realClock));

  if (bashAllowed) {
    pi.registerTool(createChildBashTool({
      getClient: () => client,
      home,
      sessionId: () => ctx?.sessionManager.getSessionId() ?? childId,
      sessionEnv: () => ({ ...process.env }) as Record<string, string>,
      trackTask: () => {},
      childId,
      runId,
      clock: realClock,
      shells,
    }));
  }

  pi.on("session_start", async (_event, startCtx) => {
    ctx = startCtx;
    client = new ManagerClient({
      home,
      sessionId: startCtx.sessionManager.getSessionId(),
      managerPath,
      piPid: process.pid,
      cwd: startCtx.cwd,
      role: "child",
      childId,
      token,
      sessionFile: startCtx.sessionManager.getSessionFile?.(),
    });
    await client.connect().catch(() => {});
    client.onEvent((event) => {
      const hosted = event as { event?: string; residency?: string };
      if (hosted.event === "agent_state" && hosted.residency === "parked") stopWarming = true;
    });
  });

  pi.on("message_end", async (event) => {
    if (!client?.isAvailable()) return;
    const message = (event as { message?: { role?: string; usage?: { cacheWrite1h?: number } } }).message;
    if (message?.role !== "assistant") return;
    const model = ctx?.model as { promptCache?: { short?: number; long?: number } } | undefined;
    const long = process.env.PI_CACHE_RETENTION === "long" || (message.usage?.cacheWrite1h ?? 0) > 0;
    const seconds = long ? model?.promptCache?.long : model?.promptCache?.short;
    if (!seconds || seconds <= 0) {
      await client.agentCache(null, null, null).catch(() => {});
      return;
    }
    await client.agentCache(Date.now() + seconds * 1000, long ? "long" : "short", seconds * 1000).catch(() => {});
  });

  pi.on("cache_warming_decision", () => {
    if (stopWarming) return { action: "stop" as const };
    return undefined;
  });

  pi.on("session_shutdown", async () => {
    await client?.close();
    client = null;
  });
}
