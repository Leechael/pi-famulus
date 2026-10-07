/**
 * bash tool override (design doc §4.2).
 *
 * Routes bash commands through pi-famulus:
 * - foreground commands wait up to `foregroundBudgetMs` (default 20000,
 *   configurable) and are then moved to the background instead of blocking;
 * - bare sleep / idle-loop commands are rejected with guidance;
 * - when the manager is unavailable, execution falls back to a local
 *   child_process implementation that mimics the built-in bash tool
 *   (degraded but never broken).
 *
 * Result details stay compatible with BashToolDetails
 * ({ truncation?, fullOutputPath? }); extension fields are added alongside.
 */
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Type, type Static } from "typebox";
import type {
  AgentToolResult,
  BashToolDetails,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { taskOutputPath, type FamulusConfig } from "./config";
import { realClock, type Clock, type ClockTimer } from "./clock";
import { backgroundRowText, formatBackgroundNotice, truncateTail } from "./format";
import { toolComponent } from "./tui/tool-component";
import type { WorkIndex } from "./work-index";
import type { ManagerClient } from "./manager-client";
import { fill, PROMPTS } from "./prompts.generated";
import {
  appendStatus,
  bareSleepError,
  collectOutput,
  formatFinishedOutput,
  killedBy,
  killedStatus,
  timedOutStatus,
  SHELL_MAX_BYTES,
  SHELL_MAX_LINES,
  withAbort,
} from "./shell-exec";

/** Same limits as the built-in bash tool. */
const MAX_LINES = SHELL_MAX_LINES;
const MAX_BYTES = SHELL_MAX_BYTES;

const bashParameters = Type.Object({
  command: Type.String({ description: PROMPTS["tools.bash.param.command"] }),
  timeout: Type.Optional(
    Type.Number({ description: PROMPTS["tools.bash.param.timeout"] }),
  ),
  run_in_background: Type.Optional(
    Type.Boolean({ description: PROMPTS["tools.bash.param.run_in_background"] }),
  ),
});

type BashParams = { command: string; timeout?: number; run_in_background?: boolean };

/**
 * Mirrors the built-in bash outputSchema (pi ≥0.99) so codemode scripts calling
 * `bash()` resolve to the same structured shape the built-in tool returns.
 * pi does not export `bashOutputSchema`, so keep this in sync with BashToolOutput.
 */
const bashOutputSchema = Type.Object({
  output: Type.String(),
  truncated: Type.Boolean(),
  full_output_path: Type.Optional(Type.String()),
  exit_code: Type.Number(),
  wall_time_seconds: Type.Number(),
});
type BashToolOutput = Static<typeof bashOutputSchema>;

/** Same cap as the built-in structured output (1 MiB). */
const STRUCTURED_OUTPUT_MAX_BYTES = 1024 * 1024;

/** Full-output slice for structuredContent, keeping the tail like the built-in. */
function structuredOutput(fullText: string): { output: string; truncated: boolean } {
  if (Buffer.byteLength(fullText, "utf8") <= STRUCTURED_OUTPUT_MAX_BYTES) {
    return { output: fullText, truncated: false };
  }
  // Keep the largest tail that fits the cap without materializing the whole
  // output as a second buffer: search a UTF-16 index near the end (each code
  // unit encodes to at least one byte, so the tail starts within `cap` units
  // of the end) and only encode that slice.
  let lo = Math.max(0, fullText.length - STRUCTURED_OUTPUT_MAX_BYTES);
  let hi = fullText.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (Buffer.byteLength(fullText.slice(mid), "utf8") <= STRUCTURED_OUTPUT_MAX_BYTES) hi = mid;
    else lo = mid + 1;
  }
  return { output: fullText.slice(lo), truncated: true };
}

function wallSeconds(startedAtMs: number, endedAtMs: number): number {
  return Math.round((endedAtMs - startedAtMs) / 100) / 10;
}

/** Details shape returned by this override; superset of BashToolDetails. */
export interface FamulusBashDetails extends BashToolDetails {
  backgrounded?: boolean;
  task_id?: string;
}

