/**
 * /tasks — one live overlay for shells, monitors, and subagents.
 *
 * Type to filter, ↑↓ selects, Enter opens, Ctrl+X asks before stopping, Tab
 * switches between active/recent and the manager's complete task history.
 */
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { realClock, type Clock } from "../clock";
import type { ManagerClient, TaskRecord } from "../manager-client";
import { taskOutputPath } from "../config";
import { formatConversation } from "../subagent/conversation";
import type { SubagentRegistry } from "../subagent/registry";
import { formatAge, nextAgeBoundary, type WorkIndex, type WorkItem } from "../work-index";
import { keyHints, notifyPlainFallback, showScrollDetail, spread, type PaneRenderer, type PaneTheme } from "./scroll-detail-view";
import { renderConversation } from "./conversation-view";
import { readTaskFileTailCached, stderrPathFor } from "./task-output-paths";
import { fitLines, loadPiTui, truncateToWidth } from "./pi-tui-load";
import { statusGlyph } from "./tool-component";


function isLive(item: WorkItem): boolean {
  return item.status === "pending" || item.status === "running";
}

/** Selection follows the item id so a re-sort cannot move the cursor. */
export function selectedIndex(items: readonly WorkItem[], selectedId: string | undefined): number {
  if (items.length === 0 || !selectedId) return 0;
  const index = items.findIndex((item) => item.id === selectedId);
  return index >= 0 ? index : 0;
}

export function moveSelection(
  items: readonly WorkItem[],
  selectedId: string | undefined,
  dir: -1 | 1,
): string | undefined {
  if (items.length === 0) return undefined;
  const index = selectedIndex(items, selectedId);
  return items[Math.max(0, Math.min(items.length - 1, index + dir))]?.id;
}

export function stopChoice(
  items: readonly WorkItem[],
  selectedId: string | undefined,
): { action: "stop" | "already-finished" | "none"; id?: string } {
  if (items.length === 0) return { action: "none" };
  const item = items[selectedIndex(items, selectedId)];
  if (!item) return { action: "none" };
  if (!isLive(item)) return { action: "already-finished", id: item.id };
  return { action: "stop", id: item.id };
}

export interface TasksCommandDeps {
  getRegistry: () => SubagentRegistry | null;
  getIndex: () => WorkIndex | null;
  getClient: () => ManagerClient | null;
  home?: string;
  sessionId?: () => string;
  clock?: Clock;
  syncWithManager?: () => Promise<unknown>;
}

export function registerTasksCommand(pi: ExtensionAPI, deps: TasksCommandDeps): void {
  pi.registerCommand("tasks", {
    description: "List and inspect background work",
    handler: async (_args, ctx) => openTasksUi(ctx, deps),
  });
}

type TaskListRow = { type: "run"; runId: string; count: number } | { type: "task"; item: WorkItem; indent: boolean };

export function groupTaskRows(items: readonly WorkItem[]): TaskListRow[] {
  const runs = new Map<string, WorkItem[]>();
  for (const item of items) {
    if (item.runId) runs.set(item.runId, [...(runs.get(item.runId) ?? []), item]);
  }
  const emitted = new Set<string>();
  const rows: TaskListRow[] = [];
  for (const item of items) {
    if (!item.runId) {
      rows.push({ type: "task", item, indent: false });
      continue;
    }
    if (!emitted.has(item.runId)) {
      emitted.add(item.runId);
      rows.push({ type: "run", runId: item.runId, count: runs.get(item.runId)?.length ?? 1 });
      for (const child of runs.get(item.runId) ?? []) rows.push({ type: "task", item: child, indent: true });
    }
  }
  return rows;
}

export function filterTaskItems(items: readonly WorkItem[], query: string): WorkItem[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return [...items];
  return items.filter((item) =>
    [item.id, item.kind, item.status, item.title, item.runId, item.endReason, item.error]
      .filter(Boolean)
      .some((value) => value!.toLocaleLowerCase().includes(needle)),
  );
}

