/**
 * First parent for the offline-parent-result integration test. Speaks just enough of the
 * daemon protocol to start a hosted child and a background shell, then
 * waits until it is killed. The test SIGKILLs this process so the
 * connection drops without shutdown_session.
 */
import { spawn } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import net from "node:net";
import { join } from "node:path";

const home = process.env.PI_FAMULUS_HOME;
const bin = process.env.PI_FAMULUS_MANAGER_PATH;
const sessionId = process.env.PI_FAMULUS_HOLD_SESSION;
const cwd = process.env.PI_FAMULUS_HOLD_CWD;
const readyPath = process.env.PI_FAMULUS_HOLD_READY;
const command = process.env.PI_FAMULUS_HOLD_COMMAND;
const childArgv = JSON.parse(process.env.PI_FAMULUS_HOLD_CHILD_ARGV ?? "[]");
const tracePath = process.env.PI_FAMULUS_TEST_FAUX_TRACE;
const logPath = process.env.PI_FAMULUS_HOLD_LOG;
if (!home || !bin || !sessionId || !cwd || !readyPath || !command) {
  throw new Error("hold-session is missing required environment");
}

function log(message) {
  if (logPath) writeFileSync(logPath, `${message}\n`, { flag: "a" });
}

process.on("unhandledRejection", (error) => {
  log(error instanceof Error ? (error.stack ?? error.message) : String(error));
  process.exit(1);
});

function encodeFrame(message) {
  const payload = Buffer.from(JSON.stringify(message), "utf8");
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length, 0);
  return Buffer.concat([header, payload]);
}

class Decoder {
  constructor() {
    this.buf = Buffer.alloc(0);
  }
  push(data) {
    this.buf = this.buf.length === 0 ? data : Buffer.concat([this.buf, data]);
    const messages = [];
    while (this.buf.length >= 4) {
      const len = this.buf.readUInt32BE(0);
      if (this.buf.length < 4 + len) break;
      messages.push(JSON.parse(this.buf.subarray(4, 4 + len).toString("utf8")));
      this.buf = this.buf.subarray(4 + len);
    }
    return messages;
  }
}

function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const tick = () => {
      if (predicate()) {
        resolve();
        return;
      }
      if (Date.now() > deadline) {
        reject(new Error(`timed out waiting for ${label}`));
        return;
      }
      setTimeout(tick, 50);
    };
    tick();
  });
}

const socketPath = join(home, "manager.sock");
const daemon = spawn(bin, ["--home", home, "daemon"], {
  detached: true,
  stdio: "ignore",
  env: process.env,
});
daemon.unref();

log("waiting for socket");
await waitFor(() => existsSync(socketPath), 15_000, "the daemon socket");
log("socket ready");

const socket = await new Promise((resolve, reject) => {
  const conn = net.connect(socketPath);
  conn.once("connect", () => resolve(conn));
  conn.once("error", reject);
});

const decoder = new Decoder();
const pending = new Map();
socket.on("data", (data) => {
  for (const message of decoder.push(data)) {
    const waiter = typeof message.id === "string" ? pending.get(message.id) : undefined;
    if (waiter) {
      pending.delete(message.id);
      if (message.ok === false) waiter.reject(new Error(message.error ?? JSON.stringify(message)));
      else waiter.resolve(message);
    }
  }
});

function request(body) {
  const id = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    socket.write(encodeFrame({ v: 1, id, ...body }));
  });
}

log("hello");
await request({
  type: "hello",
  client_kind: "extension",
  session_id: sessionId,
  pi_pid: process.pid,
  cwd,
  extension_version: "0.2.0",
  protocol: 6,
  role: "parent",
});

const childId = "ch_keep";
log("agent_spawn");
await request({
  type: "agent_spawn",
  child_id: childId,
  run_id: "run_keep",
  name: "keeper",
  argv: childArgv,
  cwd,
  env: Object.fromEntries(Object.entries(process.env).filter((entry) => typeof entry[1] === "string")),
});

log("start shell");
const started = await request({
  type: "start",
  kind: "shell",
  command,
  cwd,
  env: Object.fromEntries(Object.entries(process.env).filter((entry) => typeof entry[1] === "string")),
  run_in_background: true,
  origin: { via: "bash-bg" },
});

log("prompt child");
await request({
  type: "agent_input",
  child_id: childId,
  op: "prompt",
  text: "work",
  source: "parent",
});
await request({ type: "agent_watch", child_id: childId });

if (tracePath) {
  log("waiting for child model call");
  await waitFor(() => existsSync(tracePath) && readFileSync(tracePath, "utf8").trim().length > 0, 30_000, "the hosted child to enter a model call");
  log("child is inside a model call");
}

writeFileSync(
  readyPath,
  JSON.stringify({ task_id: started.task_id, child_id: childId, pid: process.pid }),
);
setInterval(() => {}, 60_000);
