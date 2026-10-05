/**
 * Part 1: deterministic e2e — real `pi` (RPC mode) + real pi-famulus + our
 * extension, with a scripted faux model. Tests the code, not the model.
 *
 *   node --test e2e/faux.test.ts
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { after, describe, it } from "node:test";
import { type FauxEpisode, runFaux } from "./run-faux.ts";
import { followedByAssistant, type Item, toolResults, wakes } from "../lib/transcript.ts";
import { FAMULUS_WAKE_LEAD_IN } from "../lib/wake-adapter.ts";

const episodes: FauxEpisode[] = [];
after(() => {
  for (const e of episodes) e.sandbox.cleanup();
});

function explain(ep: FauxEpisode): string {
  const lines = ep.items.map((i) => {
    if (i.kind === "wake") return `#${i.seq} t=${i.t} WAKE ${i.wake.kind} ${i.wake.customType} ${i.wake.taskIds} ${i.wake.status}`;
    if (i.kind === "assistant") return `#${i.seq} t=${i.t} assistant ${JSON.stringify(i.text).slice(0, 80)} ${i.toolCalls.map((c) => c.name)}`;
    if (i.kind === "toolResult") return `#${i.seq} t=${i.t} toolResult ${i.toolName} ${JSON.stringify(i.text).slice(0, 120)}`;
    return `#${i.seq} t=${i.t} ${i.kind}`;
  });
  return `${lines.join("\n")}\nfaux calls: ${ep.calls.length}\nstderr: ${ep.stderr.slice(0, 800)}${
    ep.midwayError ? `\nmidwayError: ${ep.midwayError}` : ""
  }`;
}

/** Poll until `pid` is gone (ESRCH); false after timeoutMs. */
async function waitPidGone(pid: number, timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      process.kill(pid, 0);
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "ESRCH") return true;
      throw err;
    }
    if (Date.now() > deadline) return false;
    await new Promise((res) => setTimeout(res, 100));
  }
}

/** Every faux call was scripted: no unexpected LLM requests, no exhaustion. */
function assertScriptConsumedExactly(ep: FauxEpisode, n: number) {
  assert.equal(ep.calls.length, n, `expected ${n} model calls\n${explain(ep)}`);
  assert.ok(ep.calls.every((c) => c.scripted), `unscripted model call\n${explain(ep)}`);
}

const bashResult = (items: Item[]) => toolResults(items).find((r) => r.toolName === "bash");

/** Extension events from the episode's session events.jsonl files. */
function extensionEvents(ep: FauxEpisode): Array<Record<string, unknown> & { type: string }> {
  const sessions = join(ep.sandbox.famulusHome, "sessions");
  if (!existsSync(sessions)) return [];
  const out: Array<Record<string, unknown> & { type: string }> = [];
  for (const sid of readdirSync(sessions)) {
    const file = join(sessions, sid, "events.jsonl");
    if (!existsSync(file)) continue;
    for (const line of readFileSync(file, "utf8").split("\n")) {
      if (!line.trim()) continue;
      const e = JSON.parse(line) as Record<string, unknown> & { type: string };
      if (e.src === "extension") out.push(e);
    }
  }
  return out;
}

