/** Startup contract replays: real filesystem/socket/protocol, fake OS process boundary.
 * CI only establishes absent socket/pid before cold restart, not why spawn failed.
 * Contention and delayed startup below are synthetic discriminating inputs.
 */
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ManualClock } from "../../src/clock";

const { spawn } = vi.hoisted(() => ({ spawn: vi.fn() }));
vi.mock("node:child_process", () => ({ spawn }));
import { ManagerClient } from "../../src/manager-client";

function frame(message: Record<string, unknown>): Buffer {
  const data = Buffer.from(JSON.stringify(message));
  const header = Buffer.alloc(4);
  header.writeUInt32BE(data.length);
  return Buffer.concat([header, data]);
}

function processBoundary() {
  return Object.assign(new EventEmitter(), {
    pid: 123456, stdout: new PassThrough(), stderr: new PassThrough(), unref() {},
  });
}

describe.skipIf(process.platform === "win32")("startup contract", () => {
  let home: string;
  let clock: ManualClock;
  let client: ManagerClient;
  let server: net.Server | undefined;
  let sockets: Set<net.Socket>;
  let messages: Record<string, unknown>[];

  beforeEach(() => {
    home = mkdtempSync(join(tmpdir(), "fam-start-"));
    clock = new ManualClock(100_000);
    sockets = new Set();
    messages = [];
    spawn.mockReset();
    client = new ManagerClient({ home, sessionId: "cold-restart", managerPath: "/fixture/pi-famulus", clock });
  });
  afterEach(async () => {
    await client.close();
    expect(clock.pendingTimers).toBe(0);
    for (const socket of sockets) socket.destroy();
    if (server?.listening) await new Promise<void>((resolve) => server!.close(() => resolve()));
    server = undefined;
    rmSync(home, { recursive: true, force: true });
  });

  async function listen(helloError?: string | (() => string | undefined), silent: boolean | (() => boolean) = false, malformed = false, helloCode = "E_BAD_PROTOCOL") {
    writeFileSync(join(home, "manager.pid"), JSON.stringify({ pid: process.pid, version: "0.1.2", started_at: 1 }));
    server = net.createServer((socket) => {
      sockets.add(socket);
      socket.on("error", () => {});
      socket.on("close", () => sockets.delete(socket));
      let buffered = Buffer.alloc(0);
      socket.on("data", (data) => {
        buffered = Buffer.concat([buffered, data]);
        while (buffered.length >= 4 && buffered.length >= 4 + buffered.readUInt32BE(0)) {
          const size = buffered.readUInt32BE(0);
          const msg = JSON.parse(buffered.subarray(4, 4 + size).toString());
          buffered = buffered.subarray(4 + size);
          messages.push(msg);
          if (typeof silent === "function" ? silent() : silent) continue;
          if (malformed) { socket.write(Buffer.from([0, 64, 0, 1])); continue; }
          const error = typeof helloError === "function" ? helloError() : helloError;
          socket.write(frame(msg.type === "hello" && error
            ? { v: 1, id: msg.id, ok: false, error: { code: helloCode, message: error } }
            : { v: 1, id: msg.id, ok: true, tasks: [{ task_id: "sh_history", session_id: "cold-restart", status: "completed" }] }));
        }
      });
    });
    await new Promise<void>((resolve) => server!.listen(join(home, "manager.sock"), resolve));
  }

  async function settle<T>(promise: Promise<T>): Promise<T> {
    let done = false;
    promise.finally(() => { done = true; });
    for (let i = 0; !done && i < 1000; i++) {
      // Yield to the real UDS before moving the injected clock.
      await new Promise((resolve) => setTimeout(resolve, 1));
      if (!done) clock.advanceBy(50);
    }
    expect(done, "startup is bounded").toBe(true);
    return promise;
  }

  it("invariant: lifetime-lock contention remains bounded and never removes manager files", async () => {
    writeFileSync(join(home, "manager.lock"), "held lifetime lock inode");
    spawn.mockImplementation(() => {
      const child = processBoundary();
      queueMicrotask(() => {
        // Deliberately delivers output AFTER exit, as child_process permits.
        child.emit("exit", 0, null);
        child.stdout.write("pi-famulus already running (starting up)\n");
        child.emit("close", 0, null);
      });
      return child;
    });
    expect(await settle(client.connect())).toBe(false);
    expect(clock.now()).toBeLessThanOrEqual(110_050);
    expect(spawn.mock.calls.length).toBeGreaterThan(1);
    expect(spawn.mock.calls.length).toBeLessThan(20);
    expect(client.lastError()).toContain("contention");
    expect(client.lastError()).toContain("code=0");
    expect(client.lastError()).toContain("already running");
    expect(existsSync(join(home, "manager.spawn.lock"))).toBe(false);
    expect(readFileSync(join(home, "manager.lock"), "utf8")).toBe("held lifetime lock inode");
    expect(messages).toEqual([]);
  });

  it("characterization: failed spawn retains exit status and bounded stderr without retry", async () => {
    spawn.mockImplementation(() => {
      const child = processBoundary();
      queueMicrotask(() => {
        child.stderr.write("discard-this-prefix:" + "x".repeat(8192));
        child.stderr.write("cannot write pid file: Permission denied");
        child.emit("exit", 1, null);
        child.emit("close", 1, null);
      });
      return child;
    });
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("code=1");
    const diagnostic = client.lastError();
    const tail = diagnostic.split("signal=null): ")[1].split("; home=")[0];
    expect(tail).toBe("x".repeat(2008) + "cannot write pid file: Permission denied");
    expect(tail).toHaveLength(2048);
    expect(diagnostic).not.toContain("discard-this-prefix:");
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(existsSync(join(home, "manager.spawn.lock"))).toBe(false);
    expect(existsSync(join(home, "manager.pid"))).toBe(false);
    expect(messages).toEqual([]);
  });

  it("regression: startup diagnostics preserve UTF-8 split independently across stdout and stderr", async () => {
    spawn.mockImplementation(() => {
      const child = processBoundary();
      queueMicrotask(() => {
        child.stdout.write(Buffer.from([0xe4]));
        child.stderr.write(Buffer.from([0xf0, 0x9f]));
        child.stdout.write(Buffer.from([0xb8, 0xad]));
        child.stderr.write(Buffer.from([0x98, 0x80]));
        child.emit("close", 1, null);
      });
      return child;
    });
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("中😀");
    expect(client.lastError()).not.toContain("\ufffd");
    expect(spawn).toHaveBeenCalledTimes(1);
  });

  it("regression: the diagnostic tail does not retain half an astral character at its bound", async () => {
    spawn.mockImplementation(() => {
      const child = processBoundary();
      queueMicrotask(() => {
        child.stderr.write("😀" + "x".repeat(2047));
        child.emit("close", 1, null);
      });
      return child;
    });
    expect(await settle(client.connect())).toBe(false);
    const tail = client.lastError().split("signal=null): ")[1].split("; home=")[0];
    expect(tail).toBe("x".repeat(2047));
  });

  it("regression: stdio EOF flushes a genuinely incomplete UTF-8 character", async () => {
    spawn.mockImplementation(() => {
      const child = processBoundary();
      queueMicrotask(() => {
        child.stderr.write(Buffer.from([0xe4, 0xb8, 0xad, 0xe4]));
        child.emit("close", 1, null);
      });
      return child;
    });
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("signal=null): 中\ufffd; home=");
  });

  it("invariant: late socket readiness and silent hello share one startup deadline", async () => {
    spawn.mockImplementation(() => {
      clock.setTimeout(() => { void listen(undefined, true); }, 9000);
      return processBoundary();
    });
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("hello timed out");
    expect(clock.now()).toBeLessThanOrEqual(110_050);
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(messages.filter((message) => message.type === "hello")).toHaveLength(1);
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
  });

  it("invariant: a permanently held spawn lock is bounded and never stolen", async () => {
    writeFileSync(join(home, "manager.spawn.lock"), String(process.pid));
    expect(await settle(client.connect())).toBe(false);
    expect(clock.now()).toBeLessThanOrEqual(110_050);
    expect(client.lastError()).toMatch(/socket.*spawn.lock.*alive/);
    expect(spawn).not.toHaveBeenCalled();
    expect(readFileSync(join(home, "manager.spawn.lock"), "utf8")).toBe(String(process.pid));
    expect(existsSync(join(home, "manager.sock"))).toBe(false);
  });

  it("regression: a held spawn lock cannot extend reconnect beyond 27s or reach the list request timeout", async () => {
    await listen();
    expect(await client.connect()).toBe(true);
    writeFileSync(join(home, "manager.spawn.lock"), String(process.pid));
    for (const socket of sockets) socket.destroy();
    await new Promise<void>((resolve) => server!.close(() => resolve()));
    server = undefined;
    // Observe the actual disconnect; yielding for 5ms is not a readiness signal.
    await vi.waitFor(() => expect(client.isAvailable()).toBe(false), { timeout: 2000, interval: 1 });
    const began = clock.now();
    let finishedAt = 0;
    const result = client.list().then(
      () => "unexpected success",
      (err: Error) => { finishedAt = clock.now(); return err.message; },
    );
    try {
      for (let i = 0; !finishedAt && i < 620; i++) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (!finishedAt) clock.advanceBy(50);
      }
      expect(await result).toBe("pi-famulus reconnect exhausted");
      expect(finishedAt - began).toBeLessThanOrEqual(27_050);
      expect(finishedAt - began).toBeLessThan(30_000);
      expect(client.lastError()).toBe("reconnect exhausted");
      expect(client.isAvailable()).toBe(false);
      expect(spawn).not.toHaveBeenCalled();
      expect(readFileSync(join(home, "manager.spawn.lock"), "utf8")).toBe(String(process.pid));
    } finally {
      // Also drain the old, over-budget implementation when this regression is RED.
      await settle(client.ensureAvailable());
    }
  });

  it("invariant: a silent live manager fails hello without retry, spawn, or file cleanup", async () => {
    await listen(undefined, true);
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("hello timed out");
    expect(messages.filter((message) => message.type === "hello")).toHaveLength(1);
    expect(spawn).not.toHaveBeenCalled();
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
  });

  it("invariant: malformed hello frame preserves its protocol failure without retry or cleanup", async () => {
    await listen(undefined, false, true);
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("E_PROTOCOL");
    expect(client.lastError()).toContain("frame too large");
    expect(messages.filter((message) => message.type === "hello")).toHaveLength(1);
    expect(spawn).not.toHaveBeenCalled();
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
  });

  it("regression: a known shutdown lasting seven seconds recovers within one ten-second budget despite a stale live pid", async () => {
    await listen(() => clock.now() < 107_000 ? "manager is shutting down" : undefined, false, false, "E_INTERNAL");
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect()), client.lastError()).toBe(true);
    expect(clock.now()).toBeGreaterThanOrEqual(107_000);
    expect(clock.now()).toBeLessThan(110_000);
    expect(client.isAvailable()).toBe(true);
    expect(await client.list()).toEqual([{ task_id: "sh_history", session_id: "cold-restart", status: "completed" }]);
    expect(spawn).not.toHaveBeenCalled();
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
    expect(messages.filter((msg) => msg.type === "hello").length).toBeGreaterThan(2);
  });

  it("regression: a stalled probe after known shutdown yields within two seconds to a healthy successor", async () => {
    const helloCount = () => messages.filter((msg) => msg.type === "hello").length;
    await listen(() => helloCount() === 1 ? "manager is shutting down" : undefined,
      () => helloCount() === 2, false, "E_INTERNAL");
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect()), client.lastError()).toBe(true);
    expect(clock.now()).toBeLessThan(103_000);
    expect(helloCount()).toBe(3);
    expect(await client.list()).toEqual([{ task_id: "sh_history", session_id: "cold-restart", status: "completed" }]);
    expect(helloCount()).toBe(3); // Keep the successful probe's actual connection.
    expect(spawn).not.toHaveBeenCalled();
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
  });

  it("invariant: a permanent protocol rejection after known shutdown remains fatal", async () => {
    const helloCount = () => messages.filter((msg) => msg.type === "hello").length;
    await listen(() => helloCount() === 1 ? "manager is shutting down" : "unsupported protocol", false, false, "E_INTERNAL");
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("E_INTERNAL: unsupported protocol");
    expect(helloCount()).toBe(2);
    expect(clock.now()).toBeLessThan(101_000);
    expect(spawn).not.toHaveBeenCalled();
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
  });

  it.each([
    ["E_VERSION", "manager is shutting down"],
    ["E_INTERNAL", "unrelated failure: manager is shutting down"],
  ])("invariant: %s with shutdown-like text is fatal without retry", async (code, message) => {
    await listen(message, false, false, code);
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain(`${code}: ${message}`);
    expect(messages.filter((msg) => msg.type === "hello")).toHaveLength(1);
    expect(clock.now()).toBeLessThan(101_000);
    expect(spawn).not.toHaveBeenCalled();
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
  });

  it("invariant: permanent hello rejection does not retry, spawn, or unlink a live manager", async () => {
    await listen("unsupported extension protocol");
    const pidFile = readFileSync(join(home, "manager.pid"), "utf8");
    expect(await settle(client.connect())).toBe(false);
    expect(client.lastError()).toContain("E_BAD_PROTOCOL");
    expect(messages.filter((message) => message.type === "hello")).toHaveLength(1);
    expect(spawn).not.toHaveBeenCalled();
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(readFileSync(join(home, "manager.pid"), "utf8")).toBe(pidFile);
    expect(client.isAvailable()).toBe(false);
  });

  it("regression: closing during socket readiness cannot reopen the client when the daemon becomes ready", async () => {
    spawn.mockImplementation(() => processBoundary());
    const connecting = client.connect();
    await vi.waitFor(() => expect(spawn).toHaveBeenCalledTimes(1), { timeout: 2000, interval: 1 });
    await client.close();
    // Fixture cleanup can finish while the detached daemon is still starting.
    await listen();
    expect(await settle(connecting)).toBe(false);
    expect(client.isAvailable()).toBe(false);
    expect(messages.filter((message) => message.type === "hello")).toEqual([]);
    expect(spawn).toHaveBeenCalledTimes(1);
  });

  it("target behavior: waits for one live delayed startup beyond the old socket-ready budget", async () => {
    let ready: Promise<void> | undefined;
    spawn.mockImplementation(() => {
      clock.setTimeout(() => { ready = listen(); }, 2400);
      return processBoundary();
    });
    expect(await settle(client.connect()), client.lastError()).toBe(true);
    await ready;
    expect(spawn).toHaveBeenCalledTimes(1);
    expect(await client.list()).toEqual([{ task_id: "sh_history", session_id: "cold-restart", status: "completed" }]);
  });

  it("regression: respawns after a contender exits against the old lifetime lock, preserving history", async () => {
    // Replays idle-reaper's observable files-missing state. Old manager.lock
    // remains held at the OS boundary: Rust daemon reports already-running.
    expect(existsSync(join(home, "manager.sock"))).toBe(false);
    expect(existsSync(join(home, "manager.pid"))).toBe(false);
    writeFileSync(join(home, "manager.lock"), "old lifetime lock inode");
    let winners = 0;
    spawn.mockImplementation(() => {
      const child = processBoundary();
      if (spawn.mock.calls.length === 1) {
        queueMicrotask(() => {
          child.stdout.write("pi-famulus already running (starting up)\n");
          child.emit("exit", 0, null);
          child.emit("close", 0, null);
        });
      } else {
        winners++;
        void listen();
      }
      return child;
    });
    expect(await settle(client.connect()), client.lastError()).toBe(true);
    expect(client.isAvailable()).toBe(true);
    expect(await client.list()).toEqual([{ task_id: "sh_history", session_id: "cold-restart", status: "completed" }]);
    expect(winners).toBe(1);
    expect(spawn).toHaveBeenCalledTimes(2);
    expect(readFileSync(join(home, "manager.lock"), "utf8")).toBe("old lifetime lock inode");
    expect(messages.filter((message) => message.type === "hello")).toHaveLength(1);
  });
});
