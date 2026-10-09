/** One real-model episode: sandbox → pi (our ext + ablation harness) → grade. */
import { mkdirSync, readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { ABLATION_EXT } from "../lib/paths.ts";
import { PiRpc, type RpcEvent } from "../lib/rpc.ts";
import { createSandbox, hasRunningWork, waitManagerReady } from "../lib/sandbox.ts";
import { assistants, itemsFromEvents } from "../lib/transcript.ts";
import type { Grade } from "./graders.ts";
import { errorAfterCompatibilityGrade, errorBeforeShutdown } from "./episode-error-policy.ts";
import { judge } from "./judge.ts";
import { type Variant, variantFamulusConfig } from "./manifest.ts";
import type { Scenario } from "./scenarios.ts";

/** Settled + silent this long with no running work → the agent is done. */
const IDLE_GIVE_UP_MS = 6000;
const NO_WORK_GRACE_MS = 3000;

export interface EpisodeResult {
  grade: Grade;
  durationMs: number;
  usage: { input: number; output: number; cacheRead: number; cost: number; calls: number };
  /** Requested text segments that never matched anything (drift / unreachable). */
  ablationMisses: string[];
  /** Hard failures that are not the model's fault (provider errors, timeouts to start). */
  error?: string;
  transcriptPath?: string;
}

function deepMerge(a: Record<string, unknown>, b: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...a };
  for (const [k, v] of Object.entries(b)) {
    out[k] = v && typeof v === "object" && !Array.isArray(v) && typeof out[k] === "object" ? deepMerge(out[k] as Record<string, unknown>, v as Record<string, unknown>) : v;
  }
  return out;
}

function ablationMisses(logPath: string, variant: Variant): string[] {
  const requested = variant.segments.filter((s) => s.kind === "text" || s.kind === "regex").map((s) => s.id);
  if (requested.length === 0) return [];
  const hits = new Map<string, number>();
  if (existsSync(logPath)) {
    for (const line of readFileSync(logPath, "utf8").split("\n").filter(Boolean)) {
      const rec = JSON.parse(line) as { hook: string; hits: Record<string, number> };
      for (const [id, n] of Object.entries(rec.hits)) hits.set(id, (hits.get(id) ?? 0) + n);
    }
  }
  return requested.filter((id) => !((hits.get(id) ?? 0) > 0));
}

