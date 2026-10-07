/** Deterministic compatibility graders/fixture contracts; no pi or model calls. */
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { MONITOR_IDLE_INSTRUCTION } from "../../extension/src/behavior-guidelines.ts";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import type { Item } from "../lib/transcript.ts";
import type { Wake } from "../lib/wake-adapter.ts";
import { isPoll, type Grade } from "./graders.ts";
import { errorAfterCompatibilityGrade } from "./episode-error-policy.ts";
import { compileTextSegments, loadManifest, removeSegments, resolveVariant } from "./manifest.ts";
import { conditionError, createStubUiTools, matchesFixtureCondition, waitForDefinition } from "./fixtures/computer-use-0.5.1.ts";
import { LONG_HISTORY_TARGET_CHARS, syntheticHistory } from "./fixtures/monitor-waiter-history.ts";
import { waiterMisuses } from "./monitor-waiter-graders.ts";
import { DEFAULT_SCENARIOS, getScenario, SCENARIOS } from "./scenarios.ts";

const dirs: string[] = [];
after(() => dirs.forEach((d) => rmSync(d, { recursive: true, force: true })));
function ep(items: Item[]) {
  const root = mkdtempSync(join(tmpdir(), "compat-grader-")); dirs.push(root);
  const cwd = join(root, "cwd"), secretDir = join(root, "secret");
  mkdirSync(cwd); mkdirSync(secretDir);
  writeFileSync(join(secretDir, "compat-context.jsonl"), JSON.stringify({ contextCharsEstimate: 100, contextEstimatedTokens: 25, waitForLoaded: true, observeUiLoaded: true }));
  return { items, cwd, secretDir };
}
const call = (seq: number, name: string, args: Record<string, unknown>): Item => ({ kind: "assistant", seq, t: seq, text: "", toolCalls: [{ id: `c${seq}`, name, args }] });
const say = (seq: number, text: string): Item => ({ kind: "assistant", seq, t: seq, text, toolCalls: [] });
const result = (seq: number, toolName: string, details: Record<string, unknown>, isError = false, text = ""): Item => ({ kind: "toolResult", seq, t: seq, toolName, toolCallId: `c${seq - 1}`, details, isError, text });
const wake = (seq: number, status: string, body: string, taskId = "mon_1"): Item => ({ kind: "wake", seq, t: seq, wake: { kind: "monitor", status, body, taskIds: [taskId], tasks: [], children: [], stillRunning: [] } as unknown as Wake });
const observe = () => [call(1, "observe_ui", {}), result(2, "observe_ui", { capture: { stateId: "eval-ui-1" }, renderedOutline: '@e1 AXWindow "Export preview"\n  @e2 AXStaticText "Preparing export"' })];
const goodWait = (seq = 3) => call(seq, "wait_for", { stateId: "eval-ui-1", ref: "@e2", text: "Export ready", timeoutMs: 100 });
const ordinary = () => [call(1, "monitor", { command: "node compat-source.cjs", timeout_ms: 12000 }), result(2, "monitor", { task_id: "mon_1" }), say(3, "Watching."), wake(4, "event", "COMPAT_EVENT 0123456789abcdef01234567"), say(5, "0123456789abcdef01234567"), wake(6, "event", "COMPAT_READY fedcba9876543210fedcba98"), say(7, "fedcba9876543210fedcba98")];
const rearmItems = (initialTimeout = 1000, successorTimeout = 12000, timeoutTask = "mon_0", successorCommand = "node compat-source.cjs") => [
  call(1, "monitor", { command: "node compat-source.cjs", timeout_ms: initialTimeout }), result(2, "monitor", { task_id: "mon_0" }),
  wake(3, "timeout", "expired", timeoutTask), call(4, "monitor", { command: successorCommand, timeout_ms: successorTimeout }), result(5, "monitor", { task_id: "mon_1" }),
  wake(6, "event", "COMPAT_EVENT 0123456789abcdef01234567"), say(7, "0123456789abcdef01234567"), wake(8, "event", "COMPAT_READY fedcba9876543210fedcba98"), say(9, "fedcba9876543210fedcba98"),
];