export interface BashOverrideDeps {
  getClient: () => ManagerClient | null;
  config: FamulusConfig;
  home: string;
  sessionId: () => string;
  /** Extra environment injected into managed child processes (PI_* vars). */
  sessionEnv: (ctx: ExtensionContext) => Record<string, string>;
  /** Register task metadata so exit notifications can describe the task. */
  trackTask: (taskId: string, meta: { kind: string; command: string; cwd?: string }) => void;
  /**
   * Mark a task so its task_exited event becomes a parent <pi-famulus-wake kind="task">.
   * Only backgrounded parent bash should call this — sync waits (foreground
   * budget hit, child-bash) must not wake the parent session.
   */
  markNotifyOnExit: (taskId: string) => void;
  clock?: Clock;
  /** Live background work, so a backgrounded row can show its final status. */
  getIndex?: () => WorkIndex | null;
}

/** Same collapsed preview as pi's default tool-result view. */
const PREVIEW_LINES = 10;

function isActiveStatus(status: string | undefined): boolean {
  return status === undefined || status === "running" || status === "pending";
}

function rejectBareSleep(command: string): string | null {
  return bareSleepError(command, "tools.bash.error.bare-sleep");
}

function resolveTimeoutMs(timeoutSeconds: number | undefined): number | null {
  if (timeoutSeconds === undefined) return null;
  if (!Number.isFinite(timeoutSeconds) || timeoutSeconds <= 0) {
    throw new Error("Invalid timeout: must be a finite number of seconds");
  }
  return Math.round(timeoutSeconds * 1000);
}

function fullEnv(ctx: ExtensionContext, deps: BashOverrideDeps): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined) env[key] = value;
  }
  Object.assign(env, deps.sessionEnv(ctx));
  return env;
}

// ---------------------------------------------------------------------------
// Local fallback (manager unavailable): mimics the built-in bash tool.
// ---------------------------------------------------------------------------

