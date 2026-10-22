/**
 * pi-famulus extension entry point (design doc §4).
 *
 * M1: bash override (auto-backgrounding) + task_* tools + manager client.
 * M2: NotifyCenter + monitor tool.
 * M3: subagent tool (InProcessRunner + tasks/chain + budget-to-async) + fleet widget.
 */
import { registerBehaviorGuidelines } from "./behavior-guidelines";
import { realClock } from "./clock";
import { ExitNotifyGate } from "./exit-notify-gate";
import { ExitWatchdog } from "./exit-watchdog";
import { createExtensionEventLog } from "./events";
import { readFileTail } from "./file-tail";
import { statSync } from "node:fs";
import { readdir, readFile, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ExtensionContext, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { resolveAgent as resolveAgentDef } from "./agents/definition";
import { createAgentLoader, type AgentLoader } from "./agents/loader";
import { createBashOverride } from "./bash-override";
import { cleanupSessionSpillFiles } from "./bounded-output";
import { createComms, type CommsWithOrigin } from "./comms/comms";
import { createRegistryCommsHost } from "./comms/registry-host";
import { createAgentMessageTool } from "./comms/tools";
import { registerReplyCommand } from "./comms/reply-command";
import { describeManagerSearch, getFamulusHome, loadConfig, resolveManagerPath, resolveSubagentConfig, taskOutputPath } from "./config";
import { ensureNativeManagerInstalled } from "./native-manager.js";
import type { TaskExitInfo } from "./format";
import { EXTENSION_VERSION, ManagerClient, type ManagerEvent, type TaskRecord } from "./manager-client";
import { createMonitorTool, exitEventFromRecord, MonitorRegistry } from "./monitor";
import { admitAgentChild, reregisterAgentLeases } from "./subagent/admission";
import { NotifyCenter } from "./notify";
import { onWakeMessageEnd, registryStatusLookup } from "./wake-delivery";
import { ChildShellTracker, createOverrunNotifier } from "./subagent/overrun";
import {
  agentEndReason,
  headOf,
  loadAgentChildRecords,
  findAgentChildRecord,
  tailOf,
  updateAgentChildMetrics,
  writeAgentChildRecord,
  type AgentChildRecord,
} from "./subagent/agent-records";
import { TranscriptWriter } from "./subagent/transcript";
import { FleetWidget } from "./subagent/fleet-widget";
import { printModeDropWarning, WORK_RETAIN_MS, WorkIndex, type WorkItem } from "./work-index";
import { fileURLToPath } from "node:url";
import { AGENT_WORK_KINDS, type AgentWorkKind } from "./subagent/types";
import { isFamulusChildSession, modelCandidates } from "./subagent/pi-runtime";
import { createHostedSessionFn } from "./subagent/hosted-session";
import { isHostedChildProcess, registerChildMode } from "./child-mode";
import { openAttachView, registerAttachCommand } from "./tui/attach-command";
import { SubagentRegistry } from "./subagent/registry";
import { InProcessRunner } from "./subagent/runner";
import { createSubagentTool } from "./subagent/tool";
import { createTaskListTool, createTaskOutputTool, createTaskStopTool } from "./task-tools";
import { createPiBinding, parkRuntime, takeParkedRuntime } from "./reload-handoff";
import { registerFamulusCommand } from "./tui/famulus-command";
import { registerFamulusMessageRenderers } from "./tui/message-renderers";
import { registerTasksCommand } from "./tui/tasks-command";
import { stderrPathFor } from "./tui/task-output-paths";
import { shellWakeTitle } from "./wake";

async function promptIfHostedSession(
  c: ManagerClient,
  live: () => ExtensionContext | null,
  query: { sessionFile?: string | null; sessionId?: string },
  where: "start" | "switch",
): Promise<"takeover" | "stay" | "none"> {
  if (!c.isAvailable() || c.protocolLevel() < 6) return "none";
  const found = await c.hostedLookup({
    ...(query.sessionId ? { sessionId: query.sessionId } : {}),
    ...(query.sessionFile ? { sessionFile: query.sessionFile } : {}),
  }).catch(() => null);
  if (!found || found.residency !== "warm" || typeof found.child_id !== "string") return "none";
  const uiCtx = live();
  if (!uiCtx) return "none";
  if (where === "start" && query.sessionId && uiCtx.sessionManager.getSessionId() !== query.sessionId) return "none";
  if (!uiCtx.hasUI) {
    uiCtx.ui.notify(
      "This session file belongs to a warm hosted subagent. Use /attach from the parent, or open it in the TUI to take over.",
      "warning",
    );
    return "stay";
  }
  const choice = await uiCtx.ui.select("This session is hosted by pi-famulus", [
    "Attach to the hosted process",
    "Take over",
    "Cancel",
  ]);
  if (choice === "Take over") {
    try {
      await c.agentTakeover(found.child_id);
      return "takeover";
    } catch (err) {
      uiCtx.ui.notify(err instanceof Error ? err.message : String(err), "error");
      return "stay";
    }
  }
  if (choice === "Attach to the hosted process") {
    try {
      await openAttachView(uiCtx, c, found.child_id);
    } catch (err) {
      uiCtx.ui.notify(err instanceof Error ? err.message : String(err), "error");
    }
  }
  return "stay";
}

/** Read only the tail of a task output file for the notification preview (≤ maxChars). */
export function readPreview(outputPath: string | undefined, maxChars: number): string {
  if (!outputPath) return "";
  const tail = readFileTail(outputPath, Math.max(maxChars, maxChars * 4));
  if (!tail || !tail.text) return "";
  return tail.text.length <= maxChars ? tail.text : tail.text.slice(-maxChars);
}

/** Map a manager exit event to the background-wake status. Exported for tests. */
export function toExitStatus(event: ManagerEvent): TaskExitInfo["status"] {
  // The manager died without shutting down; its runner took the task down.
  if (event.end_reason === "manager-crash") return "orphaned";
  if (event.signal) return "killed";
  if (event.exit_code === 0) return "completed";
  return "failed";
}

interface FamulusRuntime {
  attach(pi: ExtensionAPI): void;
}

export default function (pi: ExtensionAPI): void {
  if (isHostedChildProcess()) {
    registerChildMode(pi);
    return;
  }
  if (isFamulusChildSession(pi)) return;
  // A /reload parked the previous instance's runtime: adopt it so its shells,
  // monitors and subagents keep running (see reload-handoff.ts). Another
  // version's runtime is torn down instead; its code may not match ours.
  const parked = takeParkedRuntime<FamulusRuntime>();
  if (parked?.version === EXTENSION_VERSION) {
    parked.runtime.attach(pi);
    return;
  }
  const retiring = parked?.dispose().catch(() => {});
  createFamulusRuntime(retiring).attach(pi);
}

/**
 * Everything one pi session owns. It registers on a PiBinding rather than on
 * pi directly, so a /reload can re-attach it to the next extension instance.
 */
function createFamulusRuntime(retiring?: Promise<unknown>): FamulusRuntime {
  const binding = createPiBinding();
  const pi = binding.api;
  registerBehaviorGuidelines(pi);
  const home = getFamulusHome();
  const config = loadConfig(home);
  // pi update reinstalls extensions with npm. omit=optional drops the platform
  // package during that install. Fetch it in the background so a slow or
  // unreachable registry cannot stall extension startup.
  let managerPath = resolveManagerPath(config, home);
  let famulusInfo: {
    extensionVersion: string;
    managerPath: string | null;
    describeSearch: () => string;
    getClient: () => ManagerClient | null;
  } | null = null;
  const nativeRestore: Promise<string | null> = managerPath
    ? Promise.resolve(managerPath)
    : Promise.resolve()
        .then(() => ensureNativeManagerInstalled())
        .then((path) => {
          if (path) {
            managerPath = path;
            if (famulusInfo) famulusInfo.managerPath = path;
          }
          return path;
        })
        .catch(() => null);
  const clock = realClock;

  // Claude-style transcript pills for notifications (TUI only; no-ops elsewhere).
  registerFamulusMessageRenderers(pi);

  let ctx: ExtensionContext | null = null;
  let client: ManagerClient | null = null;
  const globalAgentLeases = new Map<string, string>();
  const pendingAgentLeases = new Set<string>();
  const pendingAgentReregistrations = new Map<string, AbortController>();
  let leaseReconciliation: Promise<void> | null = null;
  const reconcileAgentLeases = (manager: ManagerClient): Promise<void> => {
    if (leaseReconciliation) return leaseReconciliation;
    leaseReconciliation = reregisterAgentLeases(
      manager,
      globalAgentLeases,
      undefined,
      pendingAgentLeases,
      pendingAgentReregistrations,
    ).finally(() => {
      leaseReconciliation = null;
    });
    return leaseReconciliation;
  };
  const capacityNotices = new Set<string>();
  const capacityNotice = (reason: "manager unavailable" | "daemon too old"): void => {
    if (capacityNotices.has(reason)) return;
    capacityNotices.add(reason);
    console.warn(`pi-famulus: machine-wide agent capacity inactive (${reason}); using per-session limit only`);
  };
  let notifyCenter: NotifyCenter | null = null;
  let monitorRegistry: MonitorRegistry | null = null;
  let subagentRegistry: SubagentRegistry | null = null;
  let fleetWidget: FleetWidget | null = null;
  let agentLoader: AgentLoader | null = null;
  /** task_id -> metadata, for notifications and the original manager task start time. */
  const taskMeta = new Map<string, { kind: string; command: string; cwd?: string; startedAt?: number }>();
  /**
   * task_ids whose task_exited should wake the parent via <pi-famulus-wake kind="task">.
   * Parent bash only adds ids when it actually backgrounded the command.
   * Child-bash (sync wait) must not — otherwise every subagent shell completion
   * is mis-labeled as a parent "Background command" wake (§4.2 / §4.6).
   */
  const notifyOnExit = new Set<string>();
  /** Sessions that already received the crash-recovery notice. */
  const recoveredSessions = new Set<string>();
  const exitGate = new ExitNotifyGate<ManagerEvent>({ clock });
  const workIndex = new WorkIndex({ clock });
  const patchExited = (taskId: string, event: ManagerEvent): void => {
    workIndex.patch(taskId, {
      status: toExitStatus(event),
      endedAt: clock.now(),
      exitCode: event.exit_code ?? null,
      ...(event.signal ? { signal: event.signal } : {}),
      ...(event.end_reason ? { endReason: event.end_reason } : {}),
      ...(event.output_path
        ? { outputPath: event.output_path, stderrPath: stderrPathFor(event.output_path) }
        : {}),
    });
  };
  /**
   * Is the daemon process alive? The pid file can be stale (pid reuse), so
   * this errs towards "alive": a false positive only delays the settle, a
   * false negative would wake the agent while its command still runs.
   */
  const isDaemonAlive = async (): Promise<boolean> => {
    try {
      const pf = JSON.parse(await readFile(join(home, "manager.pid"), "utf8")) as { pid?: unknown };
      if (typeof pf.pid !== "number" || pf.pid <= 0) return false;
      try {
        process.kill(pf.pid, 0);
        return true;
      } catch (e) {
        return (e as NodeJS.ErrnoException).code === "EPERM"; // alive, not ours
      }
    } catch {
      return false; // no pid file / unparseable: no daemon
    }
  };
  /**
   * This session's task records, straight from disk: the fallback for when
   * the manager is unavailable. A record still "running" whose daemon is
   * dead is what the next daemon start marks orphaned (manager-crash), so
   * it settles here; a "running" record with a live daemon is left alone.
   */
  const loadDiskTasks = async (): Promise<TaskRecord[] | null> => {
    const sid = ctx?.sessionManager.getSessionId();
    if (!sid) return null;
    const dir = join(home, "sessions", sid, "tasks");
    let files: string[];
    try {
      files = await readdir(dir);
    } catch {
      return null;
    }
    const daemonAlive = await isDaemonAlive();
    const out: TaskRecord[] = [];
    for (const f of files) {
      if (!f.endsWith(".json")) continue;
      try {
        const rec = JSON.parse(await readFile(join(dir, f), "utf8")) as TaskRecord;
        // The persisted output_size lags the output file; the file is the truth.
        try {
          rec.output_size = Math.max(rec.output_size, (await stat(rec.output_path)).size);
        } catch { /* output file gone */ }
        if (rec.status === "running" && !daemonAlive) {
          rec.status = "orphaned";
          rec.end_reason = "manager-crash";
          rec.ended_at ??= clock.now();
        }
        out.push(rec);
      } catch { /* not a record */ }
    }
    return out;
  };
  /**
   * Parent-owned shells this process did not start (the previous pi died
   * while the daemon stayed up). Running ones join the index and get an
   * exit wake; recent finished ones are delivered the same way.
   * manager-shutdown / manager-crash leftovers stay for the crash notice.
   */
  const adoptParentShells = (tasks: TaskRecord[]): void => {
    const sid = ctx?.sessionManager.getSessionId() ?? "";
    const now = clock.now();
    for (const task of tasks) {
      if (task.kind !== "shell") continue;
      if (task.origin?.via === "child-bash") continue;
      if (sid && task.session_id && task.session_id !== sid) continue;
      if (workIndex.get(task.task_id) || taskMeta.has(task.task_id)) continue;
      const startedAt = task.started_at ?? now;
      taskMeta.set(task.task_id, {
        kind: "shell",
        command: task.command ?? "",
        cwd: task.cwd,
        startedAt,
      });
      if (task.status === "running") {
        upsertBackgroundTask(task.task_id, startedAt);
        markNotifyOnExit(task.task_id);
        continue;
      }
      if (task.end_reason === "manager-shutdown" || task.end_reason === "manager-crash") continue;
      if (task.ended_at != null && now - task.ended_at > WORK_RETAIN_MS) continue;
      if (task.status !== "completed" && task.status !== "failed" && task.status !== "killed") continue;
      upsertBackgroundTask(task.task_id, startedAt);
      deliverExit(task.task_id, exitEventFromRecord(task));
    }
  };
  /**
   * A running disk record whose hosted process is gone: interrupt the live
   * handle when this process still holds it (daemon crash), otherwise
   * write the record. Warm children stay for resume.
   */
  const settleDeadHostedChildren = async (
    sessionId: string,
    opts: { interruptLive?: boolean } = {},
  ): Promise<AgentChildRecord[]> => {
    const manager = client;
    const registry = subagentRegistry;
    const center = notifyCenter;
    const stale = loadAgentChildRecords(home, { sessionId, includeTerminal: false });
    const lost: AgentChildRecord[] = [];
    for (const rec of stale) {
      if (rec.status !== "running" && rec.status !== "pending") continue;
      const handle = registry?.handle(rec.child_id);
      // A local admission queue has no hosted process to look up yet.
      if (handle?.status() === "pending") continue;
      let live = false;
      if (manager?.isAvailable() && manager.protocolLevel() >= 6) {
        const found =
          await manager.hostedLookup({ sessionId: rec.child_id }).catch(() => null);
        if (client !== manager || subagentRegistry !== registry || notifyCenter !== center) return [];
        live = Boolean(found && (found.residency === "warm" || found.activity === "running" || found.activity === "starting"));
      }
      if (live) continue;
      if (handle && (handle.status() === "running" || handle.status() === "pending")) {
        if (!opts.interruptLive) continue;
        await handle.interrupt().catch(() => {});
        if (client !== manager || subagentRegistry !== registry || notifyCenter !== center) return [];
      } else {
        writeAgentChildRecord(home, {
          ...rec,
          status: "interrupted",
          ended_at: rec.ended_at ?? clock.now(),
          end_reason: "interrupted",
          error: rec.error ?? "parent session ended",
        });
      }
      lost.push(rec);
    }
    return lost;
  };
  /**
   * The previous process died. Settle stale running agent records (the child
   * is gone) and tell the model about that work and about commands the
   * daemon already ended as manager-shutdown / manager-crash.
   * A hosted child that is still warm is left for resume.
   */
  const recoverCrashedSession = async (sessionId: string): Promise<void> => {
    if (!sessionId || recoveredSessions.has(sessionId)) return;
    const manager = client;
    const center = notifyCenter;
    recoveredSessions.add(sessionId);
    const interrupted =
      await settleDeadHostedChildren(sessionId);
    if (client !== manager || notifyCenter !== center) return;
    let killed: TaskRecord[] = [];
    if (manager?.isAvailable()) {
      try {
        const tasks = await manager.list(true);
        if (client !== manager || notifyCenter !== center) return;
        killed = tasks.filter((task) => {
          if (task.session_id && task.session_id !== sessionId) return false;
          if (task.kind !== "shell") return false;
          return task.end_reason === "manager-shutdown" || task.end_reason === "manager-crash";
        });
      } catch {
        killed = [];
      }
    }
    if (interrupted.length === 0 && killed.length === 0) {
      recoveredSessions.delete(sessionId);
      return;
    }
    const lines = [
      "System wake — not a new user message.",
      "",
      "This session was reopened after the previous process ended.",
    ];
    for (const rec of interrupted) {
      lines.push(`Interrupted subagent ${rec.child_id} (${rec.name}).`);
    }
    for (const task of killed) {
      lines.push(`Command ${task.task_id} ended while the session was gone (${task.end_reason ?? task.status}).`);
    }
    center?.notify({
      customType: "pi-famulus-wake",
      content: lines.join("\n"),
      details: {
        kind: "session-recovery",
        children: interrupted.map((rec) => rec.child_id),
        tasks: killed.map((task) => task.task_id),
      },
    });
  };
  /**
   * Settle everything the manager reports as ended that we still show live:
   * monitors in the registry and shell/monitor rows in the index. Runs when
   * the user or model looks (task_list, /tasks), after task_stop, after a
   * monitor timeout, and after a reconnect. A backgrounded command the model
   * is waiting on gets its exit wake here: after a manager crash no
   * task_exited ever comes (the task ended orphaned), and without the wake
   * the model would wait forever.
   */
  const syncWithManager = async (): Promise<void> => {
    const c = client;
    const center = notifyCenter;
    let tasks: TaskRecord[];
    if (c && c.isAvailable()) {
      try {
        tasks = await c.list(true);
      } catch {
        return;
      }
    } else {
      // Unavailable (it crashed and every respawn attempt failed): settle
      // from the records on disk instead of letting the model wait forever.
      const disk = await loadDiskTasks();
      if (!disk) return;
      tasks = disk;
    }
    if (client !== c || notifyCenter !== center) return;
    monitorRegistry?.reconcile(tasks);
    adoptParentShells(tasks);
    for (const task of workIndex.staleLive(tasks)) {
      const event = exitEventFromRecord(task);
      // Route through the gate: if the real task_exited is processed after
      // this (a stop-triggered exit written after the list response, a
      // reconnect replay), the consumed mark stashes it instead of waking
      // the agent a second time.
      if (notifyOnExit.has(task.task_id) && exitGate.onExit(task.task_id, event, false) === "notify") {
        deliverExit(task.task_id, event);
      } else {
        patchExited(task.task_id, event);
      }
    }
  };
  // Settle on our own while a wake is pending: the agent was told to end its
  // turn, so nothing else may look before the wake is due.
  const exitWatchdog = new ExitWatchdog({
    clock,
    hasPending: () => notifyOnExit.size > 0,
    tick: async () => {
      await client?.ensureAvailable().catch(() => false);
      await syncWithManager();
    },
  });
  const eventLog = createExtensionEventLog(home, () => ctx?.sessionManager.getSessionId() ?? "", clock);
  const logEvent = (type: string, fields?: Record<string, unknown>) => eventLog.write(type, fields);

  const trackTask = (taskId: string, meta: { kind: string; command: string; cwd?: string }) => {
    taskMeta.set(taskId, { ...meta, startedAt: taskMeta.get(taskId)?.startedAt });
  };
  const upsertBackgroundTask = (taskId: string, startedAt: number) => {
    const meta = taskMeta.get(taskId);
    workIndex.upsert({
      id: taskId,
      kind: "shell",
      status: "running",
      title: meta?.command?.replace(/\s+/g, " ").trim() || taskId,
      command: meta?.command,
      cwd: meta?.cwd,
      startedAt,
      countsAsWorker: true,
    });
  };
  const deliverExit = (taskId: string, event: ManagerEvent) => {
    notifyOnExit.delete(taskId);
    const meta = taskMeta.get(taskId);
    taskMeta.delete(taskId);
    workIndex.patch(taskId, {
      status: toExitStatus(event),
      endedAt: clock.now(),
      exitCode: event.exit_code ?? null,
      ...(event.signal ? { signal: event.signal } : {}),
      ...(event.end_reason ? { endReason: event.end_reason } : {}),
      ...(event.output_path
            ? { outputPath: event.output_path, stderrPath: stderrPathFor(event.output_path) }
            : {}),
    });
    notifyCenter?.notifyTaskExit({
      taskId,
      kind: meta?.kind ?? event.kind ?? "shell",
      command: meta?.command ?? event.command ?? "",
      status: toExitStatus(event),
      exitCode: event.exit_code ?? null,
      ...(typeof event.signal === "string" && event.signal ? { signal: event.signal } : {}),
      ...(event.end_reason ? { endReason: event.end_reason } : {}),
      durationMs: event.duration_ms ?? 0,
      outputPath: event.output_path ?? "",
      preview: readPreview(event.output_path, 4000),
    });
  };

  const markNotifyOnExit = (taskId: string) => {
    notifyOnExit.add(taskId);
    exitWatchdog.arm();
    void client?.markBackground(taskId).catch(() => {});
    const prior = exitGate.mark(taskId);
    if (prior) {
      deliverExit(taskId, prior);
      return;
    }
    const startedAt = taskMeta.get(taskId)?.startedAt;
    if (startedAt !== undefined) {
      upsertBackgroundTask(taskId, startedAt);
    } else {
      // If task_started was missed (e.g. reconnect), recover the daemon's start
      // time rather than making this task appear younger than it is.
      void client?.list(true).then((tasks) => {
        const task = tasks.find((candidate) => candidate.task_id === taskId);
        const recoveredStart = taskMeta.get(taskId)?.startedAt ?? task?.started_at;
        if (recoveredStart !== undefined && notifyOnExit.has(taskId)) {
          const current = taskMeta.get(taskId) ?? { kind: "shell", command: task?.command ?? "" };
          taskMeta.set(taskId, { ...current, startedAt: recoveredStart });
          upsertBackgroundTask(taskId, recoveredStart);
        }
      }).catch(() => {});
    }
  };

  const sessionEnv = (c: ExtensionContext): Record<string, string> => ({
    PI_SESSION_ID: c.sessionManager.getSessionId(),
    PI_SESSION_FILE: c.sessionManager.getSessionFile() ?? "",
    PI_PROVIDER: c.model?.provider ?? "",
    PI_MODEL: c.model?.id ?? "",
    PI_REASONING_LEVEL: c.thinkingLevel ?? "",
  });

  const deps = {
    getClient: () => client,
    config,
    home,
    sessionId: () => ctx?.sessionManager.getSessionId() ?? "",
    sessionEnv,
    trackTask,
    markNotifyOnExit,
    clock,
    getRegistry: () => subagentRegistry,
    getIndex: () => workIndex,
    syncWithManager: () => syncWithManager(),
  };

  monitorRegistry = new MonitorRegistry({
    getClient: () => client,
    sessionEnv,
    getNotifyCenter: () => notifyCenter,
    trackTask,
    clock,
    logEvent,
    toast: (message, type) => {
      if (ctx?.hasUI) ctx.ui.notify(message, type);
    },
    onExited: (taskId, event) => patchExited(taskId, event),
    afterStop: () => void syncWithManager(),
  });

  pi.registerTool(createBashOverride(deps));
  pi.registerTool(createTaskListTool({ ...deps, getIndex: () => workIndex }));
  pi.registerTool(createTaskOutputTool({ ...deps, getIndex: () => workIndex }));
  pi.registerTool(
    createTaskStopTool({
      ...deps,
      getIndex: () => workIndex,
      noteStopRequested: (id) => monitorRegistry?.noteStopRequested(id),
    }),
  );
  pi.registerTool(createMonitorTool(monitorRegistry, () => workIndex));
  registerTasksCommand(pi, {
    getRegistry: () => subagentRegistry,
    getIndex: () => workIndex,
    getClient: () => client,
    home,
    sessionId: () => ctx?.sessionManager.getSessionId() ?? "",
    clock,
    syncWithManager: deps.syncWithManager,
  });
  registerAttachCommand(pi, {
    getClient: () => client,
    getRegistry: () => subagentRegistry,
  });
  famulusInfo = {
    extensionVersion: EXTENSION_VERSION,
    managerPath,
    describeSearch: () => describeManagerSearch(config, home),
    getClient: () => client,
  };
  registerFamulusCommand(pi, famulusInfo);
  monitorRegistry.onChange(() => {
    for (const mon of monitorRegistry.listActive()) {
      const existing = workIndex.get(mon.taskId);
      // Never revive a row that already ended (an exit can beat registration).
      if (existing && existing.status !== "running" && existing.status !== "pending") continue;
      workIndex.upsert({
        id: mon.taskId,
        kind: "monitor",
        status: "running",
        title: mon.description,
        command: taskMeta.get(mon.taskId)?.command,
        cwd: taskMeta.get(mon.taskId)?.cwd,
        startedAt: existing?.startedAt ?? mon.startedAt,
        outputPath: existing?.outputPath,
        stderrPath: existing?.stderrPath,
        countsAsWorker: false,
      });
    }
  });

  // M3: subagent tool. The registry/runner are (re)built on every session_start;
  // the tool resolves them lazily through getters.
  const subagentConfig = resolveSubagentConfig(config);
  // M4: comms host bridges the registry and the NotifyCenter. A single Comms
  // instance lives at extension scope (mailbox is per-run namespaced; pending
  // waiters are resolved via dispose() on session_shutdown).
  let watchResume: (childId: string) => void = () => {};
  const commsHost = createRegistryCommsHost({
    getRegistry: () => subagentRegistry,
    getNotifyCenter: () => notifyCenter,
    onResumeAccepted: (childId) => watchResume(childId),
  });
  const comms: CommsWithOrigin = createComms(commsHost, {
    decisionTimeoutMs: subagentConfig.decisionTimeoutMs,
    clock,
    logEvent,
  });
  registerReplyCommand(pi, comms);
  pi.registerTool(
    createSubagentTool({
      getRegistry: () => subagentRegistry,
      getNotifyCenter: () => notifyCenter,
      getIndex: () => workIndex,
      budgetMs: () => subagentConfig.budgetMs,
      defaultTimeoutMs: subagentConfig.timeoutMs,
      defaultConcurrency: subagentConfig.concurrency,
      clock,
      registerResumeWatch: (watch) => {
        watchResume = watch;
      },
      resolveAgent: (name) => {
        const loader = agentLoader;
        if (!loader) return resolveAgentDef([], name); // builtins-only fallback
        return resolveAgentDef(loader.reload().definitions, name);
      },
      recoverChild: async (childId) => {
        const registry = subagentRegistry;
        if (!registry) return undefined;
        const rec = findAgentChildRecord(home, childId);
        if (!rec) return undefined;
        if (registry.handle(rec.child_id)) {
          return registry.get(rec.run_id);
        }
        const workKind = AGENT_WORK_KINDS.includes(rec.work_kind as AgentWorkKind)
          ? rec.work_kind as AgentWorkKind
          : undefined;
        const loader = agentLoader;
        const agent = resolveAgentDef(loader ? loader.reload().definitions : [], rec.agent);
        registry.adoptExisting({
          runId: rec.run_id,
          childId: rec.child_id,
          name: rec.name,
          agent: rec.agent,
          workKind,
          startedAt: rec.started_at,
          prompt: rec.prompt_head,
        });
        await registry.attachChild({
          childId: rec.child_id,
          runId: rec.run_id,
          name: rec.name,
          prompt: rec.prompt_head ?? "",
          agent,
          timeoutMs: subagentConfig.timeoutMs,
          depth: 1,
          ...(workKind ? { workKind } : {}),
        });
        return registry.get(rec.run_id);
      },
      listModels: () => {
        const current = ctx?.model;
        return modelCandidates({
          getModelRegistry: () => ctx?.modelRegistry ?? null,
          getScopedModels: () => ctx?.scopedModels ?? [],
        }).map((c) => ({
          ...c,
          current: current !== undefined && c.provider === current.provider && c.id === current.id,
        }));
      },
    }),
  );
  // M4: parent-side agent_message tool (child-side variant is injected into
  // each child session via customTools, see below).
  pi.registerTool(createAgentMessageTool(comms, { kind: "parent" }, commsHost, clock));

  /** Session whose runtime a /reload parked; the reloaded instance's session_start resumes it. */
  let parkedSessionId: string | null = null;
  const startFleetWidget = (startCtx: ExtensionContext) => {
    if (!startCtx.hasUI) return;
    fleetWidget = new FleetWidget({
      index: workIndex,
      getUi: () => (ctx?.hasUI ? (ctx.ui as never) : null),
      clock,
    });
    fleetWidget.start();
  };

  pi.on("session_start", async (event, startCtx) => {
    if (parkedSessionId !== null) {
      const resumed = event.reason === "reload" && startCtx.sessionManager.getSessionId() === parkedSessionId;
      parkedSessionId = null;
      if (resumed) {
        // Same session after /reload: the client, registry and monitors kept
        // running; only the context and the UI belong to the new instance.
        ctx = startCtx;
        binding.ready();
        startFleetWidget(startCtx);
        void syncWithManager();
        return;
      }
      binding.discardQueued();
      await teardown();
    }
    await retiring;
    ctx = startCtx;
    binding.ready();
    notifyCenter?.dispose();
    notifyCenter = new NotifyCenter({
      sendMessage: (msg, opts) => pi.sendMessage(msg, opts),
      isIdle: () => ctx?.isIdle() ?? true,
      clock,
      logEvent,
      listStillRunning: () =>
        [...notifyOnExit].map((id) => {
          const command = taskMeta.get(id)?.command;
          const title = command ? shellWakeTitle(command) || id : id;
          return { id, title };
        }),
    });
    fleetWidget?.dispose();
    fleetWidget = null;
    comms.dispose(); // Release decision waiters before awaiting child aborts.
    await subagentRegistry?.disposeAll().catch(() => {});
    subagentRegistry = null;
    for (const controller of pendingAgentReregistrations.values()) controller.abort();
    pendingAgentReregistrations.clear();
    globalAgentLeases.clear();
    pendingAgentLeases.clear();

    client = new ManagerClient({
      home,
      sessionId: startCtx.sessionManager.getSessionId(),
      managerPath,
      cwd: startCtx.cwd,
      sessionFile: startCtx.sessionManager.getSessionFile?.(),
      clock,
      log: () => {}, // keep quiet; degradation is surfaced via tools
    });

    // Hosted children run bash in their own process. The parent learns the
    // foreground shell from task_started (origin child-bash) so overrun wakes
    // still carry <shell>.
    const childShells = new ChildShellTracker();
    const parentSessionId = startCtx.sessionManager.getSessionId();
    // Bind this session's notify centre and client. After /new /resume /fork
    // the outer `notifyCenter` is replaced; late events on the old client
    // must not follow that rebinding.
    const sessionNotify = notifyCenter;
    const sessionClient = client;
    client.onEvent((event) => {
      if (client !== sessionClient) return;
      const hosted = event as ManagerEvent & {
        child_id?: string;
        reason?: string;
        request_id?: string;
        message?: string;
        name?: string;
        residency?: string;
        driver?: string;
        last_text?: string;
        request?: { type?: string; id?: string; method?: string; title?: string; message?: string };
      };
      if (hosted.event === "agent_comms" && hosted.child_id && hosted.request_id) {
        const reason = hosted.reason === "need_decision" ? "need_decision" : "progress_update";
        void comms.contactSupervisor(hosted.child_id, reason, hosted.message ?? "").then((reply) => {
          if (reason === "need_decision") void client?.agentCommsReply(hosted.request_id!, hosted.child_id!, reply).catch(() => {});
        }).catch(() => {});
        return;
      }
      if (hosted.event === "agent_ui" && hosted.child_id) {
        const request = hosted.request ?? {};
        const method = request.method;
        if (method !== "select" && method !== "confirm" && method !== "input" && method !== "editor") return;
        const title = typeof request.title === "string" ? request.title : "";
        const body = typeof request.message === "string" ? request.message : "";
        sessionNotify?.notify({
          customType: "pi-famulus-wake",
          content:
            `System wake — not a new user message.\n\n` +
            `Subagent ${hosted.child_id} is waiting for a ${method}` +
            (title ? `: ${title}` : ".") +
            (body ? `\n${body}` : ""),
          details: {
            kind: "supervisor-request",
            from: hosted.child_id,
            method,
            ...(title ? { title } : {}),
            ...(body ? { message: body } : {}),
          },
        });
        return;
      }
      if (hosted.event === "agent_detach" && hosted.child_id) {
        sessionNotify?.notify({
          customType: "pi-famulus-wake",
          content: `System wake — not a new user message.\n\nThe user talked to ${hosted.name ?? hosted.child_id} directly.\n${hosted.last_text ?? ""}`,
          details: { kind: "supervisor-update", from: hosted.child_id, name: hosted.name ?? hosted.child_id, message: hosted.last_text ?? "" },
        });
        return;
      }
      if (hosted.event === "agent_state" && hosted.child_id) {
        const handle = subagentRegistry?.handle(hosted.child_id) as { pauseStall?: () => void; resumeStall?: () => void } | undefined;
        if (hosted.driver === "user") handle?.pauseStall?.();
        else handle?.resumeStall?.();
        return;
      }
      if (event.event === "task_started" && event.task_id) {
        const prior = taskMeta.get(event.task_id);
        taskMeta.set(event.task_id, {
          kind: prior?.kind ?? event.kind ?? "shell",
          command: prior?.command ?? event.command ?? "",
          ...(typeof event.ts === "number"
            ? { startedAt: event.ts }
            : prior?.startedAt !== undefined
              ? { startedAt: prior.startedAt }
              : {}),
        });
        if (event.origin?.via === "child-bash" && event.origin.child_id) {
          childShells.start(event.origin.child_id, {
            taskId: event.task_id,
            command: event.command ?? "",
            startedAt: typeof event.ts === "number" ? event.ts : clock.now(),
            outputPath: taskOutputPath(home, parentSessionId, event.task_id),
          });
        }
        if (notifyOnExit.has(event.task_id) && typeof event.ts === "number") {
          upsertBackgroundTask(event.task_id, event.ts);
        }
        if (event.kind === "monitor" && !workIndex.get(event.task_id)) {
          workIndex.upsert({
            id: event.task_id,
            kind: "monitor",
            status: "running",
            title: event.command || event.task_id,
            command: event.command,
            cwd: taskMeta.get(event.task_id)?.cwd,
            startedAt: clock.now(),
            countsAsWorker: false,
          });
        }
        return;
      }
      if (event.event === "output" && event.task_id && typeof event.chunk === "string") {
        monitorRegistry?.handleOutput(event.task_id, event.chunk, event.next_cursor, event.monitor_records);
        return;
      }
      if (event.event === "task_exited" && event.task_id) {
        childShells.endByTask(event.task_id);
        // A known monitor closes (and patches the index via onExited); an
        // unknown id is buffered in case a monitor start is about to claim it.
        if (monitorRegistry?.handleExit(event.task_id, event)) return;
        patchExited(event.task_id, event);
        // Exit may share a socket read with wait done:false, before bash marks
        // the id. Stash and fire on the late mark. Sync waits never mark.
        if (exitGate.onExit(event.task_id, event, false) !== "notify") return;
        deliverExit(event.task_id, event);
      }
    });
    client.onReconnect(() => {
      if (client !== sessionClient) return;
      // A daemon restart may drop permits; a same-session rebind may retain
      // them. Keep each kind for idempotent re-registration and mark all local
      // leases pending until the daemon confirms them.
      for (const childId of globalAgentLeases.keys()) pendingAgentLeases.add(childId);
      void monitorRegistry?.rewatchAll().then(() => {
        if (client === sessionClient) return syncWithManager();
      });
      void (async () => {
        const manager = sessionClient;
        if (manager.protocolLevel() < 4) {
          capacityNotice("daemon too old");
          return;
        }
        // The registry and its child ids survive socket reconnects in this
        // extension process; held ids include resumable interrupted children.
        await reconcileAgentLeases(manager);
        if (client !== manager) return;
        const sid = parentSessionId;
        const lost =
          await settleDeadHostedChildren(sid, { interruptLive: true });
        if (lost.length > 0) {
          sessionNotify?.notify({
            customType: "pi-famulus-wake",
            content: [
              "System wake — not a new user message.",
              "",
              "The daemon restarted. Hosted children that were running are gone.",
              ...lost.map((rec) => `Interrupted subagent ${rec.child_id} (${rec.name}).`),
            ].join("\n"),
            details: { kind: "session-recovery", children: lost.map((rec) => rec.child_id) },
          });
        }
      })().catch(() => {});
    });

    // M3: subagent registry + in-process runner + fleet widget. The runner's
    // per-generation admission goes through the registry (global concurrency
    // cap); the session factory resolves models/cwd lazily from ctx.
    const registry = new SubagentRegistry({
      maxConcurrentChildren: subagentConfig.maxConcurrentChildren,
      spawnBudgetPerHour: subagentConfig.spawnBudgetPerHour,
      clock,
    });
    // M5: agent definitions, reloaded lazily (mtime-cached) per subagent call.
    agentLoader = createAgentLoader({
      userDir: join(homedir(), ".pi", "agent", "agents"),
      projectDir: join(startCtx.cwd, ".pi", "agents"),
    });
    const agentLoadErrors = agentLoader.reload().errors;
    if (agentLoadErrors.length > 0 && startCtx.hasUI) {
      startCtx.ui.notify(
        `pi-famulus: skipped ${agentLoadErrors.length} invalid agent definition(s): ` +
          agentLoadErrors.map((e) => `${e.path} (${e.error})`).join("; "),
        "warning",
      );
    }
    const createSession = createHostedSessionFn({
      getModelRegistry: () => ctx?.modelRegistry ?? null,
      getModelRuntime: () => {
        const registry = ctx?.modelRegistry as { runtime?: unknown } | null | undefined;
        return registry?.runtime;
      },
      getParentModel: () => ctx?.model,
      getParentThinkingLevel: () => ctx?.thinkingLevel,
      getScopedModels: () => ctx?.scopedModels ?? [],
      getCwd: () => ctx?.cwd ?? process.cwd(),
      getProjectTrusted: () => ctx?.isProjectTrusted() ?? false,
      getClient: () => client,
      getHome: () => home,
      extensionEntry: fileURLToPath(import.meta.url),
      piCommand: subagentConfig.piCommand,
    });
    // The registry outlives startCtx across a /reload, which leaves startCtx
    // stale; its closures keep plain values instead.
    const sessionId = startCtx.sessionManager.getSessionId();
    const sessionCwd = startCtx.cwd;
    const transcripts = new TranscriptWriter(home, clock);
    const syncTranscript = (childId: string): string | undefined => {
      const handle = registry.handle(childId);
      if (!handle) return undefined;
      return transcripts.sync(sessionId, childId, handle.conversation());
    };
    const runner = new InProcessRunner({
      createSession,
      clock,
      stallMs: subagentConfig.stallMs,
      stallRetries: subagentConfig.stallRetries,
      stallRetryDelayMs: subagentConfig.stallRetryDelayMs,
      overrunRepeatMs: subagentConfig.overrunRepeatMs,
      hardTimeoutMs: subagentConfig.hardTimeoutMs,
      acquire: (req, ticket) => admitAgentChild({
        childId: req.childId,
        workKind: req.workKind ?? "other",
        reserveLocal: () => registry.reserveChildSlot(req.childId, ticket),
        manager: client,
        leases: globalAgentLeases,
        pendingLeases: pendingAgentLeases,
        pendingReregistrations: pendingAgentReregistrations,
        ticket,
        notice: capacityNotice,
      }),
      onActivity: (childId) => {
        syncTranscript(childId);
        const handle = registry.handle(childId);
        const usage = handle?.tokenUsage();
        const wall = handle?.wallUsage();
        if (usage && wall) {
          const metrics = {
            tokens_input: usage.input,
            tokens_output: usage.output,
            tokens_cache_read: usage.cacheRead,
            tokens_cache_write: usage.cacheWrite,
            llm_ms: wall.llmMs,
            tool_ms: wall.toolMs,
            queue_ms: wall.queueMs,
            wall_other_ms: wall.otherMs,
            wall_approximate: wall.approximate,
          };
          updateAgentChildMetrics(home, sessionIdForAgents(), childId, metrics);
          logEvent("agent.usage", { child_id: childId, ...metrics });
        }
      },
      onStall: (childId, attempt) => {
        // One event per stall detection: an auto-resume follows unless the
        // retry budget (stallRetries) is already spent.
        logEvent("agent.stall", { child_id: childId, attempt });
        notifyCenter?.notify({
          customType: "pi-famulus-wake",
          content:
            `System wake — not a new user message.\n\n` +
            `Subagent ${childId} stalled with no activity (attempt ${attempt}); retrying.`,
          details: { kind: "subagent-stall", child_id: childId, attempt },
        });
      },
      onToolLoop: (childId, toolName, args) => {
        const argText = args && typeof args === "object" ? JSON.stringify(args) : String(args ?? "");
        notifyCenter?.notify({
          customType: "pi-famulus-wake",
          content:
            `System wake — not a new user message.\n\n` +
            `Subagent ${childId} called ${toolName} three times in a row with the same arguments${argText ? `: ${argText}` : "."}`,
          details: { kind: "subagent-loop", child_id: childId, tool: toolName, args },
        });
      },
      // Soft deadline: the child keeps running; the parent decides.
      onOverrun: createOverrunNotifier({
        now: () => clock.now(),
        registry,
        shells: childShells,
        stat: (path) => {
          try {
            const st = statSync(path);
            return { size: st.size, mtimeMs: st.mtimeMs };
          } catch {
            return null;
          }
        },
        notify: (wake) => notifyCenter?.notify(wake),
        logEvent,
      }),
    });
    registry.setRunner(runner);
    subagentRegistry = registry;
    // Persist child records so `pi-famulus ls` / task_list can see in-process agents.
    const sessionIdForAgents = () => sessionId;
    const previousAgentStatus = new Map<string, string>();
    registry.onTransition((run) => {
      const sid = sessionIdForAgents();
      for (const c of run.children) {
        const previousStatus = previousAgentStatus.get(c.childId);
        if (c.status === "running" && previousStatus !== "running") {
          logEvent("agent.start", {
            child_id: c.childId,
            run_id: run.runId,
            name: c.name,
            agent: c.agent,
            work_kind: c.workKind ?? "other",
            queue_ms: c.queueMs ?? 0,
            ...(c.model ? { model: c.model } : {}),
          });
        }
        if (["completed", "failed", "interrupted"].includes(c.status) && previousStatus !== c.status) {
          const error = c.result?.error;
          // Only the opt-in hard ceiling settles with error=timeout now.
          if (error === "timeout") logEvent("agent.timeout", { child_id: c.childId });
          logEvent("agent.settle", {
            child_id: c.childId,
            status: c.status,
            ...(error ? { error } : {}),
            work_kind: c.workKind ?? "other",
            ...(c.result?.stalls ? { stalls: c.result.stalls } : {}),
            ...(c.queueMs !== undefined ? { queue_ms: c.queueMs } : {}),
            duration_ms: c.result?.durationMs ?? Math.max(0, clock.now() - c.startedAt),
            ...(c.result?.queueMs !== undefined ? { queue_ms: c.result.queueMs } : {}),
          });
        }
        previousAgentStatus.set(c.childId, c.status);
        const rec: AgentChildRecord = {
          v: 1,
          kind: "agent",
          child_id: c.childId,
          run_id: run.runId,
          session_id: sid,
          name: c.name,
          agent: c.agent,
          work_kind: c.workKind ?? "other",
          ...(c.queueMs !== undefined ? { queue_ms: c.queueMs } : {}),
          ...(c.model !== undefined ? { model: c.model } : {}),
          status: c.status,
          started_at: c.startedAt,
          tokens_input: c.tokenUsage?.input ?? 0,
          tokens_output: c.tokenUsage?.output ?? 0,
          tokens_cache_read: c.tokenUsage?.cacheRead ?? 0,
          tokens_cache_write: c.tokenUsage?.cacheWrite ?? 0,
          llm_ms: c.wallUsage?.llmMs ?? 0,
          tool_ms: c.wallUsage?.toolMs ?? 0,
          queue_ms: c.wallUsage?.queueMs ?? 0,
          wall_other_ms: c.wallUsage?.otherMs ?? 0,
          wall_approximate: c.wallUsage?.approximate ?? false,
          ...(c.endedAt !== undefined ? { ended_at: c.endedAt } : {}),
          ...(c.result?.error ? { error: c.result.error } : {}),
          ...(c.result?.attempts !== undefined && c.result.attempts > 1
            ? { attempts: c.result.attempts }
            : {}),
          ...(c.result?.stalls ? { stalls: c.result.stalls } : {}),
          ...(c.prompt !== undefined ? { prompt_head: headOf(c.prompt) } : {}),
          ...(c.result?.text ? { result_tail: tailOf(c.result.text) } : {}),
        };
        const endReason = agentEndReason(c.status, c.result);
        if (endReason) rec.end_reason = endReason;
        if (c.result?.retryAfter !== undefined) rec.retry_after = c.result.retryAfter;
        const transcript = syncTranscript(c.childId);
        if (transcript) {
          rec.transcript = transcript;
          rec.tool_calls = transcripts.toolCallCount(c.childId);
        }
        writeAgentChildRecord(home, rec);
        workIndex.upsert({
          id: c.childId,
          kind: "agent",
          status: c.status,
          title: `${c.name} (${c.agent})${c.model ? ` ${c.model}` : ""}`,
          startedAt: c.startedAt,
          ...(c.endedAt !== undefined ? { endedAt: c.endedAt } : {}),
          countsAsWorker: false,
          runId: run.runId,
          name: c.name,
          agent: c.agent,
          workKind: c.workKind ?? "other",
          ...(c.queueMs !== undefined ? { queueMs: c.queueMs } : {}),
          ...(c.model !== undefined ? { model: c.model } : {}),
          cwd: sessionCwd,
          ...(c.prompt !== undefined ? { prompt: c.prompt } : {}),
          ...(c.preamble !== undefined ? { preamble: c.preamble } : {}),
          ...(c.result?.text ? { text: c.result.text } : {}),
          ...(c.result?.error ? { error: c.result.error } : {}),
        });
      }
    });
    startFleetWidget(startCtx);
    if (client?.isAvailable()) {
      await recoverCrashedSession(startCtx.sessionManager.getSessionId());
    }

    // Connect in the background: a cold manager spawn must not stall the
    // first prompt. The cache install, if one is running, finishes first so a
    // missing optional package is not reported before it can be restored.
    // Tools call ensureAvailable() lazily before use.
    if (client !== sessionClient) return;
    const c = sessionClient;
    void nativeRestore
      .then((path) => {
        if (client !== c || !c) return null;
        if (path) c.useManagerPath(path);
        return c.connect();
      })
      .then((ok) => {
        if (ok === null || client !== c || !c) return;
        if (ok) {
          if (c.protocolLevel() >= 4) void reconcileAgentLeases(c).catch(() => {});
          const file = startCtx.sessionManager.getSessionFile?.();
          const sid = startCtx.sessionManager.getSessionId();
          void promptIfHostedSession(c, () => ctx, { sessionFile: file, sessionId: sid }, "start").catch(() => {});
          void syncWithManager();
          void recoverCrashedSession(sid);
          return;
        }
        // A /reload may have replaced startCtx by now; report to the live one.
        const current = ctx;
        if (current?.hasUI) {
          const detail = c.lastError();
          const reason = detail ? ` (${detail})` : "";
          const searched = managerPath ? `using ${managerPath}` : `looked in: ${describeManagerSearch(config, home)}`;
          current.ui.notify(
            `pi-famulus unavailable${reason}; ${searched}. ` +
              "Bash runs locally without auto-backgrounding, task_*/monitor are disabled, " +
              "and subagents cannot run bash. Install it or set PI_FAMULUS_MANAGER_PATH (see README Install).",
            "warning",
          );
        }
      })
      .catch(() => {});
  });

  const teardown = async (): Promise<void> => {
    exitWatchdog.dispose();
    fleetWidget?.dispose();
    fleetWidget = null;
    comms.dispose(); // Release decision waiters before awaiting child aborts.
    await subagentRegistry?.disposeAll().catch(() => {});
    subagentRegistry = null;
    agentLoader = null;
    monitorRegistry?.disposeAll();
    notifyCenter?.dispose();
    cleanupSessionSpillFiles();
    const current = client;
    client = null;
    if (current) {
      try {
        if (current.isAvailable()) await current.shutdownSession();
      } catch {
        // Child teardown is best-effort; still release the manager connection.
      } finally {
        await current.close().catch(() => {});
      }
    }
    ctx = null;
  };
  const runtime: FamulusRuntime = { attach: (host) => binding.attach(host) };

  pi.on("session_before_switch", async (event, switchCtx) => {
    const file = event.targetSessionFile;
    if (!file || !client?.isAvailable()) return;
    const outcome = await promptIfHostedSession(client, () => switchCtx, { sessionFile: file }, "switch");
    if (outcome === "none" || outcome === "takeover") return;
    return { cancel: true };
  });

  pi.on("session_shutdown", async (event) => {
    if (event.reason === "reload" && ctx) {
      // Keep the session's shells, monitors and subagents running; the
      // reloaded instance adopts this runtime.
      parkedSessionId = ctx.sessionManager.getSessionId();
      fleetWidget?.dispose();
      fleetWidget = null;
      ctx = null;
      binding.detach();
      parkRuntime({ version: EXTENSION_VERSION, runtime, dispose: teardown });
      return;
    }
    if (String((ctx as { mode?: string } | null)?.mode) === "print") {
      const ids = [
        ...workIndex.list().filter((item) => item.status === "running" || item.status === "pending").map((item) => item.id),
        ...(subagentRegistry?.activeChildren() ?? []).map((child) => child.childId),
      ];
      const warning = printModeDropWarning([...new Set(ids)]);
      if (warning) process.stderr.write(warning);
    }
    await teardown();
  });

  pi.on("agent_settled", async () => {
    notifyCenter?.settled();
  });

  // A wake entering the model's context (steer / triggerTurn); see wake-delivery.ts.
  pi.on("message_end", async (event) => {
    const message = onWakeMessageEnd(event.message, {
      now: () => clock.now(),
      logEvent,
      lookup: () => (subagentRegistry ? registryStatusLookup(subagentRegistry) : null),
    });
    return message ? { message } : undefined;
  });

  return runtime;
}