function itemFromTask(task: TaskRecord): WorkItem {
  return {
    id: task.task_id,
    kind: task.kind === "monitor" ? "monitor" : "shell",
    status: task.status,
    title: task.command.replace(/\s+/g, " ").trim() || task.task_id,
    command: task.command,
    cwd: task.cwd,
    startedAt: task.started_at,
    ...(task.ended_at !== null ? { endedAt: task.ended_at } : {}),
    exitCode: task.exit_code,
    ...(task.signal ? { signal: task.signal } : {}),
    ...(task.end_reason ? { endReason: task.end_reason } : {}),
    outputPath: task.output_path,
    stderrPath: stderrPathFor(task.output_path),
    countsAsWorker: task.kind === "shell" && task.backgrounded_at !== undefined,
    ...(task.origin?.via === "child-bash" ? { runId: task.origin.run_id } : {}),
  };
}

function mergeItems(primary: readonly WorkItem[], extra: readonly WorkItem[]): WorkItem[] {
  const merged = new Map(extra.map((item) => [item.id, item]));
  for (const item of primary) merged.set(item.id, item);
  return [...merged.values()].sort((a, b) => {
    const aLive = isLive(a);
    const bLive = isLive(b);
    return aLive !== bLive ? (aLive ? -1 : 1) : b.startedAt - a.startedAt;
  });
}

export function formatWorkRows(
  items: readonly WorkItem[],
  selectedId: string | undefined,
  now: number,
  width: number,
): string[] {
  if (items.length === 0) return [truncateToWidth("No background tasks.", Math.max(1, width), "")];
  const selected = selectedIndex(items, selectedId);
  return items.map((item, index) => {
    const mark = index === selected ? "▸" : " ";
    const { glyph } = statusGlyph(item.status);
    const age = formatAge(item.startedAt, item.endedAt, now);
    const terminal = item.exitCode !== undefined && item.exitCode !== null
      ? ` exit=${item.exitCode}`
      : item.signal ? ` ${item.signal}` : item.endReason ? ` ${item.endReason}` : "";
    const title = item.error ? `${item.title} · ${item.error}` : item.title;
    const kind = width >= 60 ? `${item.kind.padEnd(7)} ` : "";
    return truncateToWidth(`${mark} ${glyph} ${kind}${item.status.padEnd(10)} ${age.padEnd(6)}${terminal} ${title}`, Math.max(1, width), "…");
  });
}

export interface TaskListChoice {
  action: "close" | "view" | "stop" | "already-finished";
  id?: string;
}

async function openTasksUi(ctx: ExtensionContext, deps: TasksCommandDeps): Promise<void> {
  await deps.syncWithManager?.();
  const index = deps.getIndex();
  const clock = deps.clock ?? realClock;
  const currentItems = () => index?.list(clock.now()) ?? [];
  const loadAll = async (): Promise<WorkItem[]> => {
    const client = deps.getClient();
    if (!client || !(await client.ensureAvailable())) throw new Error("pi-famulus is unavailable");
    return (await client.list(true)).map(itemFromTask);
  };
  if (!ctx.hasUI) {
    const list = currentItems();
    ctx.ui.notify(list.length ? formatWorkRows(list, undefined, clock.now(), 100).join("\n") : "No background tasks.", "info");
    return;
  }
  if (!index) {
    ctx.ui.notify("No background tasks.", "info");
    return;
  }
  const manager = deps.getClient();
  if (currentItems().length === 0 && !(manager?.isAvailable() ?? false)) {
    ctx.ui.notify(
      "No background tasks. pi-famulus is unavailable, so bash runs locally and nothing is backgrounded (see the startup warning).",
      "warning",
    );
    return;
  }

  let selectedId: string | undefined;
  for (;;) {
    const choice = await showTaskList(
      ctx,
      currentItems,
      (cb) => index.onChange(cb),
      selectedId,
      clock,
      loadAll,
      deps,
    );
    if (!choice || choice.action === "close") return;
    selectedId = choice.id ?? selectedId;
    if (!choice.id) continue;
    const item = index.get(choice.id);
    if (choice.action === "already-finished") {
      ctx.ui.notify(`${item?.id ?? choice.id} already finished`, "info");
      continue;
    }
    if (choice.action === "stop") {
      try {
        if (item) await stopItem(item, deps);
        ctx.ui.notify(`Stopped ${choice.id}`, "info");
      } catch (err) {
        ctx.ui.notify(err instanceof Error ? err.message : String(err), "error");
      }
      continue;
    }
    if (item) await viewItem(ctx, item, deps);
  }
}

