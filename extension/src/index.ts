/**
 * pi-famulus extension entry point (design doc §4).
 *
 * M1: bash override (auto-backgrounding) + task_* tools + manager client.
 * M2: NotifyCenter + monitor tool.
 * M3: subagent tool (InProcessRunner + tasks/chain + budget-to-async) + fleet widget.
 */
import { applyBehaviorGuidelines } from "./behavior-guidelines";
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
import { createComms, type CommsWithOrigin } from "./comms/comms";
import { createRegistryCommsHost } from "./comms/registry-host";
import { createAgentMessageTool, createContactSupervisorTool } from "./comms/tools";
import { registerReplyCommand } from "./comms/reply-command";
import { describeManagerSearch, getFamulusHome, loadConfig, resolveManagerPath, resolveSubagentConfig } from "./config";
import type { TaskExitInfo } from "./format";
import { ManagerClient, type ManagerEvent, type TaskRecord } from "./manager-client";
import { createMonitorTool, exitEventFromRecord, MonitorRegistry } from "./monitor";
import { admitAgentChild, reregisterAgentLeases } from "./subagent/admission";
import { NotifyCenter } from "./notify";
import { onWakeMessageEnd, registryStatusLookup } from "./wake-delivery";
import { createChildBashTool } from "./subagent/child-bash";
import { ChildShellTracker, createOverrunNotifier } from "./subagent/overrun";
import {
  agentEndReason,
  headOf,
  tailOf,
  writeAgentChildRecord,
  type AgentChildRecord,
} from "./subagent/agent-records";
import { TranscriptWriter } from "./subagent/transcript";
import { FleetWidget } from "./subagent/fleet-widget";
import { WorkIndex, type WorkItem } from "./work-index";
import { createPiSessionFn, modelCandidates } from "./subagent/pi-runtime";
import { SubagentRegistry } from "./subagent/registry";
import { InProcessRunner } from "./subagent/runner";
import { createSubagentTool } from "./subagent/tool";
import { createTaskListTool, createTaskOutputTool, createTaskStopTool } from "./task-tools";
import { registerFamulusMessageRenderers } from "./tui/message-renderers";
import { registerTasksCommand } from "./tui/tasks-command";
import { stderrPathFor } from "./tui/task-output-paths";
import { shellWakeTitle } from "./wake";

/** Read only the tail of a task output file for the notification preview (≤ maxChars). */
export function readPreview(outputPath: string | undefined, maxChars: number): string {
  if (!outputPath) return "";
  const tail = readFileTail(outputPath, Math.max(maxChars, maxChars * 4));
  if (!tail || !tail.text) return "";
  return tail.text.length <= maxChars ? tail.text : tail.text.slice(-maxChars);
}

function toExitStatus(event: ManagerEvent): TaskExitInfo["status"] {
  // The manager died without shutting down; its runner took the task down.
  if (event.end_reason === "manager-crash") return "orphaned";
  if (event.signal) return "killed";
  if (event.exit_code === 0) return "completed";
  return "failed";
}