export async function runEpisode(opts: {
  model: string;
  variant: Variant;
  scenario: Scenario;
  keep?: boolean;
  transcriptDir?: string;
  label?: string;
  /** Optional LLM judge model for scenario.judgeQuestion. */
  judgeModel?: string;
  /** Extra extensions/env, e.g. the faux harness to test graders offline. */
  extensions?: string[];
  env?: Record<string, string>;
}): Promise<EpisodeResult> {
  const { scenario, variant } = opts;
  const sb = createSandbox({ famulusConfig: deepMerge(scenario.famulusConfig, variantFamulusConfig(variant)), keep: opts.keep });
  const secretDir = join(sb.root, "secret");
  mkdirSync(secretDir, { recursive: true });
  const logPath = join(sb.root, "ablation.jsonl");
  const started = Date.now();
  const setup = scenario.setup(sb.cwd, secretDir);
  const pi = new PiRpc({
    cwd: sb.cwd,
    env: { ...sb.env, PI_FAMULUS_ABLATE: variant.id, PI_FAMULUS_ABLATION_LOG: logPath, ...(setup.env ?? {}), ...(opts.env ?? {}) },
    model: opts.model,
    extensions: [...(scenario.extensions ?? []), ...(opts.extensions ?? []), ABLATION_EXT],
  });
  const bg = [] as Array<{ kill(sig?: NodeJS.Signals): boolean }>;
  let error: string | undefined;
  let endedAt: number | undefined;
  let beforeShutdown: RpcEvent[] = [];
  try {
    await pi.ready(60_000);
    await waitManagerReady(sb, 15_000);
    const helper = setup.background?.();
    if (helper) bg.push(helper);
    await pi.prompt(setup.prompt);
    const deadline = started + scenario.timeoutMs;
    const view = () => ({ items: itemsFromEvents(pi.events), cwd: sb.cwd, secretDir });
    // Until the goal is met, or the agent gave up: settled, quiet, and nothing
    // left running that could still wake it.
    // "No running work" must hold for a grace period: a task that just exited
    // has not delivered its wake yet (200ms batching + preview read).
    let idleSince: number | null = null;
    for (;;) {
      if (Date.now() >= deadline || scenario.done(view().items, view())) break;
      const quietFor = pi.now() - (pi.events.at(-1)?.t ?? 0);
      if (pi.isSettled() && quietFor >= IDLE_GIVE_UP_MS && !hasRunningWork(sb)) {
        idleSince ??= pi.now();
        if (pi.now() - idleSince >= NO_WORK_GRACE_MS && pi.now() - (pi.events.at(-1)?.t ?? 0) >= NO_WORK_GRACE_MS) break;
      } else {
        idleSince = null;
      }
      await new Promise((r) => setTimeout(r, 500));
    }
    await pi.waitQuiet(scenario.quietMs, Math.max(1000, deadline - Date.now()));
  } catch (err) {
    error = (err as Error).message;
  } finally {
    endedAt = pi.now();
    // Shutdown can finish an in-flight response with an abort. Capture the
    // observed prefix by event boundary (timestamps can share a millisecond).
    beforeShutdown = pi.events.slice();
    await pi.stop();
    for (const p of bg) p.kill("SIGKILL");
  }
  const items = itemsFromEvents(beforeShutdown);
  const grade = scenario.grade({ items, cwd: sb.cwd, secretDir, endedAt });
  Object.assign(grade.metrics, setup.metadata ?? {});
  // ready() already records get_state: audit actual resolution without another
  // request (notably thinking levels which pi may silently clamp).
  const state = beforeShutdown.find((e) => e.type === "response" && e.command === "get_state" && e.success === true)?.data as
    { model?: { provider?: string; id?: string; api?: string }; thinkingLevel?: string } | undefined;
  Object.assign(grade.metrics, {
    requestedModelSpec: opts.model,
    modelProvider: state?.model?.provider ?? "unknown",
    modelId: state?.model?.id ?? "unknown",
    modelApi: state?.model?.api ?? "unknown",
    thinkingLevel: state?.thinkingLevel ?? "unknown",
  });
  error = errorAfterCompatibilityGrade(scenario, grade, errorBeforeShutdown(beforeShutdown, grade, error));
  if (opts.judgeModel && scenario.judgeQuestion) {
    const excerpt = items
      .map((i) =>
        i.kind === "assistant"
          ? `ASSISTANT: ${i.text}${i.toolCalls.map((c) => `\n[tool ${c.name} ${JSON.stringify(c.args).slice(0, 300)}]`).join("")}`
          : i.kind === "toolResult"
            ? `TOOL RESULT (${i.toolName}): ${i.text.slice(0, 400)}`
            : i.kind === "wake"
              ? `NOTIFICATION: ${i.wake.raw.slice(0, 600)}`
              : i.kind === "user"
                ? `USER: ${i.text}`
                : "",
      )
      .join("\n");
    const verdict = await judge(opts.judgeModel, scenario.judgeQuestion, excerpt);
    grade.metrics.judge = verdict === null ? "unparsed" : verdict;
  }
  // Keep shutdown usage in the cost audit, but never use its messages to grade.
  const usage = assistants(itemsFromEvents(pi.events)).reduce(
    (acc, a) => ({
      input: acc.input + (a.usage?.input ?? 0),
      output: acc.output + (a.usage?.output ?? 0),
      cacheRead: acc.cacheRead + ((a.usage as { cacheRead?: number })?.cacheRead ?? 0),
      cost: acc.cost + (a.usage?.cost?.total ?? 0),
      calls: acc.calls + 1,
    }),
    { input: 0, output: 0, cacheRead: 0, cost: 0, calls: 0 },
  );
  let transcriptPath: string | undefined;
  if (opts.transcriptDir) {
    mkdirSync(opts.transcriptDir, { recursive: true });
    transcriptPath = join(opts.transcriptDir, `${opts.label ?? `${scenario.id}-${Date.now()}`}.jsonl`);
    const { writeFileSync } = await import("node:fs");
    writeFileSync(transcriptPath, `${pi.events.map((e) => JSON.stringify(e)).join("\n")}\n`);
  }
  const misses = ablationMisses(logPath, variant);
  sb.cleanup();
  return { grade, durationMs: Date.now() - started, usage, ablationMisses: misses, ...(error ? { error } : {}), ...(transcriptPath ? { transcriptPath } : {}) };
}
