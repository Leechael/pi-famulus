/**
 * Prompt-ablation runner: models × (baseline + leave-one-out variants) ×
 * scenarios × k repeats, with sequential early stopping, a concurrency
 * limit, a cost gate, and resumable JSONL results.
 *
 *   node ablation/run.ts --tier smoke                 # plan + cost estimate only
 *   node ablation/run.ts --tier smoke --yes           # 1 model, baseline, k=3
 *   node ablation/run.ts --tier full --yes            # all models, all variants, k=10
 *   node ablation/run.ts --models xai/grok-4.3 --scenarios bg-end-turn,no-fabrication --k 1 --yes
 *   node ablation/report.ts                           # tables + load-bearing / slop flags
 *
 * Flags: --tier smoke|full  --models a,b  --scenarios ids  --variants ids  --k N
 *        --concurrency N  --pairs affected|all  --results FILE  --max-episodes N
 *        --transcripts  --keep  --judge MODEL  --yes
 *
 * Auth: nothing here touches credentials. Each episode spawns the user's own
 * `pi --mode rpc --model <spec>`, which resolves auth from ~/.pi/agent/auth.json
 * (OAuth refresh included) or env vars exactly as in normal use.
 */
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { parseArgs } from "node:util";
import { availableModels, type ModelInfo, parseModelSpec } from "../lib/models.ts";
import { EVAL_DIR } from "../lib/paths.ts";
import { runEpisode } from "./episode.ts";
import { isAblatable, loadManifest, resolveVariant, type Variant, variantAffects } from "./manifest.ts";
import { DEFAULT_SCENARIOS, getScenario, type Scenario } from "./scenarios.ts";
import { setupBroken, stopDecision, type StopDecision } from "./stats.ts";

export interface ResultRecord {
  v: 1;
  ts: string;
  model: string;
  variant: string;
  scenario: string;
  attempt: number;
  pass: boolean | null;
  reason: string;
  metrics: Record<string, unknown>;
  durationMs: number;
  usage: { input: number; output: number; cacheRead: number; cost: number; calls: number };
  ablationMisses: string[];
  error?: string;
  transcriptPath?: string;
}

export const DEFAULT_RESULTS = join(EVAL_DIR, "results", "results.jsonl");

export function loadResults(path: string): ResultRecord[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l) as ResultRecord);
}

const cellKey = (model: string, variant: string, scenario: string) => `${model}|${variant}|${scenario}`;

interface Cell {
  model: string;
  variant: Variant;
  scenario: Scenario;
  key: string;
}

/** Per-call token assumptions for the estimate (system prompt ≈ 5k tokens, mostly cached after call 1). */
const EST = { freshInput: 2500, cachedInput: 6000, output: 450 };

function estimateCost(info: ModelInfo | undefined, calls: number, extraContextTokens = 0): number {
  if (!info) return Number.NaN;
  const c = info.cost;
  return (calls * (EST.freshInput * c.input + EST.cachedInput * (c.cacheRead || c.input) + EST.output * c.output) + extraContextTokens * c.input) / 1e6;
}

