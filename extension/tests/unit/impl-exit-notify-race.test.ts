/**
 * task_exited and a wait reply of done:false can arrive in one socket read.
 * The event is dispatched before bash calls markNotifyOnExit.
 */
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { ExitNotifyGate } from "../../src/exit-notify-gate";
import { famulusPaths } from "../../src/config";
import { ManagerClient, type ManagerEvent } from "../../src/manager-client";

function encodeFrame(message: Record<string, unknown>): Buffer {
  const payload = Buffer.from(JSON.stringify(message), "utf8");
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length, 0);
  return Buffer.concat([header, payload]);
}

describe("wait done:false and task_exited in one read", () => {
  let home: string;
  let server: net.Server;
  let client: ManagerClient;

  afterEach(async () => {
    await client?.close();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    rmSync(home, { recursive: true, force: true });
  });

  it("still delivers the wake when the exit is processed before the mark", async () => {
    home = mkdtempSync(join(tmpdir(), "pi-famulus-exit-race-"));
    const socketPath = famulusPaths(home).socket;
    server = net.createServer((socket) => {
      let buf = Buffer.alloc(0);
      socket.on("data", (data) => {
        buf = Buffer.concat([buf, data]);
        while (buf.length >= 4) {
          const len = buf.readUInt32BE(0);
          if (buf.length < 4 + len) break;
          const msg = JSON.parse(buf.subarray(4, 4 + len).toString("utf8")) as Record<string, unknown>;
          buf = buf.subarray(4 + len);
          if (msg.type === "hello") {
            socket.write(encodeFrame({ v: 1, id: msg.id, ok: true, version: "0.1.0", pid: 1, started_at: 1 }));
          } else if (msg.type === "start") {
            socket.write(encodeFrame({ v: 1, id: msg.id, ok: true, task_id: "sh_race01", pid: 9 }));
          } else if (msg.type === "wait") {
            socket.write(
              Buffer.concat([
                encodeFrame({
                  v: 1,
                  type: "event",
                  event: "task_exited",
                  task_id: "sh_race01",
                  exit_code: 0,
                  signal: null,
                  duration_ms: 12,
                  output_path: "/tmp/sh_race01.output",
                  output_size: 0,
                  ts: 1,
                }),
                encodeFrame({ v: 1, id: msg.id, ok: true, done: false }),
              ]),
            );
          }
        }
      });
    });
    await new Promise<void>((resolve) => server.listen(socketPath, resolve));
    client = new ManagerClient({ home, sessionId: "sess-race", managerPath: null });
    const gate = new ExitNotifyGate<ManagerEvent>();
    const notified: string[] = [];
    client.onEvent((event) => {
      if (event.event !== "task_exited" || !event.task_id) return;
      if (gate.onExit(event.task_id, event, false) === "notify") notified.push(event.task_id);
    });
    expect(await client.connect()).toBe(true);
    await client.start({ kind: "shell", command: "echo hi", cwd: "/tmp", env: {} });
    const wait = await client.wait("sh_race01", 20);
    expect(wait.done).toBe(false);
    expect(notified).toEqual([]);
    const late = gate.mark("sh_race01");
    expect(late?.event).toBe("task_exited");
    expect(late?.exit_code).toBe(0);
  });
});
