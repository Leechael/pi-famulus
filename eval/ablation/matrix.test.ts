/**
 *   node --test ablation/matrix.test.ts
 */
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";
import { cells, passes, renderMatrix } from "./matrix.ts";

const dir = mkdtempSync(join(tmpdir(), "matrix-"));
const file = (name: string, lines: object[]) => {
  const p = join(dir, name);
  writeFileSync(p, lines.map((l) => JSON.stringify({ variant: "baseline", ...l })).join("\n") + "\n");
  return p;
};
const ep = (model: string, scenario: string, pass: boolean | null, extra: object = {}) => ({ model, scenario, pass, ...extra });

describe("matrix", () => {
  it("blank only when k episodes were scored and all passed", () => {
    const a = file("a.jsonl", [
      ...Array.from({ length: 10 }, () => ep("p/m:high", "bg-end-turn", true)),
      ...Array.from({ length: 9 }, () => ep("p/m:high", "wake-continue", true)),
      ep("p/m:high", "wake-continue", null),
      ...Array.from({ length: 9 }, () => ep("p/m:high", "monitor-not-sleep", true)),
      ep("p/m:high", "monitor-not-sleep", false),
      ...Array.from({ length: 9 }, () => ep("p/m:high", "no-fabrication", true)),
      ep("p/m:high", "no-fabrication", true, { error: "provider" }),
    ]);
    const { cells: c } = cells([a]);
    assert.equal(passes(c.get("p/m:high\u0000bg-end-turn"), 10), true);
    assert.equal(passes(c.get("p/m:high\u0000wake-continue"), 10), false, "9 scored + 1 INVALID is not 10 of 10");
    assert.equal(passes(c.get("p/m:high\u0000monitor-not-sleep"), 10), false, "a FAIL");
    assert.equal(passes(c.get("p/m:high\u0000no-fabrication"), 10), false, "an errored episode is not scored");
    assert.equal(passes(c.get("p/m:high\u0000resume-finished"), 10), false, "never run");
  });

  it("a later file replaces an earlier one cell by cell", () => {
    const first = file("first.jsonl", [ep("p/m:high", "overrun-stuck", false), ep("p/m:high", "bg-end-turn", false)]);
    const rerun = file("rerun.jsonl", Array.from({ length: 10 }, () => ep("p/m:high", "overrun-stuck", true)));
    const { cells: c } = cells([first, rerun]);
    assert.equal(passes(c.get("p/m:high\u0000overrun-stuck"), 10), true);
    assert.equal(passes(c.get("p/m:high\u0000bg-end-turn"), 10), false, "cells the rerun did not touch keep the first file's");
  });

  it("lists opt-in scenarios only when the run scored them", () => {
    const plain = renderMatrix([file("plain.jsonl", [ep("p/m:high", "bg-end-turn", true)])], 10);
    assert.ok(!plain.includes("| `monitor-waiter-event`"), "an opt-in probe nobody ran is not a row of X");
    const probed = renderMatrix([file("probed.jsonl", [ep("p/m:high", "monitor-waiter-event", true)])], 10);
    assert.ok(probed.includes("| `monitor-waiter-event`"));
  });

  it("escapes | in the tests column so the GFM row keeps its columns", () => {
    const table = renderMatrix([file("one.jsonl", [ep("p/m:high", "bg-end-turn", true)])], 10);
    const row = table.split("\n").find((l) => l.startsWith("| `monitor-not-sleep`"))!;
    assert.match(row, /tail -f \\\| grep/);
    assert.equal(row.split(/(?<!\\)\|/).length - 2, 3, "scenario, tests, one model column");
  });
});