async function showTaskList(
  ctx: ExtensionContext,
  getItems: () => WorkItem[],
  subscribe: (cb: () => void) => () => void,
  initialSelectedId: string | undefined,
  clock: Clock,
  loadAll: () => Promise<WorkItem[]>,
  deps: TasksCommandDeps,
): Promise<TaskListChoice | undefined> {
  const piTui = loadPiTui();
  if (!piTui) {
    const items = getItems();
    notifyPlainFallback(ctx.ui.notify.bind(ctx.ui), "Background tasks", items.length ? formatWorkRows(items, initialSelectedId, clock.now(), 100).join("\n") : "No background tasks.");
    return { action: "close" };
  }
  const { matchesKey } = piTui;

  return ctx.ui.custom<TaskListChoice | undefined>(
    (tui, theme, kb, done) => {
      let selectedId = initialSelectedId;
      let filter = "";
      let scope: "active-recent" | "all" = "active-recent";
      let allItems: WorkItem[] = [];
      let allLoaded = false;
      let loadingAll = false;
      let confirmId: string | undefined;
      let hint = "";
      // formatAge rounds (now - startedAt) to the nearest second, so each
      // live row's visible age flips on half-second boundaries relative to
      // its own startedAt. Schedule the earliest pending flip and
      // re-evaluate after every tick and every index change.
      let ageTimer: unknown = null;
      const clearAgeTimer = () => {
        if (ageTimer === null) return;
        clock.clearTimeout(ageTimer);
        ageTimer = null;
      };
      const scheduleAgeTick = () => {
        clearAgeTimer();
        const now = clock.now();
        let next = Infinity;
        for (const item of visibleItems()) {
          if (!isLive(item)) continue;
          next = Math.min(next, nextAgeBoundary(now, item.startedAt));
        }
        if (!Number.isFinite(next)) return;
        ageTimer = clock.setTimeout(() => {
          ageTimer = null;
          tui.requestRender();
          scheduleAgeTick();
        }, next - now);
        clock.unref?.(ageTimer);
      };
      const unsub = subscribe(() => {
        tui.requestRender();
        scheduleAgeTick();
      });
      const pageSize = () => Math.max(1, (tui.terminal?.rows ?? 24) - 8);
      const matches = (data: string, id: "tui.select.up" | "tui.select.down" | "tui.select.confirm" | "tui.select.cancel") => {
        try { return kb.matches(data, id); } catch { return false; }
      };
      const visibleItems = () => filterTaskItems(
        scope === "all" ? mergeItems(getItems(), allItems) : getItems(),
        filter,
      );
      scheduleAgeTick();
      const choose = (id: string | undefined) => {
        selectedId = id;
        hint = "";
        tui.requestRender();
      };
      const requestStop = async () => {
        const item = visibleItems().find((candidate) => candidate.id === confirmId);
        if (!item || !isLive(item)) {
          hint = "Already finished — select another task.";
          confirmId = undefined;
          tui.requestRender();
          return;
        }
        try {
          await stopItem(item, deps);
          hint = `Stop requested for ${item.id}.`;
        } catch (err) {
          hint = err instanceof Error ? err.message : String(err);
        }
        confirmId = undefined;
        tui.requestRender();
      };
      const startLoadingAll = () => {
        if (allLoaded || loadingAll) return;
        loadingAll = true;
        hint = "Loading manager history…";
        tui.requestRender();
        void loadAll().then((items) => {
          allItems = items;
          allLoaded = true;
          hint = "";
        }).catch((err) => {
          hint = `Manager unavailable: ${err instanceof Error ? err.message : String(err)}`;
        }).finally(() => {
          loadingAll = false;
          tui.requestRender();
        });
      };
      return {
        render(width: number) {
          const t = theme as unknown as PaneTheme;
          const dim = (text: string) => t.fg("dim", text);
          const items = visibleItems();
          if (!items.some((item) => item.id === selectedId)) selectedId = items[0]?.id;
          const rows = groupTaskRows(items);
          const selectedRow = Math.max(0, rows.findIndex((row) => row.type === "task" && row.item.id === selectedId));
          const selectedTask = selectedIndex(items, selectedId);
          const height = pageSize();
          const start = Math.max(0, Math.min(selectedRow, rows.length - height));
          const window = rows.slice(start, start + height);
          const inner = Math.max(1, width);
          const active = items.filter(isLive).length;
          const scopeLabel = scope === "all" ? "all history" : "active + recent";
          const rule = t.fg("borderMuted", "─".repeat(inner));
          const counts = `${active > 0 ? t.fg("success", `${active} active`) : dim("0 active")}${dim(` · ${items.length} shown · ${scopeLabel}`)}`;
          const title = spread(`${t.fg("accent", "◆")} ${t.bold("Tasks")}`, counts, inner);
          const prompt = filter
            ? `${t.fg("accent", "›")} ${filter}${t.fg("accent", "▏")}`
            : `${t.fg("accent", "›")} ${dim("type to filter")}`;
          const notice = confirmId
            ? `  ${t.bold(t.fg("error", `■ stop ${confirmId}?`))}  ${keyHints(t, [["enter", "confirm"], ["esc", "cancel"]])}`
            : hint
              ? `  ${t.fg(hint.startsWith("Already") || hint.startsWith("Stop requested") ? "dim" : "warning", hint)}`
              : "";
          const kindWidth = width >= 60 ? 8 : 0;
          const body = window.map((row) => {
            if (row.type === "run") {
              return truncateToWidth(`  ${dim("▾")} ${t.fg("muted", row.runId)} ${dim(`· ${row.count} child${row.count === 1 ? "" : "ren"}`)}`, inner, "…");
            }
            const item = row.item;
            const selected = item.id === selectedId;
            const { color, glyph } = statusGlyph(item.status);
            const age = formatAge(item.startedAt, item.endedAt, clock.now());
            const terminal = item.exitCode !== undefined && item.exitCode !== null
              ? `exit=${item.exitCode}`
              : item.signal ?? item.endReason ?? "";
            const name = item.error ? `${item.title} ${t.fg("error", `· ${item.error}`)}` : item.title;
            const lead = (selected ? t.fg("accent", "▌") : " ") + (row.indent ? "   " : " ");
            const kind = kindWidth ? t.fg("muted", item.kind.padEnd(kindWidth)) : "";
            const status = t.fg(isLive(item) ? "success" : color, item.status.padEnd(10));
            const left = `${lead}${t.fg(color, glyph)} ${kind}${status}${selected ? t.bold(name) : name}`;
            const right = dim(`${terminal ? `${terminal}  ` : ""}${age}`);
            return spread(left, right, inner);
          });
          if (body.length === 0) body.push(`  ${dim(filter ? "No task matches the filter." : "No background tasks.")}`);
          const place = items.length ? dim(`${selectedTask + 1} of ${items.length}`) : "";
          const keys = keyHints(t, [["↑↓", "select"], ["enter", "open"], ["ctrl+x", "stop"], ["tab", "scope"], ["esc", "close"]]);
          return [rule, title, prompt, ...(notice ? [truncateToWidth(notice, inner, "…")] : []), "", ...body, "", spread(`  ${keys}`, place, inner), rule];
        },
        invalidate() {},
        handleInput(data: string) {
          const items = visibleItems();
          const current = items[selectedIndex(items, selectedId)];
          if (matchesKey(data, "escape") || data === "\u001b") {
            if (confirmId) { confirmId = undefined; hint = "Stop cancelled."; tui.requestRender(); return; }
            if (filter) { filter = ""; hint = ""; tui.requestRender(); return; }
            done({ action: "close", id: selectedId });
            return;
          }
          if (confirmId) {
            if (matchesKey(data, "tui.select.confirm") || data === "\r" || data === "\n") { void requestStop(); return; }
            return;
          }
          if (matches(data, "tui.select.up")) { choose(moveSelection(items, selectedId, -1) ?? selectedId); return; }
          if (matches(data, "tui.select.down")) { choose(moveSelection(items, selectedId, 1) ?? selectedId); return; }
          if (matches(data, "tui.select.confirm") || data === "\r" || data === "\n") {
            if (current) done({ action: "view", id: current.id });
            return;
          }
          if (matchesKey(data, "ctrl+x") || data === "\u0018") {
            if (current && !isLive(current)) { hint = "Already finished — select another task."; tui.requestRender(); return; }
            if (current) { confirmId = current.id; hint = ""; tui.requestRender(); }
            return;
          }
          if (matchesKey(data, "tab") || data === "\t") {
            scope = scope === "active-recent" ? "all" : "active-recent";
            if (scope === "all") startLoadingAll();
            selectedId = visibleItems()[0]?.id;
            tui.requestRender();
            return;
          }
          if (matchesKey(data, "pageUp")) { choose(items[Math.max(0, selectedIndex(items, selectedId) - Math.max(1, pageSize() - 2))]?.id); return; }
          if (matchesKey(data, "pageDown")) { choose(items[Math.min(items.length - 1, selectedIndex(items, selectedId) + Math.max(1, pageSize() - 2))]?.id); return; }
          if (matchesKey(data, "home")) { choose(items[0]?.id); return; }
          if (matchesKey(data, "end")) { choose(items.at(-1)?.id); return; }
          if (data === "\u007f" || data === "\b") { filter = filter.slice(0, -1); selectedId = visibleItems()[0]?.id; hint = ""; tui.requestRender(); return; }
          if (data === "\u0015") { filter = ""; selectedId = visibleItems()[0]?.id; hint = ""; tui.requestRender(); return; }
          if (!data.startsWith("\u001b") && [...data].every((char) => char >= " " && char !== "\u007f")) {
            filter += data;
            selectedId = visibleItems()[0]?.id;
            hint = "";
            tui.requestRender();
          }
        },
        dispose() {
          clearAgeTimer();
          unsub();
        },
      };
    },
    // Bottom sheet over the editor, where the user typed /tasks; the list is
    // content-sized, so a top anchor left it at the far end of the screen.
    { overlay: true, overlayOptions: { anchor: "bottom-left", width: "100%", maxHeight: "100%", margin: 0 } },
  );
}