async function executeLocal(
  params: BashParams,
  signal: AbortSignal | undefined,
  ctx: ExtensionContext,
  clock: Clock,
): Promise<AgentToolResult<FamulusBashDetails | undefined>> {
  const timeoutMs = resolveTimeoutMs(params.timeout);
  const startedAtMs = clock.now();
  const shell = process.env.SHELL && process.env.SHELL.length > 0 ? process.env.SHELL : "/bin/bash";

  const output = await new Promise<{ text: string; exitCode: number | null; signal: string | null; timedOut: boolean; aborted: boolean }>(
    (resolve, reject) => {
      const child = spawn(shell, ["-c", params.command], {
        cwd: ctx.cwd,
        env: process.env,
        stdio: ["ignore", "pipe", "pipe"],
      });
      const chunks: Buffer[] = [];
      let settled = false;
      let timedOut = false;
      let aborted = false;
      let timer: ClockTimer | null = null;
      const finish = (exitCode: number | null, killSignal: string | null) => {
        if (settled) return;
        settled = true;
        if (timer !== null) clock.clearTimeout(timer);
        signal?.removeEventListener("abort", onAbort);
        resolve({ text: Buffer.concat(chunks).toString("utf8"), exitCode, signal: killSignal, timedOut, aborted });
      };
      const kill = () => {
        try {
          child.kill("SIGTERM");
        } catch {
          // already dead
        }
      };
      const onAbort = () => {
        aborted = true;
        kill();
      };
      timer =
        timeoutMs !== null
          ? clock.setTimeout(() => {
              timedOut = true;
              kill();
            }, timeoutMs)
          : null;
      if (timer !== null) clock.unref?.(timer);
      signal?.addEventListener("abort", onAbort, { once: true });
      child.stdout?.on("data", (d: Buffer) => chunks.push(d));
      child.stderr?.on("data", (d: Buffer) => chunks.push(d));
      child.on("error", (err) => {
        if (timer !== null) clock.clearTimeout(timer);
        signal?.removeEventListener("abort", onAbort);
        if (!settled) {
          settled = true;
          reject(new Error(`Failed to start shell: ${err.message}`));
        }
      });
      child.on("close", (code, killSignal) => finish(code, killSignal));
    },
  );

  const t = truncateTail(output.text, MAX_LINES, MAX_BYTES);
  let text = t.text || "(no output)";
  let details: FamulusBashDetails | undefined;
  if (t.truncated) {
    const fullOutputPath = join(tmpdir(), `pi-famulus-bash-${randomUUID()}.log`);
    writeFileSync(fullOutputPath, output.text, "utf8");
    const outputLines = t.text.length === 0 ? 0 : t.text.split("\n").length;
    const startLine = t.totalLines - outputLines + 1;
    details = {
      truncation: {
        content: t.text,
        truncated: true,
        truncatedBy: t.totalLines > MAX_LINES ? "lines" : "bytes",
        totalLines: t.totalLines,
        totalBytes: t.totalBytes,
        outputLines,
        outputBytes: Buffer.byteLength(t.text, "utf8"),
        lastLinePartial: false,
        firstLineExceedsLimit: false,
        maxLines: MAX_LINES,
        maxBytes: MAX_BYTES,
      },
      fullOutputPath,
    };
    text += `\n\n[Showing lines ${startLine}-${t.totalLines} of ${t.totalLines}. Full output: ${fullOutputPath}]`;
  }

  if (output.aborted) throw new Error(appendStatus(text, "Command aborted"));
  if (output.timedOut && params.timeout !== undefined) {
    throw new Error(appendStatus(text, timedOutStatus(params.timeout)));
  }
  if (output.exitCode === null) throw new Error(appendStatus(text, killedBy(output.signal)));
  const { output: structuredText, truncated: structuredTruncated } = structuredOutput(output.text);
  const structuredContent: BashToolOutput = {
    output: structuredText,
    truncated: structuredTruncated,
    ...(details?.fullOutputPath ? { full_output_path: details.fullOutputPath } : {}),
    exit_code: output.exitCode,
    wall_time_seconds: wallSeconds(startedAtMs, clock.now()),
  };
  if (output.exitCode !== 0) {
    // Mirror the built-in bash tool: non-zero exits resolve to an isError
    // result carrying structuredContent instead of rejecting.
    return {
      content: [{ type: "text", text: appendStatus(text, `Command exited with code ${output.exitCode}`) }],
      details,
      structuredContent,
      isError: true,
    };
  }
  return { content: [{ type: "text", text }], details, structuredContent };
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

export function createBashOverride(
  deps: BashOverrideDeps,
): ToolDefinition<typeof bashParameters, FamulusBashDetails | undefined> {
  return {
    name: "bash",
    label: "Bash",
    description: fill("tools.bash.description", { maxLines: MAX_LINES, maxKb: MAX_BYTES / 1024 }),
    promptSnippet: PROMPTS["tools.bash.snippet"],
    promptGuidelines: [...PROMPTS["tools.bash.rules"]],
    parameters: bashParameters,
    outputSchema: bashOutputSchema,
    renderResult(result, { expanded }, theme, context) {
      const details = result.details as FamulusBashDetails | undefined;
      if (details?.backgrounded && details.task_id) {
        const taskId = details.task_id;
        const index = deps.getIndex?.() ?? null;
        const state = context.state as { unsubscribe?: () => void };
        const item = index?.get(taskId);
        if (index && !state.unsubscribe && isActiveStatus(item?.status)) {
          // Redraw this row when the task finishes, then stop listening.
          state.unsubscribe = index.onChange(() => {
            if (!isActiveStatus(index.get(taskId)?.status)) {
              state.unsubscribe?.();
              state.unsubscribe = () => {};
            }
            context.invalidate();
          });
        }
        const row = backgroundRowText(taskId, item, (deps.clock ?? realClock).now());
        return toolComponent([`${theme.fg(row.color as never, row.glyph)} ${theme.fg("muted", row.text)}`]) as never;
      }
      const text = result.content
        .filter((c): c is { type: "text"; text: string } => c.type === "text")
        .map((c) => c.text)
        .join("\n");
      const lines = text.split("\n");
      const shown = expanded ? lines : lines.slice(0, PREVIEW_LINES);
      const out = shown.map((l) => theme.fg("toolOutput", l));
      if (shown.length < lines.length) {
        out.push(theme.fg("muted", `... (${lines.length - shown.length} more lines, ctrl+o to expand)`));
      }
      return toolComponent(out) as never;
    },
    async execute(_toolCallId, params, signal, onUpdate, ctx) {
      const input = params as BashParams;

      const sleepError = rejectBareSleep(input.command);
      if (sleepError) throw new Error(sleepError);

      const client = deps.getClient();
      const managed = client !== null && (await client.ensureAvailable());
      if (!managed || client === null) {
        // Degraded mode: run locally like the built-in bash tool.
        return executeLocal(input, signal, ctx, deps.clock ?? realClock);
      }

      const timeoutMs = resolveTimeoutMs(input.timeout);
      const startedAtMs = (deps.clock ?? realClock).now();
      let start;
      try {
        start = await client.start({
          kind: "shell",
          command: input.command,
          cwd: ctx.cwd,
          env: fullEnv(ctx, deps),
          run_in_background: input.run_in_background === true,
          timeout_ms: timeoutMs,
          origin: { via: input.run_in_background === true ? "bash-bg" : "bash-fg" },
        });
      } catch {
        // Manager request failed mid-session; degrade to local execution.
        return executeLocal(input, signal, ctx, deps.clock ?? realClock);
      }
      deps.trackTask(start.task_id, { kind: "shell", command: input.command, cwd: ctx.cwd });
      const outputPath = taskOutputPath(deps.home, deps.sessionId(), start.task_id);

      if (input.run_in_background === true) {
        deps.markNotifyOnExit(start.task_id);
        return {
          content: [
            { type: "text", text: formatBackgroundNotice(start.task_id, input.command, outputPath) },
          ],
          details: { fullOutputPath: outputPath, backgrounded: true, task_id: start.task_id },
        };
      }

      // Foreground: wait up to the budget, then move to background.
      let waitResult;
      try {
        waitResult = await withAbort(client.wait(start.task_id, deps.config.foregroundBudgetMs), signal, () => {
          client.stop(start.task_id, "tool").catch(() => {});
        });
      } catch (err) {
        if ((err as Error).message === "aborted") {
          throw new Error("Command aborted (background task stopped)");
        }
        // Lost contact with the manager while waiting; the task may still run.
        deps.markNotifyOnExit(start.task_id);
        return {
          content: [
            {
              type: "text",
              text:
                `Lost contact with pi-famulus while waiting for task ${start.task_id}. ` +
                `The command may still be running. Output: ${outputPath}. ` +
                "Use task_list/task_output to check on it once the manager is back.",
            },
          ],
          details: { fullOutputPath: outputPath, backgrounded: true, task_id: start.task_id },
        };
      }

      if (!waitResult.done) {
        deps.markNotifyOnExit(start.task_id);
        return {
          content: [
            { type: "text", text: formatBackgroundNotice(start.task_id, input.command, outputPath) },
          ],
          details: { fullOutputPath: outputPath, backgrounded: true, task_id: start.task_id },
        };
      }

      const collected = await collectOutput(client, start.task_id);
      const { text, details } = formatFinishedOutput(collected, outputPath);
      const exitCode = waitResult.exit_code ?? null;
      // No exit code: killed (timeout, stop, crash), unless the manager
      // finished it as completed with its runner status unobservable.
      if (exitCode === null && collected.status !== "completed") {
        throw new Error(appendStatus(text, await killedStatus(client, start.task_id, input.timeout)));
      }
      if (exitCode === null) {
        // Completed but the exit code was unobservable: no honest
        // structured shape (exit_code is required, and a sentinel like -1
        // would misclassify success as failure), so omit structuredContent
        // and let codemode scripts fall back to the text content.
        return { content: [{ type: "text", text }], details };
      }
      const { output: structuredText, truncated: structuredTruncated } = structuredOutput(collected.text);
      const structuredContent: BashToolOutput = {
        output: structuredText,
        truncated: structuredTruncated || collected.windowed,
        full_output_path: outputPath,
        exit_code: exitCode,
        wall_time_seconds: wallSeconds(startedAtMs, (deps.clock ?? realClock).now()),
      };
      if (exitCode !== 0) {
        // Mirror the built-in bash tool: non-zero exits resolve to an isError
        // result carrying structuredContent instead of rejecting.
        return {
          content: [{ type: "text", text: appendStatus(text, `Command exited with code ${exitCode}`) }],
          details,
          structuredContent,
          isError: true,
        };
      }
      return { content: [{ type: "text", text }], details, structuredContent };
    },
  };
}