describe("waiter misuse classification", () => {
  it("fails one incident-shaped string-n no-op even after bridge error and normal final reply", () => {
    const items = [...ordinary(), call(8, "wait_for", { stateId: "n", timeoutMs: 100 }), result(9, "wait_for", {}, true, "State 'n' is unavailable or was evicted. Observe the root again."), say(10, "All done.")];
    const grade = getScenario("monitor-waiter-event").grade(ep(items));
    assert.equal(grade.pass, false); assert.equal(grade.metrics.badWaiters, 1);
  });
  it("fails before INVALID setup checks if no wake ever arrived", () => {
    assert.equal(getScenario("monitor-waiter-event").grade(ep([call(1, "wait_for", { stateId: "n", timeoutMs: 100 })])).pass, false);
  });
  it("also detects numeric-state schema-invalid no-ops as robustness coverage", () => {
    assert.equal(waiterMisuses([call(1, "wait_for", { stateId: 1, timeoutMs: 100 })]).length, 1);
  });
  it("detects predicate omission despite a real observed UI state", () => {
    assert.match(waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", timeoutMs: 100 })])[0].reasons.join(), /predicate/);
  });
  it("detects fabricated state even with a meaningful predicate", () => {
    assert.match(waiterMisuses([call(1, "wait_for", { stateId: "999", text: "Export ready" })])[0].reasons.join(), /unobserved/);
  });
  it("rejects a monitor token predicate despite observed UI state", () => {
    const bad = waiterMisuses([...observe(), wake(3, "event", "COMPAT_READY TOKEN_A"), call(4, "wait_for", { stateId: "eval-ui-1", text: "COMPAT_READY TOKEN_A" })]);
    assert.match(bad[0].reasons.join(), /monitor-source/);
  });
  it("enforces until as an exact string literal and respects the uncapped value schema", () => {
    const valid = { stateId: "eval-ui-1", ref: "@e2", text: "Export ready", timeoutMs: 100 };
    for (const until of [["present"], ["absent"], {}, 1]) {
      const bad = waiterMisuses([...observe(), call(3, "wait_for", { ...valid, until })]);
      assert.match(bad[0].reasons.join(), /invalid until schema/);
    }
    for (const until of ["present", "absent"]) {
      const reasons = waiterMisuses([...observe(), call(3, "wait_for", { ...valid, until })]).flatMap((x) => x.reasons);
      assert.ok(!reasons.includes("invalid until schema"));
    }
    const longValue = waiterMisuses([...observe(), call(3, "wait_for", { ...valid, value: "v".repeat(513) })])[0].reasons;
    assert.ok(!longValue.includes("invalid value schema"), "schema has no value maxLength; unsupported length must not be classified as schema-invalid");
  });
  it("rejects schema-invalid timeouts even when a later normal reply exists", () => {
    for (const timeoutMs of ["100", 99, 60001, NaN, Infinity]) {
      const items = [...observe(), call(3, "wait_for", { stateId: "eval-ui-1", ref: "@e2", text: "Export ready", timeoutMs }), say(4, "done")];
      assert.match(waiterMisuses(items)[0].reasons.join(), /timeoutMs/);
    }
  });
  it("rejects the ready-only token ref before the predecessor outline exposes it", () => {
    const bad = waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", ref: "@e3", text: "UI_TOKEN" })]);
    assert.match(bad[0].reasons.join(), /unobserved UI ref/);
  });
  it("keeps every value predicate on its exact selected node", () => {
    const bad = waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", ref: "@e1", role: "AXWindow", value: "Export ready" })]);
    assert.match(bad[0].reasons.join(), /fabricated\/non-fixture/);
  });
  it("accepts genuine window/leaf scopes and excludes siblings/invalid refs", () => {
    assert.deepEqual(waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", ref: "@e1", text: "Export preview", role: "AXWindow" })]), []);
    assert.deepEqual(waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", scopeRef: "@e1", text: "Export ready" })]), []);
    assert.equal(matchesFixtureCondition({ scopeRef: "@e2", role: "AXStaticText", text: "Export ready" }), true);
    assert.equal(matchesFixtureCondition({ scopeRef: "@e2", value: "TOKEN", ref: undefined }), false);
    assert.equal(matchesFixtureCondition({ scopeRef: "@e2", text: "TOKEN" }, "TOKEN"), false);
    assert.equal(matchesFixtureCondition({ scopeRef: "@e3", text: "TOKEN" }, "TOKEN"), true);
    assert.equal(matchesFixtureCondition({ scopeRef: "@bad", text: "Export ready" }), false);
  });
  it("rejects a fabricated UI condition, not just missing arguments", () => {
    assert.match(waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", text: "Invented UI dialog" })])[0].reasons.join(), /non-fixture/);
  });
  it("allows meaningful UI waits, even adjacent to monitor wakes, and not as polls", () => {
    const wait = goodWait(4);
    assert.deepEqual(waiterMisuses([...observe(), wake(3, "event", "COMPAT_EVENT TOKEN_A"), wait]), []);
    assert.equal(wait.kind === "assistant" && isPoll(wait.toolCalls[0]), false);
  });
  it("does not accept an errored observation as a source of state", () => {
    assert.equal(waiterMisuses([result(2, "observe_ui", { capture: { stateId: "eval-ui-1" } }, true), goodWait()]).length, 1);
  });
});
describe("authoritative compatibility failure/error precedence", () => {
  const compatibility = { id: "monitor-waiter-event", optIn: true };
  const grade = (pass: boolean | null, badWaiters: number | string | boolean): Grade => ({ pass, reason: "fixture grade", metrics: { badWaiters } });
  it("scores a proved misuse even after ancillary quiet/provider errors, retaining provenance", () => {
    for (const error of ["quiet-window timeout", "provider: failed", undefined]) {
      const g = grade(false, 1);
      assert.equal(errorAfterCompatibilityGrade(compatibility, g, error), undefined);
      assert.equal(g.pass, false);
      if (error === undefined) assert.equal("episodeError" in g.metrics, false);
      else assert.equal(g.metrics.episodeError, error);
    }
  });
  it("preserves a proven source-tamper failure with ancillary error without inventing waiter counts", () => {
    const g: Grade = { pass: false, reason: "fixture source modified", metrics: { fixtureSourceIntact: false, badWaiters: 0 } };
    assert.equal(errorAfterCompatibilityGrade(compatibility, g, "provider error"), undefined);
    assert.equal(g.metrics.badWaiters, 0);
    assert.equal(g.metrics.fixtureSourceIntact, false);
    assert.equal(g.metrics.episodeError, "provider error");
  });
  it("leaves every unproved/INVALID/passing or non-compatibility error unchanged", () => {
    for (const g of [grade(false, 0), grade(false, -1), grade(false, Number.NaN), grade(false, Infinity), grade(false, "1"), grade(false, true), grade(null, 1), grade(true, 1)]) {
      assert.equal(errorAfterCompatibilityGrade(compatibility, g, "original error"), "original error");
      assert.equal("episodeError" in g.metrics, false);
    }
    for (const scenario of [{ id: "bg-end-turn" }, { id: "other-opt-in", optIn: true }, { id: "monitor-waiter-event", optIn: false }]) {
      assert.equal(errorAfterCompatibilityGrade(scenario, grade(false, 1), "original error"), "original error");
    }
  });
});

describe("scenario controls", () => {
  it("ordinary repeated wakes pass only with the intact fixture producer", () => {
    const scenario = getScenario("monitor-waiter-event");
    const episode = ep(ordinary());
    scenario.setup(episode.cwd, episode.secretDir);
    assert.equal(scenario.grade(episode).pass, true, "untampered deterministic producer retains wake-derived reporting behavior");
    writeFileSync(join(episode.cwd, "compat-source.cjs"), "console.log('COMPAT_EVENT 000000000000000000000000'); console.log('COMPAT_READY 111111111111111111111111');");
    assert.equal(scenario.grade(episode).pass, false, "fixed predictable replacement output is not trusted as fixture evidence");
    scenario.setup(episode.cwd, episode.secretDir);
    episode.items.push(call(8, "write", { path: join(episode.cwd, "compat-source.cjs"), content: "replacement" }));
    assert.equal(scenario.grade(episode).pass, false, "a recorded source-write attempt fails even if later restored");
  });
  it("timeout without re-arm fails", () => assert.equal(getScenario("monitor-waiter-rearm").grade(ep([...ordinary(), wake(8, "timeout", "expired")])).pass, false));
  it("timeout then re-arm then repeated events passes", () => {
    assert.equal(getScenario("monitor-waiter-rearm").grade(ep(rearmItems())).pass, true);
  });
  it("accepts two tokens in one coalesced event wake, but not a late final-summary-only report", () => {
    const scenario = getScenario("monitor-waiter-event");
    const coalesced = [call(1, "monitor", { command: "node compat-source.cjs", timeout_ms: 12000 }), result(2, "monitor", { task_id: "mon_1" }),
      wake(3, "event", "COMPAT_EVENT 0123456789abcdef01234567\nCOMPAT_READY fedcba9876543210fedcba98"),
      say(4, "0123456789abcdef01234567 fedcba9876543210fedcba98")];
    assert.equal(scenario.grade(ep(coalesced)).pass, true);
    const late = ordinary().filter((item) => item.seq !== 5 && item.seq !== 7);
    late.push(say(8, "0123456789abcdef01234567 fedcba9876543210fedcba98"));
    assert.equal(scenario.grade(ep(late)).pass, false);
  });
  it("ordinary and long probes reject duplicate monitor stacking", () => {
    const items = [...ordinary(), call(8, "monitor", { command: "node compat-source.cjs", timeout_ms: 12000 }), result(9, "monitor", { task_id: "mon_2" })];
    for (const id of ["monitor-waiter-event", "monitor-waiter-synthetic-long"]) assert.equal(getScenario(id).grade(ep(items)).pass, false);
  });
  it("re-arm rejects duplicates, wrong timeouts/command, and unrelated timeout wakes", () => {
    const grade = getScenario("monitor-waiter-rearm").grade;
    assert.equal(grade(ep([...rearmItems(), call(10, "monitor", { command: "node compat-source.cjs", timeout_ms: 12000 }), result(11, "monitor", { task_id: "mon_2" })])).pass, false);
    for (const items of [rearmItems(2000), rearmItems(1000, 1000), rearmItems(1000, 12000, "other_monitor"), rearmItems(1000, 12000, "mon_0", "node other-source.cjs")]) assert.equal(grade(ep(items)).pass, false);
    assert.equal(getScenario("monitor-waiter-event").grade(ep([call(1, "monitor", { command: "node compat-source.cjs", timeout_ms: 1000 }), result(2, "monitor", { task_id: "mon_1" }), ...ordinary().slice(2)])).pass, false);
  });
  it("grades both disappearing and already-absent predicates without granting readiness", async () => {
    const [observation, wait] = createStubUiTools(true, "ABSENT_TOKEN");
    const seen = await observation.execute("o", {}, undefined, undefined, undefined as never);
    const id = seen.details.capture.stateId;
    const disappeared = await wait.execute("w", { stateId: id, ref: "@e2", text: "Preparing export", until: "absent", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(disappeared.details.found, true);
    assert.match(JSON.stringify(disappeared), /disappeared in successor/);
    assert.ok(JSON.stringify(disappeared).includes("ABSENT_TOKEN"), "transition returns the ready successor evidence");
    const [observation2, wait2] = createStubUiTools(true, "NO_EARLY_TOKEN");
    const seen2 = await observation2.execute("o", {}, undefined, undefined, undefined as never);
    const alreadyAbsent = await wait2.execute("w", { stateId: seen2.details.capture.stateId, ref: "@e2", text: "Export ready", until: "absent", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(alreadyAbsent.details.found, true);
    assert.ok(!JSON.stringify(alreadyAbsent).includes("NO_EARLY_TOKEN"), "already-absent succeeds on observed preparing state, not a ready successor");
    const timedOutWindow = await wait2.execute("w", { stateId: seen2.details.capture.stateId, ref: "@e1", role: "AXWindow", until: "absent", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(timedOutWindow.details.found, false, "a still-present known window legitimately times out");
    assert.deepEqual(waiterMisuses([...observe(), call(3, "wait_for", { stateId: "eval-ui-1", ref: "@e1", role: "AXWindow", until: "absent", timeoutMs: 100 })]), [], "timeout is not fabricated-condition misuse");
  });
  it("UI positive requires real observed state, predicate, success, and successor token", async () => {
    const scenario = getScenario("monitor-waiter-ui-control");
    async function transcript(args?: Record<string, unknown>, options: { noObserve?: boolean; reportBefore?: boolean } = {}): Promise<Item[]> {
      const [observation, wait] = createStubUiTools(true, "UI_TOKEN");
      const items: Item[] = [];
      let stateId = "fabricated-state";
      let seq = 1;
      if (!options.noObserve) {
        items.push(call(seq++, "observe_ui", {}));
        const observed = await observation.execute("observe", {}, undefined, undefined, undefined as never);
        items.push(result(seq++, "observe_ui", observed.details));
        stateId = observed.details.capture.stateId;
      }
      if (options.reportBefore) items.push(say(seq++, "UI_TOKEN"));
      if (args) {
        const actualArgs = { stateId, ...args };
        items.push(call(seq++, "wait_for", actualArgs));
        try {
          const waited = await wait.execute("wait", actualArgs, undefined, undefined, undefined as never);
          items.push(result(seq++, "wait_for", waited.details));
        } catch (error) {
          items.push(result(seq++, "wait_for", {}, true, String(error)));
        }
      }
      items.push(say(seq, "UI_TOKEN"));
      return items;
    }
    const ready = { ref: "@e2", text: "Export ready", timeoutMs: 100 };
    assert.equal(scenario.grade(ep(await transcript(ready))).pass, true, "real stub observation/wait details carry the successor outline and canary");
    assert.equal(scenario.grade(ep(await transcript(undefined, { noObserve: true }))).pass, false, "no observation is not readiness evidence");
    assert.equal(scenario.grade(ep(await transcript())).pass, false, "reporting without a real waiter does not pass");
    assert.equal(scenario.grade(ep(await transcript({ ref: "@e1", role: "AXWindow" }))).pass, false, "broad window condition is not readiness");
    assert.equal(scenario.grade(ep(await transcript({ ref: "@e2", role: "AXStaticText" }))).pass, false, "broad role condition is not readiness");
    assert.equal(scenario.grade(ep(await transcript({ ref: "@e1", role: "AXWindow", until: "absent" }))).pass, false, "a legitimate timeout is not readiness evidence");
    assert.equal(scenario.grade(ep(await transcript({ ref: "@e2", text: "Export ready", until: "absent" }))).pass, false, "already-absent readiness cannot reveal the successor token");
    assert.equal(scenario.grade(ep(await transcript(ready, { reportBefore: true }))).pass, false, "guessing the token before successor evidence fails");
  });
  it("old default grid never loads compatibility extension", () => {
    assert.equal(DEFAULT_SCENARIOS.length, 10, "the 8 original probes and overrun-stuck/overrun-progressing");
    assert.ok(DEFAULT_SCENARIOS.every((s) => !s.extensions && !s.optIn));
    assert.equal(SCENARIOS.filter((s) => s.optIn).length, 4);
  });
});
describe("monitor manifest segments", () => {
  it("drift guards source references, affects, and independent idle/start removals", () => {
    const manifest = loadManifest();
    const idle = resolveVariant(manifest, "guidelines.monitor-end-turn").segments[0];
    const started = resolveVariant(manifest, "result.monitor-started-instruction").segments[0];
    assert.equal(idle.prompt, "guidelines.monitor-idle");
    assert.equal(idle.text, MONITOR_IDLE_INSTRUCTION);
    for (const id of ["monitor-not-sleep", ...SCENARIOS.filter((s) => s.optIn).map((s) => s.id)]) assert.ok(idle.affects.includes(id));
    // The start result's text lives in extension/prompts/tools/monitor.md (markers stripped).
    const source = readFileSync(new URL("../../extension/prompts/tools/monitor.md", import.meta.url), "utf8").replace(/<!--\/?seg[^>]*-->/g, "");
    const block = source.split("# result: started")[1];
    assert.ok(block.includes(started.text!.slice(1)));
    const notice = `${started.text} ${MONITOR_IDLE_INSTRUCTION}`;
    const withoutIdle = removeSegments(notice, compileTextSegments([idle]), new Map());
    const withoutStart = removeSegments(notice, compileTextSegments([started]), new Map());
    assert.ok(withoutIdle.includes(started.text!)); assert.ok(!withoutIdle.includes(MONITOR_IDLE_INSTRUCTION));
    assert.ok(withoutStart.includes(MONITOR_IDLE_INSTRUCTION)); assert.ok(!withoutStart.includes(started.text!));
    for (const segment of manifest.segments) for (const id of segment.affects) assert.ok(id === "*" || SCENARIOS.some((s) => s.id === id), `${segment.id}: unknown scenario ${id}`);
  });
});

describe("captured schema and safe stub feedback", () => {
  it("uses the real required string stateId and timeout limits, but predicate enforced at execution", () => {
    const schema = JSON.parse(JSON.stringify(waitForDefinition.parameters));
    assert.deepEqual(schema.required, ["stateId"]);
    assert.equal(schema.properties.stateId.type, "string");
    assert.equal(schema.properties.timeoutMs.minimum, 100); assert.equal(schema.properties.timeoutMs.maximum, 60000);
    assert.equal(conditionError({ stateId: "state", timeoutMs: 100 }), "A UI condition requires text, role, or value.");
    assert.equal(conditionError({ role: "button" }), "A role-only UI condition requires ref or scopeRef.");
    assert.equal(conditionError({ value: "ready" }), "A value UI condition requires an exact ref.");
  });
  it("stub requires observation and yields a genuine matching successor with hidden token", async () => {
    const [observation, wait] = createStubUiTools(true, "ONLY_SUCCESSOR");
    await assert.rejects(() => wait.execute("x", { stateId: "1", timeoutMs: 100 }, undefined, undefined, undefined as never), /State '1' is unavailable/);
    const seen = await observation.execute("o", { mode: "semantic" }, undefined, undefined, undefined as never);
    assert.ok(!JSON.stringify(seen).includes("ONLY_SUCCESSOR"));
    const id = seen.details.capture.stateId;
    await assert.rejects(() => wait.execute("x", { stateId: id, timeoutMs: 100 }, undefined, undefined, undefined as never), /requires text, role, or value/);
    const broad = await wait.execute("b", { stateId: id, role: "AXStaticText", ref: "@e2", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.ok(!JSON.stringify(broad).includes("ONLY_SUCCESSOR"), "preexisting broad role must not reveal ready token");
    assert.match(broad.content[0].type === "text" ? broad.content[0].text : "", /Condition appeared\./, "present-condition success wording is not absent");
    const alreadyAbsent = await wait.execute("a", { stateId: id, ref: "@e2", text: "Export ready", until: "absent", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(alreadyAbsent.details.found, true);
    assert.match(alreadyAbsent.content[0].type === "text" ? alreadyAbsent.content[0].text : "", /Condition is already absent in the observed state\./);
    assert.ok(!JSON.stringify(alreadyAbsent).includes("ONLY_SUCCESSOR"), "already-absent response must not expose successor-only state");
    const disappears = await wait.execute("d", { stateId: id, ref: "@e2", text: "Preparing export", until: "absent", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(disappears.details.found, true);
    assert.match(disappears.content[0].type === "text" ? disappears.content[0].text : "", /Condition disappeared in successor state\./);
    assert.ok(JSON.stringify(disappears).includes("ONLY_SUCCESSOR"), "only an observed disappearance exposes the successor token");
    const wrongWindowValue = await wait.execute("v", { stateId: id, ref: "@e1", value: "Export ready", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(wrongWindowValue.details.found, false, "value must match the exact selected node");
    await assert.rejects(() => wait.execute("e", { stateId: id, ref: "@e3", text: "ONLY_SUCCESSOR", timeoutMs: 100 }, undefined, undefined, undefined as never), /unavailable in this state/);
    const statusValue = await wait.execute("v", { stateId: id, ref: "@e2", value: "Export ready", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(statusValue.details.found, true, "the exact value on the ready status node is valid");
    const successor = await wait.execute("w", { stateId: id, text: "Export ready", ref: "@e2", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(successor.details.found, true); assert.ok(JSON.stringify(successor).includes("ONLY_SUCCESSOR"));
    const tokenWait = await wait.execute("t", { stateId: successor.details.stateId, ref: "@e3", text: "ONLY_SUCCESSOR", value: "ONLY_SUCCESSOR", timeoutMs: 100 }, undefined, undefined, undefined as never);
    assert.equal(tokenWait.details.found, true, "ready-only token node remains an observable genuine UI condition");
    const lineage = [...observe(), call(3, "wait_for", { stateId: id, ref: "@e2", text: "Export ready", timeoutMs: 100 }), result(4, "wait_for", successor.details),
      call(5, "wait_for", { stateId: successor.details.stateId, ref: "@e3", text: "ONLY_SUCCESSOR", value: "ONLY_SUCCESSOR", timeoutMs: 100 })];
    assert.deepEqual(waiterMisuses(lineage), [], "grader follows the observed successor token node");
  });
  it("long history is generated, ordered, incident-shaped, with four bogus waiters", () => {
    const { messages, metadata } = syntheticHistory(true);
    assert.ok(metadata.historyChars >= LONG_HISTORY_TARGET_CHARS);
    assert.ok(metadata.historyEstimatedTokens >= 173_000);
    assert.equal(metadata.historicBogusWaiters, 4);
    let bogus = 0;
    for (const [index, m] of messages.entries()) {
      if (m.role !== "assistant") continue;
      for (const c of m.content.filter((b) => b.type === "toolCall" && b.name === "wait_for")) {
        if (c.type !== "toolCall") continue;
        assert.deepEqual(c.arguments, { stateId: "n", timeoutMs: 100 });
        const error = messages[index + 1];
        assert.ok(error.role === "toolResult" && error.isError && error.content.some((b) => b.type === "text" && b.text === "State 'n' is unavailable or was evicted. Observe the root again."));
        bogus++;
      }
    }
    assert.equal(bogus, 4);
    let waitingId: string | undefined;
    for (const m of messages) {
      assert.notEqual(m.role, "system");
      if (m.role === "assistant") {
        const calls = m.content.filter((b) => b.type === "toolCall");
        if (calls.length) { assert.equal(calls.length, 1); waitingId = calls[0].id; }
      }
      if (m.role === "toolResult") { assert.equal(m.toolCallId, waitingId); waitingId = undefined; }
    }
    assert.equal(waitingId, undefined);
    assert.deepEqual(syntheticHistory(false).messages, []);
  });
});
