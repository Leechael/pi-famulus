/**
 * Poll guard: blocks task_output/task_list calls that only check on running
 * work with nothing new to read.
 *
 * In the 2026-11-07 log-monitor session (deepseek-flash) the agent made 258
 * task_output calls, 326 of them answered "no new output"; the result text
 * already told it to end its turn. In the 2026-11-08 agent-wait-ready and
 * monitor-not-sleep probes gpt-6-luna kept a monitor running and still called
 * task_list/task_output before the event came. The wait guard
 * (wait-guard.ts) moves sleeps to exactly these calls.
 *
 * A call is a poll when the work it looks at is still active and no wake
 * about it has arrived since the last look:
 *   task_output  on an active task, without a cursor past 0 (paging on from
 *                an earlier read stays allowed; max_bytes alone is a fresh
 *                read), and no wake for that task since the last read of it.
 *   task_list    while any work is active, and no wake, user or peer message
 *                since the last list or since work was last started (a list
 *                right after starting a task only checks that it runs).
 * Finished work, a wake's details and reads after a new message stay open.
 *
 * The call is refused, not the run: ending the run could leave an agent with
 * nothing that will wake it (a server it started without a monitor), so the
 * reason says to start a monitor in that case.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { famulusWakeDetails } from "./wake-delivery.ts";
import { wakeIds } from "./wake.ts";
import { fill } from "./prompts.generated.ts";

export interface PollGuardIndex {
  list(): { id: string; status: string; title?: string }[];
}

export interface PollGuardOptions {
  enabled: boolean;
  getIndex: () => PollGuardIndex | null;
  onBlock?: (info: { tool: string; taskId?: string }) => void;
}

const isActive = (status: string) => status === "pending" || status === "running";

/** A tool result that started background work (bash backgrounded, monitor started). */
const STARTED = /moved to background \(task_id: |Monitor started · task /;

function resultText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content.map((c: { type?: string; text?: string }) => (c?.type === "text" ? c.text ?? "" : "")).join("\n");
}

export function installPollGuard(pi: Pick<ExtensionAPI, "on">, opts: PollGuardOptions): void {
  if (!opts.enabled) return;
  /** Task ids with a wake not yet followed by a task_output read of them. */
  const news = new Set<string>();
  /** A wake, user or peer message arrived since the last task_list. */
  let listNews = true;

  pi.on("message_end", async (event) => {
    const message = event.message as { role?: string; content?: unknown };
    if (message.role === "user") { listNews = true; return; }
    if (message.role === "toolResult") {
      if (STARTED.test(resultText(message.content))) listNews = false;
      return;
    }
    const wake = famulusWakeDetails(event.message);
    if (wake) {
      listNews = true;
      for (const id of wakeIds(wake)) news.add(id);
      return;
    }
    if (message.role === "custom") listNews = true;
  });

  pi.on("tool_call", async (event) => {
    if (event.toolName !== "task_output" && event.toolName !== "task_list") return;
    const items = opts.getIndex()?.list() ?? [];
    const active = items.filter((i) => isActive(i.status));
    if (active.length === 0) return;
    const input = (event.input ?? {}) as Record<string, unknown>;

    if (event.toolName === "task_list") {
      if (listNews) { listNews = false; return; }
      opts.onBlock?.({ tool: "task_list" });
      return { block: true, reason: fill("guards.error.poll-list", { count: active.length }) };
    }

    const taskId = typeof input.task_id === "string" ? input.task_id : "";
    const target = active.find((i) => i.id === taskId);
    if (!target) return;
    if (typeof input.cursor === "number" && input.cursor > 0) return;
    if (news.delete(taskId)) return;
    opts.onBlock?.({ tool: "task_output", taskId });
    return { block: true, reason: fill("guards.error.poll-output", { id: taskId }) };
  });
}
