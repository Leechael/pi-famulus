/**
 * End-to-end integration: TS ManagerClient <-> real pi-famulus binary.
 *
 * Opt-in (spawns real processes, takes ~10s including the idle-reaper check):
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/real-manager.test.ts
 *
 * Uses the release binary at ../../manager/target/release/pi-famulus unless
 * PI_FAMULUS_MANAGER_PATH overrides it. Runs in an isolated Famulus home under tmpdir.
 */
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, lstatSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { ManagerClient, type ManagerEvent } from "../../src/manager-client";
import { reregisterAgentLeases } from "../../src/subagent/admission";
import { famulusPaths } from "../../src/config";

const RUN = process.env.PI_FAMULUS_INTEG === "1";
const BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/release/pi-famulus");
if (RUN && !existsSync(BIN)) {
  throw new Error(`Requested pi-famulus integration binary does not exist: ${BIN}`);
}

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

describe.skipIf(!RUN)("real pi-famulus integration", () => {
  const home = mkdtempSync(join(tmpdir(), "pi-famulus-integ-"));
  const paths = famulusPaths(home);
  const events: ManagerEvent[] = [];
  let client: ManagerClient;

  afterAll(async () => {
    await client?.close();
    // Preserve this suite's home before shutdown/unlink, not only eval-*.
    // The hidden cold-restart failure had no artifact or connect reason.
    const artifacts = process.env.PI_FAMULUS_TEST_ARTIFACTS;
    if (artifacts) {
      try {
        const destination = join(artifacts, basename(home));
        cpSync(home, destination, {
          recursive: true,
          filter: (source) => {
            try {
              const stat = lstatSync(source);
              // Never copy sockets/symlinks or unbounded task output.
              return stat.isDirectory() || (stat.isFile() && stat.size <= 1024 * 1024);
            } catch {
              return false; // A daemon may remove its pid/socket concurrently.
            }
          },
        });
        const ps = execFileSync("ps", ["-eo", "pid,ppid,stat,etime,args"], {
          encoding: "utf8", timeout: 2000,
        });
        const lines = ps.split("\n");
        writeFileSync(join(destination, "processes.txt"),
          [lines[0], ...lines.slice(1).filter((line) => line.includes(basename(home)))].join("\n"));
      } catch (error) {
        // Diagnostics must not mask the actual test failure or skip cleanup.
        console.warn("Could not preserve integration diagnostics:", error);
      }
    }
    // Last test reconnects a daemon that stays up ~5s after close; rmSync
    // then races log writes and fails ENOTEMPTY on macOS. Same cleanup as
    // eval/lib/sandbox.ts: ask it to exit, then retry the unlink.
    try {
      execFileSync(BIN, ["--home", home, "shutdown"], { stdio: "ignore", timeout: 5000 });
    } catch {
      // already gone
    }
    rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  });

  it("spawns the daemon on first connect (cold start, §3.1)", async () => {
    writeFileSync(paths.config, JSON.stringify({ maxAgents: 1 }));
    client = new ManagerClient({ home, sessionId: "integ", managerPath: BIN });
    client.onEvent((e) => events.push(e));
    const ok = await client.connect();
    expect(ok, client.lastError() ?? undefined).toBe(true);
    expect(client.isAvailable()).toBe(true);
    expect(existsSync(paths.socket)).toBe(true);
    expect(existsSync(paths.pidFile)).toBe(true);
  }, 15000);

  it("runs a quick task: start -> wait done -> output (§3.3)", async () => {
    const { task_id, pid } = await client.start({
      kind: "shell",
      command: "echo hello-integ",
      cwd: "/tmp",
      env: {},
    });
    expect(task_id.length).toBeGreaterThan(0);
    expect(pid).toBeGreaterThan(0);

    const w = await client.wait(task_id, 5000);
    expect(w.done).toBe(true);
    expect(w.exit_code).toBe(0);

    const out = await client.output(task_id, 0, 65536);
    expect(out.chunk).toContain("hello-integ");
    expect(out.status).toBe("completed"); // §3.4 terminal status, not "exited"
    expect(out.exit_code).toBe(0);
    expect(out.total_size).toBeGreaterThan(0);
  });

  it("pushes task_started / task_exited events to the owning session", async () => {
    const before = events.length;
    const { task_id } = await client.start({
      kind: "shell",
      command: "true",
      cwd: "/tmp",
      env: {},
    });
    await client.wait(task_id, 5000);
    await delay(200); // allow event delivery
    const mine = events.slice(before).filter((e) => e.task_id === task_id);
    expect(mine.some((e) => e.event === "task_started")).toBe(true);
    expect(mine.some((e) => e.event === "task_exited")).toBe(true);
  });

  it("wait budget expires with done:false while the task keeps running", async () => {
    const { task_id } = await client.start({
      kind: "shell",
      command: "sleep 30",
      cwd: "/tmp",
      env: {},
    });
    const w = await client.wait(task_id, 300);
    expect(w.done).toBe(false);

    await client.stop(task_id);
    const w2 = await client.wait(task_id, 5000);
    expect(w2.done).toBe(true);

    const rec = (await client.list()).find((t) => t.task_id === task_id);
    expect(rec).toBeDefined();
    expect(["killed", "completed"]).toContain(rec!.status);
    if (rec!.status === "killed") expect(rec!.exit_code).toBeNull();
  });

  it("streams output events to watchers (watch/unwatch)", async () => {
    const before = events.length;
    const { task_id } = await client.start({
      kind: "monitor",
      command: "printf 'line-a\\n'; sleep 0.3; printf 'line-b\\n'",
      cwd: "/tmp",
      env: {},
    });
    await client.watch(task_id);
    await client.wait(task_id, 5000);
    await delay(300);
    await client.unwatch(task_id);

    const chunks = events
      .slice(before)
      .filter((e) => e.event === "output" && e.task_id === task_id)
      .map((e) => e.chunk ?? "")
      .join("");
    expect(chunks).toContain("line-a");
    expect(chunks).toContain("line-b");
  });

  it("list shows session tasks; CLI status works against the same daemon", async () => {
    const tasks = await client.list();
    expect(tasks.length).toBeGreaterThanOrEqual(3);
    expect(tasks.every((t) => t.session_id === "integ")).toBe(true);

    const sessions = execFileSync(BIN, ["--home", home, "sessions"], {
      encoding: "utf8",
    });
    expect(sessions).toMatch(/integ/);
  });

  it("shutdown_session kills the session's running tasks", async () => {
    const { task_id } = await client.start({
      kind: "shell",
      command: "sleep 30",
      cwd: "/tmp",
      env: {},
    });
    await delay(200);
    const killed = await client.shutdownSession();
    expect(killed).toContain(task_id);

    // §3.3: the response means signals are sent; the status flips when the
    // exit watcher reaps the process (SIGTERM -> 2s grace -> SIGKILL).
    let status = "running";
    const deadline = Date.now() + 6000;
    while (Date.now() < deadline) {
      const rec = (await client.list()).find((t) => t.task_id === task_id);
      status = rec?.status ?? "missing";
      if (status !== "running") break;
      await delay(200);
    }
    expect(status).toBe("killed");
  });

  it("re-registers a held child lease after the real daemon restarts", async () => {
    const childId = "ch_reconnect01";
    const leases = new Set([childId]);
    expect((await client.acquireAgent(childId)).granted).toBe(true);
    let signal!: () => void;
    const reconnected = new Promise<void>((resolve) => { signal = resolve; });
    client.onReconnect(() => {
      void reregisterAgentLeases(client, leases).then(signal);
    });
    const pid = (JSON.parse(readFileSync(paths.pidFile, "utf8")) as { pid: number }).pid;
    process.kill(pid, "SIGKILL");
    let reconnectTimeout: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        reconnected,
        new Promise<void>((_, reject) => {
          reconnectTimeout = setTimeout(
            () => reject(new Error("daemon reconnect did not re-register the child permit")),
            20000,
          );
        }),
      ]);
    } finally {
      if (reconnectTimeout) clearTimeout(reconnectTimeout);
    }

    const competitor = new ManagerClient({ home, sessionId: "competitor", managerPath: BIN });
    expect(await competitor.connect()).toBe(true);
    const denied = await competitor.acquireAgent("ch_competitor");
    expect(denied).toEqual({ granted: false, rejection: "global_capacity" });
    await competitor.close();
    await client.releaseAgent(childId);
    leases.delete(childId);
  }, 25000);

  it("idle reaper: manager exits ~5s after the last connection closes (§3.2)", async () => {
    await client.close();
    const deadline = Date.now() + 12000;
    let gone = false;
    while (Date.now() < deadline) {
      if (!existsSync(paths.socket) && !existsSync(paths.pidFile)) {
        gone = true;
        break;
      }
      await delay(250);
    }
    expect(gone).toBe(true);

    // Log should record the shutdown reason for postmortems.
    if (existsSync(paths.log)) {
      expect(readFileSync(paths.log, "utf8")).toMatch(/shutdown|idle|exit/i);
    }
  }, 20000);

  it("cold restart keeps finished history (records survive a daemon restart)", async () => {
    // Daemon is down now; a fresh client respawns it and sees prior tasks.
    const client2 = new ManagerClient({ home, sessionId: "integ", managerPath: BIN });
    try {
      const ok = await client2.connect();
      expect(ok, client2.lastError() ?? undefined).toBe(true);
      const tasks = await client2.list();
      expect(tasks.length).toBeGreaterThanOrEqual(4); // tasks from earlier its
    } finally {
      await client2.close();
    }
  }, 15000);
});
