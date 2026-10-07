/**
 * Child-session bash variant (design doc §4.2 tail / §4.6).
 *
 * Registered as a custom tool named "bash" inside subagent sessions (it
 * overrides the built-in there). Differences from the main bash override:
 * - no run_in_background: subagents must not background work — the schema
 *   omits the flag entirely (lain waitUntilExit lesson);
 * - execute waits for the whole timeout via manager start + wait; on expiry
 *   the process is killed (SIGTERM->SIGKILL by the manager) and a timeout
 *   error is returned instead of backgrounding;
 * - bare sleep / idle-loop commands are rejected (same patterns as the main
 *   bash override; the detection logic is copied, not imported, per the M3
 *   file-ownership rules);
 * - no local fallback: when the manager is unavailable the tool fails with a
 *   clear error (subagent sessions are spawned by the extension where the
 *   manager is expected to be up).
 */
import { Type } from "typebox";
import type {
  AgentToolResult,
  BashToolDetails,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { taskOutputPath } from "../config";
import { realClock, type Clock } from "../clock";
import type { ManagerClient } from "../manager-client";
import type { ChildShellTracker } from "./overrun";
import { fill, PROMPTS } from "../prompts.generated";
import {
  appendStatus,
  bareSleepError,
  collectOutput,
  formatFinishedOutput,
  killedStatus,
  timedOutStatus,
  SHELL_MAX_BYTES,
  SHELL_MAX_LINES,
  withAbort,
} from "../shell-exec";

const MAX_LINES = SHELL_MAX_LINES;
const MAX_BYTES = SHELL_MAX_BYTES;
/** Wait granularity when no explicit timeout is given (abort responsiveness). */
const WAIT_SLICE_MS = 60_000;

const childBashParameters = Type.Object({
  command: Type.String({ description: PROMPTS["tools.child.bash.param.command"] }),
  timeout: Type.Optional(Type.Number({ description: PROMPTS["tools.child.bash.param.timeout"] })),
});

type ChildBashParams = { command: string; timeout?: number };

export interface FamulusChildBashDetails extends BashToolDetails {
  task_id?: string;
}

export interface ChildBashDeps {
  getClient: () => ManagerClient | null;
  home: string;
  /** Parent session id — the manager namespace this connection owns. */
  sessionId: () => string;
  /** Precomputed PI_* env injection (from the parent session). */
  sessionEnv: () => Record<string, string>;
  trackTask: (taskId: string, meta: { kind: string; command: string; cwd?: string }) => void;
  childId?: string;
  runId?: string;
  clock?: Clock;
  /** Records the shell this child is blocked on, for its overrun wake. */
  shells?: Pick<ChildShellTracker, "start" | "end">;
}

function rejectBareSleep(command: string): string | null {
  return bareSleepError(command, "tools.child.bash.error.bare-sleep");
}

function resolveTimeoutMs(timeoutSeconds: number | undefined): number | null {
  if (timeoutSeconds === undefined) return null;
  if (!Number.isFinite(timeoutSeconds) || timeoutSeconds <= 0) {
    throw new Error("Invalid timeout: must be a finite number of seconds");
  }
  return Math.round(timeoutSeconds * 1000);
}

function fullEnv(deps: ChildBashDeps): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined) env[key] = value;
  }
  Object.assign(env, deps.sessionEnv());
  return env;
}

export function createChildBashTool(
  deps: ChildBashDeps,
): ToolDefinition<typeof childBashParameters, FamulusChildBashDetails | undefined> {
  return {
    name: "bash",
    label: "Bash",
    description: fill("tools.child.bash.description", { maxLines: MAX_LINES, maxKb: MAX_BYTES / 1024 }),
    promptSnippet: PROMPTS["tools.child.bash.snippet"],
    parameters: childBashParameters,
    async execute(_toolCallId, params, signal, _onUpdate, ctx: ExtensionContext) {
      const input = params as ChildBashParams;

      const sleepError = rejectBareSleep(input.command);
      if (sleepError) throw new Error(sleepError);

      const client = deps.getClient();
      if (!client || !(await client.ensureAvailable())) {
        throw new Error("pi-famulus is not available; bash is disabled inside this subagent");
      }

      const timeoutMs = resolveTimeoutMs(input.timeout);
      const start = await client.start({
        kind: "shell",
        command: input.command,
        cwd: ctx.cwd,
        env: fullEnv(deps),
        run_in_background: false,
        timeout_ms: timeoutMs,
        ...(deps.childId && deps.runId
          ? { origin: { via: "child-bash" as const, child_id: deps.childId, run_id: deps.runId } }
          : {}),
      });
      deps.trackTask(start.task_id, { kind: "shell", command: input.command, cwd: ctx.cwd });
      const outputPath = taskOutputPath(deps.home, deps.sessionId(), start.task_id);

      const clock = deps.clock ?? realClock;
      const childId = deps.childId;
      if (childId) {
        deps.shells?.start(childId, {
          taskId: start.task_id,
          command: input.command,
          startedAt: clock.now(),
          outputPath,
        });
      }
      try {
        const deadline = timeoutMs !== null ? clock.now() + timeoutMs : null;
        let waitResult: { done: boolean; exit_code?: number | null } | null = null;
        for (;;) {
          const budget =
            deadline === null
              ? WAIT_SLICE_MS
              : Math.min(WAIT_SLICE_MS, Math.max(1, deadline - clock.now()));
          try {
            waitResult = await withAbort(client.wait(start.task_id, budget), signal, () => {
              client.stop(start.task_id, "tool").catch(() => {});
            });
          } catch (err) {
            if ((err as Error).message === "aborted") {
              throw new Error("Command aborted (task stopped)");
            }
            throw new Error(
              `Lost contact with pi-famulus while waiting for task ${start.task_id}: ` +
                `${(err as Error).message}. Output so far: ${outputPath}.`,
            );
          }
          if (waitResult.done) break;
          if (deadline !== null && clock.now() >= deadline) {
            await client.stop(start.task_id, "timeout").catch(() => {});
            const collected = await collectOutput(client, start.task_id).catch(() => null);
            const text = collected ? formatFinishedOutput(collected, outputPath).text : "";
            throw new Error(
              appendStatus(text, timedOutStatus(input.timeout!)),
            );
          }
        }

        const collected = await collectOutput(client, start.task_id);
        const { text, details } = formatFinishedOutput(collected, outputPath);
        const exitCode = waitResult.exit_code ?? null;
        if (exitCode !== 0 && exitCode !== null) {
          throw new Error(appendStatus(text, `Command exited with code ${exitCode}`));
        }
        // No exit code: killed (timeout, stop, crash), unless the manager
        // finished it as completed with its runner status unobservable.
        if (exitCode === null && collected.status !== "completed") {
          throw new Error(appendStatus(text, await killedStatus(client, start.task_id, input.timeout)));
        }
        const finalDetails = details
          ? { ...details, task_id: start.task_id }
          : ({ task_id: start.task_id } as FamulusChildBashDetails);
        return { content: [{ type: "text", text }], details: finalDetails };
      } finally {
        // The overrun wake must not name a shell that already returned.
        if (childId) deps.shells?.end(childId, start.task_id);
      }
    },
  };
}