describe("faux e2e", { concurrency: true }, () => {
  it("(a) command over the foreground budget is backgrounded and notifies exactly once", async () => {
    const ep = await runFaux({
      script: "bg-once.ts",
      famulusConfig: { foregroundBudgetMs: 300 },
      until: (items) => wakes(items).length >= 1 && followedByAssistant(items, wakes(items)[0].seq),
      quietMs: 2500, // long enough for a duplicate notification to show up
    });
    episodes.push(ep);
    const result = bashResult(ep.items);
    assert.ok(result, explain(ep));
    assert.equal(result.details?.backgrounded, true, `tool result not backgrounded\n${explain(ep)}`);
    const taskId = String(result.details?.task_id);
    assert.match(taskId, /^sh_[0-9a-f]{8}$/);
    assert.match(result.text, new RegExp(`moved to background \\(task_id: ${taskId}\\)`));

    const ws = wakes(ep.items);
    assert.equal(ws.length, 1, `expected exactly one wake\n${explain(ep)}`);
    const [w] = ws;
    assert.equal(w.wake.customType, "pi-famulus-wake");
    assert.equal(w.wake.source, "details", "wake details missing from the event stream");
    assert.equal(w.wake.tasks[0].exitCode, 0);
    assert.equal(w.wake.leadIn, FAMULUS_WAKE_LEAD_IN);
    assert.equal(w.wake.kind, "task");
    assert.deepEqual(w.wake.taskIds, [taskId]);
    assert.equal(w.wake.status, "completed");
    assert.match(w.wake.body, /bg-canary-7731/);
    assert.ok(w.seq > result.seq);

    // The wake started a new model turn, and that turn saw the payload.
    assert.ok(followedByAssistant(ep.items, w.seq), `wake did not trigger a turn\n${explain(ep)}`);
    const last = ep.items.at(-1);
    assert.equal(last?.kind, "assistant");
    assert.match((last as { text: string }).text, /WAKE-HANDLED saw-canary/);
    assertScriptConsumedExactly(ep, 3);

    // Real pi emits extension message_end for the triggered wake: the
    // extension logs when it entered the context and how long it waited.
    const injects = extensionEvents(ep).filter((e) => e.type === "wake.inject");
    assert.equal(injects.length, 1, `expected one wake.inject\n${JSON.stringify(injects)}`);
    assert.equal(injects[0].kind, "task");
    assert.deepEqual(injects[0].ids, [taskId]);
    assert.equal(typeof injects[0].as_of, "number");
    assert.ok(typeof injects[0].lag_ms === "number" && injects[0].lag_ms >= 0, JSON.stringify(injects[0]));
  });

  it("(b) command within the foreground budget returns inline and produces no notification", async () => {
    const ep = await runFaux({
      script: "fg-quiet.ts",
      famulusConfig: { foregroundBudgetMs: 10_000 },
      quietMs: 2000,
    });
    episodes.push(ep);
    const result = bashResult(ep.items);
    assert.ok(result, explain(ep));
    assert.equal(result.isError, false);
    assert.match(result.text, /fg-quick-4410/);
    assert.notEqual(result.details?.backgrounded, true);
    assert.equal(wakes(ep.items).length, 0, `unexpected wake\n${explain(ep)}`);
    assert.equal(ep.items.filter((i) => i.kind === "custom").length, 0, explain(ep));
    assertScriptConsumedExactly(ep, 2);
  });

  it("(c) monitor lines arrive as monitor events, then exactly one timeout notice", async () => {
    const ep = await runFaux({
      script: "monitor-timeout.ts",
      until: (items) => wakes(items).some((w) => w.wake.status === "timeout"),
      quietMs: 2000,
    });
    episodes.push(ep);
    const monitorResult = toolResults(ep.items).find((r) => r.toolName === "monitor");
    assert.ok(monitorResult && !monitorResult.isError, explain(ep));
    const taskId = String(monitorResult.details?.task_id);
    assert.match(taskId, /^mon_[0-9a-f]{8}$/);

    const ws = wakes(ep.items);
    assert.ok(ws.every((w) => w.wake.customType === "pi-famulus-wake" && w.wake.kind === "monitor"), explain(ep));
    assert.ok(ws.every((w) => w.wake.taskIds[0] === taskId), explain(ep));
    const events = ws.filter((w) => w.wake.status === "event");
    const timeouts = ws.filter((w) => w.wake.status === "timeout");
    assert.ok(events.length >= 1, `no monitor events\n${explain(ep)}`);
    // All lines delivered, in order, across however many batches.
    const lines = events.flatMap((w) => w.wake.body.split("\n")).filter(Boolean);
    assert.deepEqual(lines, ["tick-1", "tick-2", "tick-3"], explain(ep));
    assert.equal(timeouts.length, 1, `expected one timeout notice\n${explain(ep)}`);
    assert.match(timeouts[0].wake.body, /Monitor timed out — re-arm if needed/);
    assert.ok(events.every((w) => w.seq < timeouts[0].seq), "event after timeout");
    // Killed by the timeout: no separate "exited" notice.
    assert.equal(ws.filter((w) => w.wake.status === "exited").length, 0, explain(ep));
    // Wakes got model turns (idle → triggerTurn; busy → steered into the running turn).
    assert.ok(followedByAssistant(ep.items, timeouts[0].seq), `timeout did not wake the agent\n${explain(ep)}`);
    const lastText = (ep.items.at(-1) as { text?: string }).text ?? "";
    assert.match(lastText, /WOKE: timeout/);
  });

  it("(e2) a manager crash ends a backgrounded command with an orphaned exit wake", async () => {
    // Keep in sync with scripts/manager-crash.ts (importing it here would pull
    // faux-dsl.ts -> @earendil-works/pi-ai, which only resolves inside pi).
    const commandMarker = "crash-start";
    let managerPid = 0;
    let runnerPid = 0;
    const ep = await runFaux({
      script: "manager-crash.ts",
      famulusConfig: { foregroundBudgetMs: 300 },
      midway: {
        when: (items) => toolResults(items).some((r) => r.toolName === "bash" && r.details?.backgrounded === true),
        act: (sb) => {
          // A stalled or failing manager must hang the test at most 3s and
          // fail with a diagnostic, not a cryptic JSON parse error.
          const mgr = sb.env.PI_FAMULUS_MANAGER_PATH;
          const fail = (what: string, detail: string): never => {
            throw new Error(`midway ${what} failed: ${detail}`);
          };
          const st = spawnSync(mgr, ["--home", sb.famulusHome, "status", "--json"], { encoding: "utf8", timeout: 3000 });
          if (st.error) fail("status", String(st.error));
          if (st.status !== 0) fail("status", `exit ${st.status}: ${st.stderr.slice(0, 300)}`);
          if (!st.stdout.trim()) fail("status", "empty stdout");
          let parsed: { pid?: unknown };
          try {
            parsed = JSON.parse(st.stdout) as { pid?: unknown };
          } catch (err) {
            throw new Error(`midway status failed: stdout is not JSON: ${(err as Error).message}: ${st.stdout.slice(0, 300)}`);
          }
          if (!Number.isInteger(parsed.pid) || (parsed.pid as number) <= 0) {
            fail("status", `pid is not a positive integer: ${JSON.stringify(parsed.pid)}`);
          }
          managerPid = parsed.pid as number;

          // The runner (process-group leader) must actually die for the
          // lifeline to count as working; capture it before the kill.
          const ls = spawnSync(mgr, ["--home", sb.famulusHome, "ls", "--json"], { encoding: "utf8", timeout: 3000 });
          if (ls.error) fail("ls", String(ls.error));
          if (ls.status !== 0) fail("ls", `exit ${ls.status}: ${ls.stderr.slice(0, 300)}`);
          if (!ls.stdout.trim()) fail("ls", "empty stdout");
          let rows: Array<{ kind?: string; title?: string; running?: boolean; pid?: unknown }>;
          try {
            rows = JSON.parse(ls.stdout) as typeof rows;
          } catch (err) {
            throw new Error(`midway ls failed: stdout is not JSON: ${(err as Error).message}: ${ls.stdout.slice(0, 300)}`);
          }
          const row = rows.find(
            (r) => r.kind === "shell" && r.running === true && typeof r.title === "string" && r.title.includes(commandMarker),
          );
          if (!row) {
            fail("ls", `no running shell task matching ${JSON.stringify(commandMarker)} in: ${ls.stdout.slice(0, 400)}`);
          }
          const foundRow = row as { pid?: unknown };
          if (!Number.isInteger(foundRow.pid) || (foundRow.pid as number) <= 0) {
            fail("ls", `runner pid is not a positive integer: ${JSON.stringify(foundRow.pid)}`);
          }
          runnerPid = foundRow.pid as number;

          process.kill(managerPid, "SIGKILL");
        },
      },
      until: (items) => wakes(items).some((w) => w.wake.kind === "task") && /WOKE/.test(JSON.stringify(items.at(-1) ?? {})),
      untilTimeoutMs: 20_000,
      quietMs: 1500,
    });
    episodes.push(ep);
    assert.ok(!ep.midwayError, explain(ep));
    assert.ok(managerPid > 0, explain(ep));
    assert.ok(runnerPid > 0, `runner pid not captured\n${explain(ep)}`);
    const bash = bashResult(ep.items);
    const taskId = String(bash?.details?.task_id);
    const ws = wakes(ep.items).filter((w) => w.wake.kind === "task");
    assert.equal(ws.length, 1, `expected one exit wake\n${explain(ep)}`);
    assert.deepEqual(ws[0].wake.taskIds, [taskId], explain(ep));
    assert.equal(ws[0].wake.status, "orphaned", explain(ep));
    assert.ok(followedByAssistant(ep.items, ws[0].seq), `the wake did not start a turn\n${explain(ep)}`);
    assert.match((ep.items.at(-1) as { text?: string }).text ?? "", /WOKE orphaned/);
    // The runner only exits after its process group is empty, so its death
    // proves the lifeline tore down `sleep 30` (SIGTERM, then SIGKILL).
    assert.ok(await waitPidGone(runnerPid), `runner pid ${runnerPid} still alive 10s after the manager crash\n${explain(ep)}`);
  });

  it("(e3) an in-place manager upgrade is invisible to a running command and monitor", async () => {
    let upgradeOut = "";
    let before: { pid?: number } = {};
    const ep = await runFaux({
      script: "manager-upgrade.ts",
      famulusConfig: { foregroundBudgetMs: 300 },
      midway: {
        when: (items) =>
          toolResults(items).some((r) => r.toolName === "bash" && r.details?.backgrounded === true) &&
          toolResults(items).some((r) => r.toolName === "monitor"),
        act: (sb) => {
          const m = sb.env.PI_FAMULUS_MANAGER_PATH;
          before = JSON.parse(spawnSync(m, ["--home", sb.famulusHome, "status", "--json"], { encoding: "utf8" }).stdout);
          const r = spawnSync(m, ["--home", sb.famulusHome, "upgrade"], { encoding: "utf8", timeout: 40_000 });
          upgradeOut = `${r.status} ${r.stdout}${r.stderr}`;
        },
      },
      until: (items) =>
        wakes(items).some((w) => w.wake.kind === "task") &&
        wakes(items).some((w) => w.wake.kind === "monitor" && w.wake.status === "exited"),
      untilTimeoutMs: 25_000,
      quietMs: 1500,
    });
    episodes.push(ep);
    assert.match(upgradeOut, /^0 upgraded in place/, `upgrade: ${upgradeOut}\n${explain(ep)}`);
    const after = JSON.parse(
      spawnSync(ep.sandbox.env.PI_FAMULUS_MANAGER_PATH, ["--home", ep.sandbox.famulusHome, "status", "--json"], { encoding: "utf8" }).stdout,
    );
    assert.equal(after.pid, before.pid, "same manager pid");
    assert.equal(after.generation, 1);

    // The command ran through the manager (no local fallback) and ends once,
    // with its real exit code.
    const bash = bashResult(ep.items);
    const taskId = String(bash?.details?.task_id);
    assert.match(taskId, /^sh_[0-9a-f]{8}$/, `not a manager task\n${explain(ep)}`);
    const taskWakes = wakes(ep.items).filter((w) => w.wake.kind === "task");
    assert.equal(taskWakes.length, 1, `exactly one exit wake\n${explain(ep)}`);
    assert.deepEqual(taskWakes[0].wake.taskIds, [taskId]);
    assert.equal(taskWakes[0].wake.tasks[0].exitCode, 5, explain(ep));

    // Every monitor line once, in order, across the upgrade.
    const monitorResult = toolResults(ep.items).find((r) => r.toolName === "monitor");
    const monId = String(monitorResult?.details?.task_id);
    const monWakes = wakes(ep.items).filter((w) => w.wake.kind === "monitor" && w.wake.taskIds[0] === monId);
    const lines = monWakes.filter((w) => w.wake.status === "event").flatMap((w) => w.wake.body.split("\n")).filter(Boolean);
    const detail = monWakes.map((w) => `${w.seq} ${w.wake.status} ${JSON.stringify(w.wake.body)}`).join("\n");
    assert.deepEqual(lines, Array.from({ length: 30 }, (_, i) => `m-${i + 1}`), `${detail}\n${explain(ep)}`);
    assert.equal(monWakes.filter((w) => w.wake.status === "exited").length, 1, explain(ep));
    assert.equal(monWakes.at(-1)?.wake.status, "exited", `exit before its last event\n${explain(ep)}`);
  });

  it("(c2) a monitor that exits at once ends with an exit wake, not a timeout", async () => {
    const ep = await runFaux({
      script: "monitor-fast-exit.ts",
      // A clean exit right after its event starts no turn of its own; the
      // user's next message is when the model sees it.
      midway: {
        when: (items) => wakes(items).some((w) => w.wake.status === "exited"),
        act: (_sandbox, pi) => pi.prompt("anything still running?"),
      },
      until: (items) =>
        toolResults(items).some((r) => r.toolName === "task_list") ||
        wakes(items).some((w) => w.wake.status === "timeout"),
      quietMs: 1500,
    });
    episodes.push(ep);
    const monitorResult = toolResults(ep.items).find((r) => r.toolName === "monitor");
    assert.ok(monitorResult && !monitorResult.isError, explain(ep));
    const taskId = String(monitorResult.details?.task_id);
    const ws = wakes(ep.items).filter((w) => w.wake.taskIds[0] === taskId);
    assert.equal(ws.filter((w) => w.wake.status === "timeout").length, 0, `stuck until timeout\n${explain(ep)}`);
    const exitedWakes = ws.filter((w) => w.wake.status === "exited");
    assert.equal(exitedWakes.length, 1, explain(ep));
    // The exit started no turn: the model's next response answers the user's
    // message, not the wake (cubic review: without this, the old
    // triggerTurn delivery passed too).
    const exitSeq = exitedWakes[0].seq;
    const nextUser = ep.items.find((i) => i.kind === "user" && i.seq > exitSeq);
    const nextAssistant = ep.items.find((i) => i.kind === "assistant" && i.seq > exitSeq);
    assert.ok(nextUser && nextAssistant && nextUser.seq < nextAssistant.seq, `exit started a turn\n${explain(ep)}`);
    // The exit notice never precedes the monitor's own lines (batches 2–4 of the
    // real-model eval had it first in 25 of 125 monitors).
    const firstEvent = ws.findIndex((w) => w.wake.status === "event");
    assert.ok(firstEvent >= 0 && firstEvent < ws.indexOf(exitedWakes[0]), `exit before its event\n${explain(ep)}`);
    // `echo noop` ends in milliseconds; the exit must not wait on anything.
    assert.ok(exitedWakes[0].t - monitorResult.t < 2000, `exit wake late\n${explain(ep)}`);
    const lines = ws.filter((w) => w.wake.status === "event").flatMap((w) => w.wake.body.split("\n"));
    assert.ok(lines.includes("noop"), `first line lost\n${explain(ep)}`);
    const listing = toolResults(ep.items).find((r) => r.toolName === "task_list");
    assert.ok(listing, explain(ep));
    assert.doesNotMatch(listing.text, new RegExp(`${taskId}[^\n]*running`), `still listed as running\n${explain(ep)}`);
  });

  it("(b2) a command killed by its own timeout is reported as timed out", async () => {
    const ep = await runFaux({ script: "bash-timeout.ts", until: (items) => toolResults(items).some((r) => r.toolName === "bash") });
    episodes.push(ep);
    const r = toolResults(ep.items).find((x) => x.toolName === "bash");
    assert.ok(r, explain(ep));
    assert.equal(r.isError, true, `killed command reported as success: ${r.text}\n${explain(ep)}`);
    assert.match(r.text, /started[\s\S]*timed out after 1 seconds/, explain(ep));
  });

  it("(c3) a monitor that prints and exits while the agent is busy: event first, exit starts no turn", async () => {
    const ep = await runFaux({
      script: "monitor-busy-exit.ts",
      until: (items) => wakes(items).some((w) => w.wake.status === "exited"),
      quietMs: 1500,
    });
    episodes.push(ep);
    const monitorResult = toolResults(ep.items).find((r) => r.toolName === "monitor");
    assert.ok(monitorResult && !monitorResult.isError, explain(ep));
    const ws = wakes(ep.items).filter((w) => w.wake.taskIds[0] === String(monitorResult.details?.task_id));
    const event = ws.find((w) => w.wake.status === "event");
    const exited = ws.find((w) => w.wake.status === "exited");
    assert.ok(event && exited, explain(ep));
    assert.ok(event.seq < exited.seq, `exit before its event\n${explain(ep)}`);
  });

  it(
    "behavior guidelines are in the system prompt of the wake-triggered turn",
    // Regression: guidelines used to be a forced before_agent_start systemPrompt,
    // which wake-triggered runs never saw (fixed in a164135).
    async () => {
      const ep = await runFaux({
        script: "bg-once.ts",
        famulusConfig: { foregroundBudgetMs: 300 },
        until: (items) => wakes(items).length >= 1 && followedByAssistant(items, wakes(items)[0].seq),
      });
      episodes.push(ep);
      const heading = "Background tasks and notifications (pi-famulus)";
      const sees = ep.calls.map((c) => JSON.stringify(c.messages).includes(heading));
      assert.equal(sees.length, 3, explain(ep));
      assert.ok(sees[0], "guidelines missing on the user-prompted turn");
      assert.ok(sees[2], "guidelines missing on the wake-triggered turn");
    },
  );

  it(
    "subagent children can call contact_supervisor",
    // Regression: the child tool allowlist used to filter out custom tools.
    async () => {
      const ep = await runFaux({
        script: "exercise-surfaces.ts",
        famulusConfig: { foregroundBudgetMs: 300 },
        until: (items) => wakes(items).some((w) => w.wake.kind === "subagent-done"),
        untilTimeoutMs: 15_000,
      });
      episodes.push(ep);
      const childSawTool = ep.calls.some((c) =>
        c.messages.some((m) => m.role === "system" && JSON.stringify(m).includes('"name":"contact_supervisor"')),
      );
      assert.ok(childSawTool, "contact_supervisor is not declared in any child session");
      assert.ok(wakes(ep.items).some((w) => w.wake.kind === "supervisor-request"), explain(ep));
    },
  );

  it(
    "cold start: first backgrounded command right after launch still notifies",
    // Regression: concurrent connects rebound each other and the manager exited,
    // killing the task (fixed in c59d926).
    async () => {
      const ep = await runFaux({
        script: "bg-once.ts",
        famulusConfig: { foregroundBudgetMs: 300 },
        warm: false,
        until: (items) => wakes(items).length >= 1,
        untilTimeoutMs: 6000,
      });
      episodes.push(ep);
      assert.equal(wakes(ep.items).length, 1, `no task wake after cold start\n${explain(ep)}`);
    },
  );
});
