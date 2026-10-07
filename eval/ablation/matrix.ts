/**
 * Pass/fail matrix of baseline runs: one row per scenario, one column per
 * model spec (provider/id:thinking). A cell is blank when the grader scored
 * at least k episodes and passed every one; anything else is X (a FAIL, or
 * too few scored episodes: INVALID and errors do not count).
 *
 *   node ablation/matrix.ts --results a.jsonl,b.jsonl [--k 10] [--title T --note N --write RESULTS.md]
 *
 * Later files replace earlier ones cell by cell, so a rerun of some cells is
 * passed after the run it corrects. Without --write the section is printed.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { parseArgs } from "node:util";
import { DEFAULT_SCENARIOS, SCENARIOS } from "./scenarios.ts";

interface Line {
  model: string;
  variant: string;
  scenario: string;
  pass: boolean | null;
  error?: string;
}

export interface Cell {
  scored: number;
  passed: number;
}

export function cells(files: string[]): { models: string[]; cells: Map<string, Cell> } {
  const merged = new Map<string, Cell>();
  const models: string[] = [];
  for (const file of files) {
    const own = new Map<string, Cell>();
    for (const raw of readFileSync(file, "utf8").split("\n")) {
      if (!raw.trim()) continue;
      const r = JSON.parse(raw) as Line;
      if (r.variant !== "baseline") continue;
      if (!models.includes(r.model)) models.push(r.model);
      const key = `${r.model}\u0000${r.scenario}`;
      const c = own.get(key) ?? { scored: 0, passed: 0 };
      if (r.pass !== null && !r.error) {
        c.scored++;
        if (r.pass) c.passed++;
      }
      own.set(key, c);
    }
    for (const [key, c] of own) merged.set(key, c);
  }
  return { models, cells: merged };
}

export const passes = (c: Cell | undefined, k: number) => !!c && c.scored >= k && c.passed === c.scored;

/** Column label: drop the provider when no other column has the same id. */
function label(model: string, models: string[]): string {
  const short = model.replace(/^[^/]+\//, "");
  return models.filter((m) => m.replace(/^[^/]+\//, "") === short).length > 1 ? model : short;
}

export function renderMatrix(files: string[], k: number): string {
  const { models, cells: c } = cells(files);
  const header = `| scenario | tests | ${models.map((m) => `\`${label(m, models)}\``).join(" | ")} |`;
  const rule = `|---|---|${models.map(() => "---").join("|")}|`;
  // The default grid, plus any opt-in scenario this run actually scored.
  const ran = new Set([...c.keys()].map((key) => key.split("\u0000")[1]));
  const shown = SCENARIOS.filter((s) => DEFAULT_SCENARIOS.includes(s) || ran.has(s.id));
  const rows = shown.map(
    (s) => `| \`${s.id}\` | ${s.behavior.replace(/\|/g, "\\|")} | ${models.map((m) => (passes(c.get(`${m}\u0000${s.id}`), k) ? "" : "X")).join(" | ")} |`,
  );
  return [header, rule, ...rows].join("\n");
}

if (import.meta.main) {
  const { values } = parseArgs({
    options: {
      results: { type: "string" },
      k: { type: "string", default: "10" },
      title: { type: "string" },
      note: { type: "string", default: "" },
      write: { type: "string" },
    },
  });
  if (!values.results) throw new Error("--results a.jsonl,b.jsonl is required");
  const files = values.results.split(",");
  const k = Number(values.k);
  const table = renderMatrix(files, k);
  const section = [
    `## ${values.title ?? "run"}`,
    "",
    ...(values.note ? [values.note, ""] : []),
    `Blank: ${k} of ${k} scored episodes passed. X: any FAIL, or fewer than ${k} scored (INVALID and errors excluded).`,
    "",
    table,
    "",
    `<sub>results: ${files.map((f) => `\`${f}\``).join(", ")}</sub>`,
    "",
  ].join("\n");
  if (!values.write) {
    process.stdout.write(section);
  } else {
    const doc = readFileSync(values.write, "utf8");
    const marker = "<!-- runs: newest first -->\n";
    if (!doc.includes(marker)) throw new Error(`${values.write} has no "${marker.trim()}" marker`);
    writeFileSync(values.write, doc.replace(marker, `${marker}\n${section}`));
  }
}
