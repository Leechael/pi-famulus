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
import type { ChildHandle, ChildStatus, CommsHost } from "./comms/types";
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

  interface Peer {
    childId: string;
    name: string;
    runId: string;
    activity: string;
  }
  const peers: Peer[] = [];

  const peerStatus = (peer: Peer): ChildStatus => {
    if (peer.activity === "running" || peer.activity === "starting") return "running";
    if (peer.activity === "failed") return "failed";
    return "completed";
  };

  const handleFor = (peer: Peer): ChildHandle => ({
    childId: peer.childId,
    result: Promise.resolve({ status: "completed", text: "", durationMs: 0 }),
    steer: async (message: string) => { await client?.agentInput(peer.childId, "prompt_steer", message, childId); },
    followUp: async (message: string) => { await client?.agentInput(peer.childId, "prompt_follow", message, childId); },
    resume: async (message: string) => { await client?.agentInput(peer.childId, "prompt", message, childId); },
    interrupt: async () => { await client?.agentInput(peer.childId, "abort", "", childId); },
    status: () => peerStatus(peer),
    lastEventAt: () => Date.now(),
  });

  const host: CommsHost = {
    getChild(id) {
      const peer = peers.find((item) => item.childId === id);
      if (!peer) return undefined;
      return { handle: handleFor(peer), runId: peer.runId, name: peer.name, status: peerStatus(peer) };
    },
    listChildren() {
      return peers.map((peer) => ({ childId: peer.childId, runId: peer.runId, name: peer.name, status: peerStatus(peer) }));
    },
    sameRun: () => true,
    notifySupervisor: () => {},
    resumeChild: async (id, message) => {
      if (!client) throw new Error("pi-famulus daemon is not connected");
      await client.agentInput(id, "prompt", message, childId);
      return { queuedBehind: null };
    },
  };

  const refreshPeers = async () => {
    if (!client?.isAvailable()) return;
    const listed = await client.agentPeers().catch(() => []);
    peers.splice(0, peers.length, ...listed.map((peer) => ({
      childId: peer.childId,
      name: peer.name,
      runId: peer.runId,
      activity: peer.activity,
    })));
  };

  const comms = createComms(host);

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

  const messageTool = createAgentMessageTool(comms, { kind: "child", childId, runId }, host, realClock);
  pi.registerTool({
    ...messageTool,
    execute: async (toolCallId, params, signal, onUpdate, toolCtx) => {
      await refreshPeers();
      return messageTool.execute(toolCallId, params, signal, onUpdate, toolCtx);
    },
  });

  if (bashAllowed) {
    pi.registerTool(createChildBashTool({
      getClient: () => client,
      home,
      sessionId: () => ctx?.sessionManager.getSessionId() ?? childId,
      sessionEnv: (): Record<string, string> => {
        const current = ctx;
        if (!current) return {};
        return {
          PI_SESSION_ID: current.sessionManager.getSessionId(),
          PI_SESSION_FILE: current.sessionManager.getSessionFile?.() ?? "",
          PI_PROVIDER: current.model?.provider ?? "",
          PI_MODEL: current.model?.id ?? "",
          PI_REASONING_LEVEL: current.thinkingLevel ?? "",
        };
      },
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