async function main(): Promise<void> {
  const { values } = parseArgs({
    options: {
      tier: { type: "string", default: "smoke" },
      models: { type: "string" },
      scenarios: { type: "string" },
      variants: { type: "string" },
      k: { type: "string" },
      concurrency: { type: "string", default: "3" },
      pairs: { type: "string", default: "affected" },
      results: { type: "string", default: DEFAULT_RESULTS },
      "max-episodes": { type: "string" },
      transcripts: { type: "boolean", default: false },
      keep: { type: "boolean", default: false },
      judge: { type: "string" },
      yes: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
  });
  if (values.help) {
    console.log(readFileSync(new URL(import.meta.url), "utf8").split("*/")[0]);
    return;
  }
  const tier = values.tier;
  if (tier !== "smoke" && tier !== "full") throw new Error("--tier must be smoke or full");

  const configured = (JSON.parse(readFileSync(join(EVAL_DIR, "models.json"), "utf8")) as { models: string[] }).models;
  let models = values.models ? values.models.split(",").map((s) => s.trim()) : configured;
  if (tier === "smoke" && !values.models) models = models.slice(0, 1);
  const scenarios = values.scenarios ? values.scenarios.split(",").map((s) => getScenario(s.trim())) : DEFAULT_SCENARIOS;
  const manifest = loadManifest();
  const variantIds = values.variants
    ? values.variants.split(",").map((s) => s.trim())
    : tier === "smoke"
      ? ["baseline"]
      : ["baseline", ...manifest.segments.filter(isAblatable).map((s) => s.id), ...manifest.groups.map((g) => g.id)];
  if (!variantIds.includes("baseline")) variantIds.unshift("baseline");
  const variants = variantIds.map((id) => resolveVariant(manifest, id));
  const k = Number(values.k ?? (tier === "smoke" ? 3 : 10));
  const concurrency = Math.max(1, Number(values.concurrency));
  const maxEpisodes = values["max-episodes"] ? Number(values["max-episodes"]) : Number.POSITIVE_INFINITY;

  // Validate models against what pi can authenticate (no keys handled here).
  const available = await availableModels();
  const infoFor = (spec: string) => {
    const m = parseModelSpec(spec);
    return available.find((a) => a.provider === m.provider && a.id === m.id);
  };
  const unknown = models.filter((m) => !infoFor(m));
  if (unknown.length) {
    throw new Error(`not authenticated / unknown to pi: ${unknown.join(", ")} (see \`pi -ne --list-models\`)`);
  }

  const cells: Cell[] = [];
  for (const model of models) {
    for (const scenario of scenarios) {
      for (const variant of variants) {
        if (values.pairs === "affected" && !variantAffects(variant, scenario.id)) continue;
        cells.push({ model, variant, scenario, key: cellKey(model, variant.id, scenario.id) });
      }
    }
  }

  const results = loadResults(values.results);
  const scored = (key: string) => results.filter((r) => cellKey(r.model, r.variant, r.scenario) === key && r.pass !== null && !r.error);
  const attempts = (key: string) => results.filter((r) => cellKey(r.model, r.variant, r.scenario) === key).length;
  // Attempts already in the results file when this run started. The give-up
  // rule counts only this run's attempts, so a resume (after a provider outage
  // or a grader fix) tries a cell again instead of skipping it silently.
  const priorAttempts = new Map(cells.map((c) => [c.key, attempts(c.key)]));

  // ---- plan + cost estimate --------------------------------------------------
  const remaining = cells.map((c) => Math.max(0, k - scored(c.key).length));
  const plannedEpisodes = remaining.reduce((a, b) => a + b, 0);
  console.log(`tier=${tier} models=${models.join(",")} scenarios=${scenarios.length} variants=${variants.length} k=${k} pairs=${values.pairs}`);
  console.log(`cells=${cells.length} already-scored=${cells.reduce((a, c) => a + scored(c.key).length, 0)} max-new-episodes=${Math.min(plannedEpisodes, maxEpisodes)}`);
  for (const s of scenarios.filter((s) => s.optIn)) {
    console.log(`  OPT-IN ${s.id}: extra context estimate ~${s.estContextTokens ?? 0} tokens/call (synthetic, not incident replay; provider usage authoritative)`);
  }
  let totalCost = 0;
  for (const model of models) {
    const eps = cells.map((c, i) => (c.model === model ? remaining[i] : 0));
    const calls = cells.reduce((acc, c, i) => acc + (c.model === model ? eps[i] * c.scenario.estCalls : 0), 0);
    const extraContextTokens = cells.reduce((acc, c, i) => acc + (c.model === model ? eps[i] * c.scenario.estCalls * (c.scenario.estContextTokens ?? 0) : 0), 0);
    const cost = estimateCost(infoFor(model), calls, extraContextTokens);
    totalCost += cost;
    console.log(`  ${model}: ≤${eps.reduce((a, b) => a + b, 0)} episodes, ~${calls} model calls, ~$${cost.toFixed(2)} at list price`);
  }
  console.log(`estimated upper bound: ~$${totalCost.toFixed(2)} (early stopping usually cuts variants short; OAuth/subscription providers may bill $0)`);
  if (!values.yes) {
    console.log("dry run: re-run with --yes to execute");
    return;
  }

  // ---- execute with early stopping --------------------------------------------
  mkdirSync(dirname(values.results), { recursive: true });
  const inflight = new Map<string, number>();
  const decisions = new Map<string, StopDecision>();
  let launched = 0;

  // In-flight attempts count too: otherwise a broken cell launches up to
  // `concurrency` more episodes before the first one comes back.
  const triedThisRun = (key: string) => attempts(key) - (priorAttempts.get(key) ?? 0) + (inflight.get(key) ?? 0);

  const decide = (c: Cell): StopDecision => {
    const s = scored(c.key);
    const passes = s.filter((r) => r.pass === true).length;
    const b = scored(cellKey(c.model, "baseline", c.scenario.id));
    const d = stopDecision(
      { passes, n: s.length + (inflight.get(c.key) ?? 0) },
      { passes: b.filter((r) => r.pass).length, n: b.length },
      c.variant.id === "baseline",
      k,
    );
    // Invalid episodes / errors: cap total attempts so a broken setup cannot loop forever.
    if (d === "continue" && setupBroken({ attempts: triedThisRun(c.key), scored: s.length })) return "done";
    if (d === "continue" && attempts(c.key) + (inflight.get(c.key) ?? 0) >= 2 * k) return "done";
    return d;
  };

  const next = (): Cell | undefined => {
    const open = cells.filter((c) => decide(c) === "continue");
    // Baselines first; variants need baseline samples to be judged against.
    return open.find((c) => c.variant.id === "baseline") ?? open.find((c) => {
      const b = scored(cellKey(c.model, "baseline", c.scenario.id)).length;
      return b >= Math.min(3, k);
    });
  };

  await new Promise<void>((resolveAll) => {
    const pump = () => {
      while ([...inflight.values()].reduce((a, b) => a + b, 0) < concurrency && launched < maxEpisodes) {
        const cell = next();
        if (!cell) break;
        launched++;
        inflight.set(cell.key, (inflight.get(cell.key) ?? 0) + 1);
        const attempt = attempts(cell.key) + (inflight.get(cell.key) ?? 0) - 1;
        const label = `${cell.model.replace(/[/:]/g, "_")}-${cell.variant.id}-${cell.scenario.id}-${attempt}`;
        console.log(`▶ ${cell.key} #${attempt}`);
        runEpisode({
          model: cell.model,
          variant: cell.variant,
          scenario: cell.scenario,
          keep: values.keep,
          label,
          ...(values.transcripts ? { transcriptDir: join(dirname(values.results), "transcripts") } : {}),
          ...(values.judge ? { judgeModel: values.judge } : {}),
        })
          .then((r) => {
            const rec: ResultRecord = {
              v: 1,
              ts: new Date().toISOString(),
              model: cell.model,
              variant: cell.variant.id,
              scenario: cell.scenario.id,
              attempt,
              pass: r.grade.pass,
              reason: r.grade.reason,
              metrics: r.grade.metrics,
              durationMs: r.durationMs,
              usage: r.usage,
              ablationMisses: r.ablationMisses,
              ...(r.error ? { error: r.error } : {}),
              ...(r.transcriptPath ? { transcriptPath: r.transcriptPath } : {}),
            };
            results.push(rec);
            appendFileSync(values.results, `${JSON.stringify(rec)}\n`);
            const mark = r.error ? "ERR" : r.grade.pass === null ? "INVALID" : r.grade.pass ? "PASS" : "FAIL";
            console.log(`  ${mark} ${cell.key} #${attempt} (${Math.round(r.durationMs / 1000)}s): ${r.error ?? r.grade.reason} ${JSON.stringify(r.grade.metrics)}${r.ablationMisses.length ? ` [ablation-miss: ${r.ablationMisses}]` : ""}`);
          })
          .catch((err) => console.error(`  CRASH ${cell.key}: ${(err as Error).stack}`))
          .finally(() => {
            inflight.set(cell.key, (inflight.get(cell.key) ?? 1) - 1);
            const d = decide(cell);
            if (d !== "continue" && decisions.get(cell.key) !== d) {
              decisions.set(cell.key, d);
              if (d !== "done") console.log(`  ⏹ ${cell.key}: early stop (${d})`);
              else if (setupBroken({ attempts: triedThisRun(cell.key), scored: scored(cell.key).length })) {
                console.log(`  ⏹ ${cell.key}: gave up, no scored episode in ${triedThisRun(cell.key)} attempts`);
              }
            }
            pump();
            if ([...inflight.values()].every((n) => n === 0) && (!next() || launched >= maxEpisodes)) resolveAll();
          });
      }
      if ([...inflight.values()].every((n) => n === 0)) resolveAll();
    };
    pump();
  });
  console.log(`done: ${launched} episodes run. Results: ${values.results}\nReport: node ablation/report.ts --results ${values.results}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((err) => {
    console.error(err instanceof Error ? err.message : err);
    process.exit(1);
  });
}