export default function (pi: ExtensionAPI): void {
  const home = getFamulusHome();
  const config = loadConfig(home);
  const managerPath = resolveManagerPath(config, home);
  const clock = realClock;

  // Claude-style transcript pills for notifications (TUI only; no-ops elsewhere).
  registerFamulusMessageRenderers(pi);

  let ctx: ExtensionContext | null = null;
  let client: ManagerClient | null = null;
  const globalAgentLeases = new Set<string>();
  const pendingAgentLeases = new Set<string>();
  let leaseReconciliation: Promise<void> | null = null;
  const reconcileAgentLeases = (manager: ManagerClient): Promise<void> => {
    if (leaseReconciliation) return leaseReconciliation;
    leaseReconciliation = reregisterAgentLeases(
      manager,
      globalAgentLeases,
      undefined,
      pendingAgentLeases,
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
    monitorRegistry?.reconcile(tasks);
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
  pi.registerTool(createMonitorTool(monitorRegistry));
  registerTasksCommand(pi, {
    getRegistry: () => subagentRegistry,
    getIndex: () => workIndex,
    getClient: () => client,
    home,
    sessionId: () => ctx?.sessionManager.getSessionId() ?? "",
    clock,
    syncWithManager: deps.syncWithManager,
  });
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
  const commsHost = createRegistryCommsHost({
    getRegistry: () => subagentRegistry,
    getNotifyCenter: () => notifyCenter,
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
      resolveAgent: (name) => {
        const loader = agentLoader;
        if (!loader) return resolveAgentDef([], name); // builtins-only fallback
        return resolveAgentDef(loader.reload().definitions, name);
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

  pi.on("session_start", async (_event, startCtx) => {
    ctx = startCtx;
    const base = (
      startCtx as { getSystemPromptOptions?: () => { sections?: Record<string, string> } }
    ).getSystemPromptOptions?.();
    if (base) applyBehaviorGuidelines(base);
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
    subagentRegistry?.disposeAll();
    subagentRegistry = null;
    globalAgentLeases.clear();

    client = new ManagerClient({
      home,
      sessionId: startCtx.sessionManager.getSessionId(),
      managerPath,
      cwd: startCtx.cwd,
      clock,
      log: () => {}, // keep quiet; degradation is surfaced via tools
    });

    client.onEvent((event) => {
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
        monitorRegistry?.handleOutput(event.task_id, event.chunk, event.next_cursor);
        return;
      }
      if (event.event === "task_exited" && event.task_id) {
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
      // A closed socket drops daemon-side permits. Treat all local leases as
      // pending until the reconnect has re-acquired them.
      for (const childId of globalAgentLeases) pendingAgentLeases.add(childId);
      globalAgentLeases.clear();
      void monitorRegistry?.rewatchAll().then(() => syncWithManager());
      void (async () => {
        const manager = client;
        if (!manager || manager.protocolLevel() < 4) {
          capacityNotice("daemon too old");
          return;
        }
        // The registry and its child ids survive socket reconnects in this
        // extension process; held ids include resumable interrupted children.
        await reconcileAgentLeases(manager);
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
    // The foreground shell each child is blocked on, for its overrun wake.
    const childShells = new ChildShellTracker();
    const createSession = createPiSessionFn({
      getModelRegistry: () => ctx?.modelRegistry ?? null,
      getModelRuntime: () => {
        const registry = ctx?.modelRegistry as { runtime?: unknown } | null | undefined;
        return registry?.runtime;
      },
      getParentModel: () => ctx?.model,
      getParentThinkingLevel: () => ctx?.thinkingLevel,
      getScopedModels: () => ctx?.scopedModels ?? [],
      getCwd: () => ctx?.cwd ?? process.cwd(),
      customTools: (req) => {
        const tools: Array<ToolDefinition<any, any, any>> = [
          // M4: every child can reach the supervisor and its siblings.
          createContactSupervisorTool(comms, req.childId),
          createAgentMessageTool(comms, { kind: "child", childId: req.childId, runId: req.runId }, commsHost, clock),
        ];
        // The no-background bash variant replaces the built-in bash inside
        // child sessions (custom tools override builtins by name).
        if (req.agent.tools.includes("bash")) {
          tools.push(
            createChildBashTool({
              getClient: () => client,
              home,
              sessionId: () => ctx?.sessionManager.getSessionId() ?? "",
              sessionEnv: () => (ctx ? sessionEnv(ctx) : {}),
              trackTask,
              childId: req.childId,
              runId: req.runId,
              clock,
              shells: childShells,
            }),
          );
        }
        return tools;
      },
    });
    const transcripts = new TranscriptWriter(home, clock);
    const syncTranscript = (childId: string): string | undefined => {
      const handle = registry.handle(childId);
      if (!handle) return undefined;
      return transcripts.sync(startCtx.sessionManager.getSessionId(), childId, handle.conversation());
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
        reserveLocal: () => registry.reserveChildSlot(req.childId, ticket),
        manager: client,
        leases: globalAgentLeases,
        pendingLeases: pendingAgentLeases,
        ticket,
        notice: capacityNotice,
      }),
      onActivity: (childId) => {
        syncTranscript(childId);
      },
      onStall: (childId, attempt) => {
        // One event per stall detection: an auto-resume follows unless the
        // retry budget (stallRetries) is already spent.
        logEvent("agent.stall", { child_id: childId, attempt });
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
    const sessionIdForAgents = () => startCtx.sessionManager.getSessionId();
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
            ...(c.result?.stalls ? { stalls: c.result.stalls } : {}),
            duration_ms: c.result?.durationMs ?? Math.max(0, clock.now() - c.startedAt),
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
          ...(c.model !== undefined ? { model: c.model } : {}),
          status: c.status,
          started_at: c.startedAt,
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
          ...(c.model !== undefined ? { model: c.model } : {}),
          cwd: startCtx.cwd,
          ...(c.prompt !== undefined ? { prompt: c.prompt } : {}),
          ...(c.preamble !== undefined ? { preamble: c.preamble } : {}),
          ...(c.result?.text ? { text: c.result.text } : {}),
          ...(c.result?.error ? { error: c.result.error } : {}),
        });
      }
    });
    if (startCtx.hasUI) {
      fleetWidget = new FleetWidget({
        index: workIndex,
        getUi: () => (ctx?.hasUI ? (ctx.ui as never) : null),
        clock,
      });
      fleetWidget.start();
    }

    // Connect in the background: a cold manager spawn must not stall the
    // first prompt. Tools call ensureAvailable() lazily before use.
    const c = client;
    void c
      .connect()
      .then((ok) => {
        if (ok) {
          if (c.protocolLevel() >= 4) void reconcileAgentLeases(c).catch(() => {});
          return;
        }
        if (startCtx.hasUI) {
          const detail = c.lastError();
          const reason = detail ? ` (${detail})` : "";
          const searched = managerPath ? `using ${managerPath}` : `looked in: ${describeManagerSearch(config, home)}`;
          startCtx.ui.notify(
            `pi-famulus unavailable${reason}; ${searched}. ` +
              "Bash runs locally without auto-backgrounding, task_*/monitor are disabled, " +
              "and subagents cannot run bash. Install it or set PI_FAMULUS_MANAGER_PATH (see README Install).",
            "warning",
          );
        }
      })
      .catch(() => {});
  });

  pi.on("session_shutdown", async () => {
    exitWatchdog.dispose();
    fleetWidget?.dispose();
    fleetWidget = null;
    subagentRegistry?.disposeAll();
    subagentRegistry = null;
    agentLoader = null;
    comms.dispose(); // resolve orphaned need_decision waiters
    monitorRegistry?.disposeAll();
    notifyCenter?.dispose();
    const current = client;
    client = null;
    if (current) {
      if (current.isAvailable()) {
        await current.shutdownSession().catch(() => {});
      }
      await current.close();
    }
    ctx = null;
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

  pi.on("before_agent_start", async (event) => {
    applyBehaviorGuidelines(event.systemPromptOptions as { sections?: Record<string, string> });
  });
}
