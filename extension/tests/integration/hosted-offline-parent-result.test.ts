/**
 * I-20. A warm hosted child keeps the daemon alive after the parent is
 * SIGKILLed. A background bash started by that parent then finishes once a
 * new extension instance has connected to the same session. The finished
 * result must arrive as a wake and task_list must list it.
 *
 * Distinct from impl-session-restart-recovery (I-19/I-40), which covers
 * work that was already gone when the new instance starts.
 *
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/hosted-offline-parent-result.test.ts
 */
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import {
  alive,
  createHostedDirs,
  FAUX_CHILD,
  HOSTED_INTEG,
  MANAGER_BIN,
  PI_CLI,
  startHostedHarness,
  textOf,
  waitUntil,
  type HostedHarness,
} from "./hosted-harness";

const HOLD_SESSION = fileURLToPath(new URL("../fixtures/hold-session.js", import.meta.url));
const EXTENSION_ENTRY = fileURLToPath(new URL("../../src/index.ts", import.meta.url));
const SESSION = "offline-sess";

describe.skipIf(!HOSTED_INTEG)("I-20 results that finish while the parent is gone", () => {
  let harness: HostedHarness | null = null;
  let holderPid = 0;

  afterEach(async () => {
    if (holderPid > 0 && alive(holderPid)) {
      try {
        process.kill(holderPid, "SIGKILL");
      } catch {
        // already gone
      }
    }
    holderPid = 0;
    await harness?.cleanup();
    harness = null;
  });

  it.fails("delivers the finished result as a wake and lists it after reconnect", async () => {
    const dirs = createHostedDirs();
    const readyPath = join(dirs.root, "holder-ready.json");
    const logPath = join(dirs.root, "holder.log");
    const tracePath = join(dirs.root, "faux-trace.txt");
    const command = `sleep 6; echo BG-DONE-${SESSION}`;
    const childArgv = [
      process.execPath, PI_CLI, "-ne", "-ns", "-np", "-nc", "--offline",
      "--mode", "rpc",
      "--session-id", "ch_keep",
      "--name", "worker: keeper",
      "-e", EXTENSION_ENTRY,
      "-e", FAUX_CHILD,
      "--model", "faux/faux-1",
      "--thinking", "off",
      "-na",
    ];
    const holder = spawn(process.execPath, [HOLD_SESSION], {
      env: {
        ...process.env,
        PI_FAMULUS_HOME: dirs.home,
        PI_FAMULUS_MANAGER_PATH: MANAGER_BIN,
        PI_FAMULUS_HOLD_SESSION: SESSION,
        PI_FAMULUS_HOLD_CWD: dirs.cwd,
        PI_FAMULUS_HOLD_READY: readyPath,
        PI_FAMULUS_HOLD_COMMAND: command,
        PI_FAMULUS_HOLD_LOG: logPath,
        PI_FAMULUS_HOLD_CHILD_ARGV: JSON.stringify(childArgv),
        PI_FAMULUS_TEST_FAUX: "hang",
        PI_FAMULUS_TEST_FAUX_TRACE: tracePath,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    holderPid = holder.pid ?? 0;
    const holderErr: string[] = [];
    holder.stderr?.on("data", (chunk) => holderErr.push(String(chunk)));
    holder.stdout?.on("data", (chunk) => holderErr.push(String(chunk)));
    try {
      await waitUntil(() => existsSync(readyPath), 45_000, "the first parent to start the child and the shell");
    } catch (error) {
      const holderLog = existsSync(logPath) ? readFileSync(logPath, "utf8") : "";
      throw new Error(`${error instanceof Error ? error.message : String(error)}\n${holderErr.join("")}\n${holderLog}`);
    }
    const ready = JSON.parse(readFileSync(readyPath, "utf8")) as { task_id: string; child_id: string; pid: number };
    expect(ready.task_id).toMatch(/^sh_/);
    process.kill(ready.pid, "SIGKILL");
    const killedPid = ready.pid;
    await waitUntil(() => !alive(killedPid), 5_000, "the first parent to exit");
    holderPid = 0;

    harness = await startHostedHarness({
      scenario: "hang",
      sessionId: SESSION,
      existing: dirs,
    });
    const h = harness;
    const sentBefore = h.sent.length;
    const listed = await h.tool("task_list").execute("list-1", { all: true }, undefined, undefined, h.ctx);
    const listedText = textOf(listed);
    await waitUntil(
      () => {
        const wakes = h.sent.slice(sentBefore).some((sent) => {
          const details = sent.message.details as { kind?: string } | undefined;
          const content = sent.message.content ?? "";
          return details?.kind === "task" && content.includes(ready.task_id);
        });
        return wakes;
      },
      20_000,
      "a task wake for the finished command",
    ).catch(() => {});
    const after = await h.tool("task_list").execute("list-2", { all: true }, undefined, undefined, h.ctx);
    const afterText = textOf(after);
    const wake = h.sent.slice(sentBefore).some((sent) => {
      const details = sent.message.details as { kind?: string } | undefined;
      const content = sent.message.content ?? "";
      return details?.kind === "task" && content.includes(ready.task_id);
    });
    expect({
      wake,
      listed: afterText.includes(ready.task_id) || listedText.includes(ready.task_id),
    }).toEqual({ wake: true, listed: true });
  }, 90_000);
});
