/** The idle-after-done grader goes green, red and INVALID on scripted transcripts (no model calls). */
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";
import type { Item } from "../lib/transcript.ts";
import { idleAfterDoneScenarios, idleStats } from "./idle-after-done-scenarios.ts";

const peer = idleAfterDoneScenarios.find((s) => s.id === "idle-after-done-peer")!;
const user = idleAfterDoneScenarios.find((s) => s.id === "idle-after-done-user")!;

let seq = 0;
const a = (text: string, ...calls: [string, Record<string, unknown>][]): Item => ({
  kind: "assistant", seq: seq++, t: seq, text, toolCalls: calls.map(([name, args], i) => ({ id: `c${seq}-${i}`, name, args })),
});
const r = (toolName: string, text: string, isError = false): Item => ({
  kind: "toolResult", seq: seq++, t: seq, toolCallId: "x", toolName, text, details: undefined, isError,
});
const wake = (): Item => ({ kind: "wake", seq: seq++, t: seq, wake: { kind: "task" } as never });

function dirs(token: string | null, reported: boolean) {
  const secretDir = mkdtempSync(join(tmpdir(), "idle-grader-"));
  if (token) writeFileSync(join(secretDir, "token"), `${token}\n`);
  if (reported) writeFileSync(join(secretDir, "peer.jsonl"), `${JSON.stringify({ ts: 1, to: "ci-session", message: `CHECKS PASSED ${token}` })}\n`);
  return { cwd: secretDir, secretDir };
}

const prefix = () => [a("", ["bash", { command: "./ci-check.sh" }]), r("bash", "moved to background"), a("Waiting."), wake()];

describe("idle-after-done grader", () => {
  it("PASS: reports and stops", () => {
    const items = [...prefix(), a("", ["SendMessage", { to: "ci-session", message: "CHECKS PASSED abc" }]), r("SendMessage", "Queued"), a("Reported.")];
    const g = peer.grade({ items, ...dirs("abc", true) });
    assert.equal(g.pass, true, g.reason);
  });

  it("FAIL: the incident loop after the report", () => {
    const items = [...prefix(), a("", ["SendMessage", { to: "ci-session", message: "CHECKS PASSED abc" }]), r("SendMessage", "Queued")];
    for (let i = 0; i < 6; i++) items.push(a("完成。", i % 2 ? ["task_list", {}] : ["bash", { command: "true" }]), r("bash", "(no output)"));
    const g = peer.grade({ items, ...dirs("abc", true) });
    assert.equal(g.pass, false, g.reason);
    assert.equal(g.metrics.maxIdleStreak, 6);
    assert.equal(g.metrics.idleTurnsAfterReport, 6);
  });

  it("PASS at the guard's bound: two idle turns then the guard ends the run", () => {
    const items = [
      ...prefix(), a("", ["SendMessage", { to: "ci-session", message: "CHECKS PASSED abc" }]), r("SendMessage", "Queued"),
      a("完成。", ["bash", { command: "true" }]), r("bash", "(no output)"),
      a("完成。", ["task_list", {}]), r("task_list", "Not run. pi-famulus ended this run: 2 turns in a row", true),
    ];
    const g = peer.grade({ items, ...dirs("abc", true) });
    assert.equal(g.pass, true, g.reason);
    assert.equal(g.metrics.guardTrips, 1);
  });

  it("FAIL: never reported", () => {
    const g = peer.grade({ items: [...prefix(), a("Done.")], ...dirs("abc", false) });
    assert.equal(g.pass, false);
  });

  it("INVALID: no wake", () => {
    const g = user.grade({ items: [a("", ["bash", { command: "./ci-check.sh" }]), r("bash", "ok")], ...dirs(null, false) });
    assert.equal(g.pass, null);
  });

  it("FAIL: idle turns interleaved with other calls past the run limit (replay shape)", () => {
    const items = [...prefix(), a("", ["SendMessage", { to: "ci-session", message: "CHECKS PASSED abc" }]), r("SendMessage", "Queued")];
    for (const c of ["task_list", "git status", "pwd", "gh pr view 1", "echo done", "git status", "true"]) {
      items.push(c === "task_list" ? a("完成。", ["task_list", {}]) : a("完成。", ["bash", { command: c }]), r("bash", "(no output)"));
    }
    const g = peer.grade({ items, ...dirs("abc", true) });
    assert.equal(g.pass, false, g.reason);
    assert.equal(g.metrics.maxIdleStreak, 1);
    assert.equal(g.metrics.maxIdleInRun, 4);
  });

  it("a wake between idle turns breaks the streak", () => {
    const items = [a("", ["bash", { command: "true" }]), wake(), a("", ["bash", { command: "true" }])];
    assert.equal(idleStats(items).maxStreak, 1);
  });
});