async function stopItem(item: WorkItem, deps: TasksCommandDeps): Promise<void> {
  if (item.kind === "agent") {
    const handle = deps.getRegistry()?.handle(item.id);
    if (!handle) throw new Error(`no live subagent ${item.id}`);
    await handle.interrupt();
    return;
  }
  const client = deps.getClient();
  if (!client || !(await client.ensureAvailable())) throw new Error("pi-famulus is not available");
  await client.stop(item.id, "tui");
}

export function taskDetailHeader(item: WorkItem, now: number): string {
  const outcome = item.exitCode !== undefined && item.exitCode !== null
    ? `exit ${item.exitCode}`
    : item.signal ?? item.endReason ?? item.status;
  return `${item.id} · ${item.kind} · ${outcome} · ${formatAge(item.startedAt, item.endedAt, now)} · ${item.cwd ?? "cwd unavailable"}\n$ ${item.command ?? item.title}`;
}

export function resolveTaskOutputPath(item: WorkItem, home: string | undefined, sessionId: string | undefined): string {
  return item.outputPath || (home && sessionId ? taskOutputPath(home, sessionId, item.id) : "");
}

export function taskDetailInfo(item: WorkItem, now: number): string {
  const lines = [taskDetailHeader(item, now)];
  if (item.kind === "monitor" && item.command && item.title !== item.command) lines.push(`Watching: ${item.title}`);
  if (item.endReason) lines.push(`Ended: ${item.endReason}`);
  if (item.outputPath) lines.push(`Output: ${item.outputPath}`);
  if (item.stderrPath) lines.push(`Stderr: ${item.stderrPath}`);
  if (item.runId) lines.push(`Run: ${item.runId}`);
  if (item.agent) lines.push(`Agent: ${item.agent}`);
  if (item.model) lines.push(`Model: ${item.model}`);
  if (item.prompt) lines.push(`Task prompt (user-authored):\n${item.prompt}`);
  if (item.preamble) lines.push(`Agent preamble (injected):\n${item.preamble}`);
  if (item.error) lines.push(`Error: ${item.error}`);
  return lines.join("\n");
}

function isLiveStatus(status: string): boolean {
  return status === "running" || status === "pending";
}

/**
 * Tab bodies for a shell or monitor. `getItem` is read on every poll so the
 * header follows the task (a view opened while it ran must not keep saying
 * "running"), and an empty file says why it is empty.
 */
export function taskDetailTabs(
  getItem: () => WorkItem,
  paths: { outputPath: string; stderrPath: string },
  now: () => number,
): { output: () => string; stderr: () => string; info: () => string } {
  const body = (path: string, emptyLive: string, emptyDone: string) => {
    const text = readTaskFileTailCached(path);
    if (text !== "(empty)") return text;
    return isLiveStatus(getItem().status) ? emptyLive : emptyDone;
  };
  return {
    output: () => `${taskDetailHeader(getItem(), now())}\n\n${body(paths.outputPath, "(no output yet)", "(no output; the command printed nothing)")}`,
    stderr: () => `${taskDetailHeader(getItem(), now())}\n\n${body(paths.stderrPath, "(nothing on stderr yet)", "(nothing on stderr)")}`,
    info: () => taskDetailInfo({ ...getItem(), ...paths }, now()),
  };
}

async function viewItem(ctx: ExtensionContext, item: WorkItem, deps: TasksCommandDeps): Promise<void> {
  if (!ctx.hasUI) return;
  const clock = deps.clock ?? realClock;
  if (item.kind === "agent") {
    const handle = () => deps.getRegistry()?.handle(item.id);
    const conversation = () => handle()?.conversation() ?? [];
    const conversationText = () => {
      const turns = conversation();
      return turns.length > 0 ? formatConversation(turns) : item.prompt ? `Task prompt:\n${item.prompt}` : "(no conversation yet)";
    };
    const thread: PaneRenderer = {
      render(width, theme, state) {
        const turns = conversation();
        const shown = turns.length > 0 ? turns : item.prompt ? [{ role: "user", text: item.prompt }] : [];
        return renderConversation(shown, width, theme, state);
      },
    };
    const resultText = () => {
      const latest = [...conversation()].reverse().find((turn) => turn.role === "assistant")?.text;
      return latest ?? item.text ?? "(no result yet)";
    };
    const infoText = () => taskDetailInfo(deps.getIndex()?.get(item.id) ?? item, clock.now());
    try {
      await showScrollDetail(ctx.ui, {
        title: `subagent · ${item.title}`,
        tabs: {
          conversation: thread,
          result: resultText,
          info: infoText,
        },
        pollMs: 500,
        clock: deps.clock,
      });
    } catch {
      notifyPlainFallback(ctx.ui.notify.bind(ctx.ui), item.title, `${infoText()}\n\n${conversationText()}`);
    }
    return;
  }
  const outputPath = resolveTaskOutputPath(item, deps.home, deps.sessionId?.());
  const stderrPath = item.stderrPath || stderrPathFor(outputPath);
  const tabs = taskDetailTabs(() => deps.getIndex()?.get(item.id) ?? item, { outputPath, stderrPath }, () => clock.now());
  const infoText = tabs.info;
  try {
    await showScrollDetail(ctx.ui, {
      title: `${item.kind} · ${item.title}`,
      tabs,
      pollMs: 500,
      clock: deps.clock,
    });
  } catch {
    notifyPlainFallback(ctx.ui.notify.bind(ctx.ui), item.title, `${infoText()}\n\n${readTaskFileTailCached(outputPath)}`);
  }
}
