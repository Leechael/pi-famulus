/**
 * End-to-end integration: the extension's real intercom module
 * (registerIntercom + createManagerTransport over real ManagerClients)
 * against the real pi-famulus binary: registration, deliver envelopes, read
 * receipts, bridge-restart reset, daemon reconnect, and a session switch
 * with dropped receipts.
 *
 * Opt-in like the other real-binary integration tests:
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/intercom-real-manager.test.ts
 */
import { connect as connectStream } from "node:net";
import { createServer } from "node:http";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it, vi } from "vitest";
import { registerIntercom } from "../../src/intercom/intercom";
import { collectFamulusInfo, exchangeMessagingToken, formatFamulusInfo } from "../../src/tui/famulus-command";
import { ManagerClient, type ManagerEvent } from "../../src/manager-client";

const RUN = process.env.PI_FAMULUS_INTEG === "1";
const BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/debug", process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus");
if (RUN && !existsSync(BIN)) {
  throw new Error(`Requested pi-famulus integration binary does not exist: ${BIN}`);
}

const root = mkdtempSync(join(tmpdir(), "pi-famulus-intercom-integ-"));
const home = join(root, "manager");
const bridgeSock = join(root, "bridge", "client.sock");
mkdirSync(join(root, "bridge"), { recursive: true, mode: 0o700 });
mkdirSync(join(root, "state"), { recursive: true, mode: 0o700 });
mkdirSync(home, { recursive: true, mode: 0o700 });
const configPath = join(home, "config.json");
const writeConfig = (name = "intercom-integ-host") =>
  writeFileSync(
    configPath,
    JSON.stringify({
      messaging: {
        enabled: true,
        localOnly: true,
        name,
        sock: bridgeSock,
        stateDir: join(root, "state"),
        mcpListen: "",
      },
    }),
  );
writeConfig();

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitFor(predicate: () => boolean | Promise<boolean>, what: string, budgetMs = 30_000): Promise<void> {
  const deadline = Date.now() + budgetMs;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await delay(50);
  }
}

function makeCtx(sessionId: string, injections: { message: any; options?: any }[], notices: string[]) {
  const state = { idle: true, pending: false };
  return {
    state,
    ctx: {
      cwd: root,
      hasUI: false,
      mode: "print",
      sessionManager: { getSessionId: () => sessionId, getEntries: () => [] },
      isIdle: () => state.idle,
      hasPendingMessages: () => state.pending,
      ui: { notify: (message: string) => notices.push(message) },
    },
    sendMessage: (message: any, options?: any) => injections.push({ message, options }),
  };
}

describe.skipIf(!RUN)("menu token exchange over the real pi-famulus binary", () => {
  it.each([
    { name: undefined, customStateDir: false },
    { name: "configured-menu-device", customStateDir: true },
  ])("honors current config and home, persists private credentials, and never enables messaging: %j", { timeout: 30_000 }, async ({ name, customStateDir }) => {
    const testRoot = mkdtempSync(join(tmpdir(), "pi-famulus-menu-enroll-"));
    const managerHome = join(testRoot, "current-home");
    const stateDir = customStateDir ? join(testRoot, "configured-state") : join(managerHome, "messaging");
    mkdirSync(managerHome, { mode: 0o700 });
    mkdirSync(stateDir, { recursive: true, mode: 0o700 });
    const requests: { method: string; url: string; body: Record<string, unknown> }[] = [];
    let rejectEnrollment = true;
    const relay = createServer(async (request, response) => {
      const chunks: Buffer[] = [];
      for await (const chunk of request) chunks.push(Buffer.from(chunk));
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      requests.push({ method: request.method ?? "", url: request.url ?? "", body });
      if (request.method !== "POST" || request.url !== "/v1/enroll") {
        response.writeHead(404).end("unexpected enrollment endpoint");
      } else if (rejectEnrollment) {
        response.writeHead(401).end("fixture enrollment refused");
      } else {
        response.writeHead(201, { "Content-Type": "application/json" }).end(JSON.stringify({
          device_id: body.device_id,
          tenant_id: "fixture-tenant",
          credential: "private-enrolled-device-credential",
        }));
      }
    });
    await new Promise<void>((resolve) => relay.listen(0, "127.0.0.1", resolve));
    const address = relay.address() as { port: number };
    const server = `http://127.0.0.1:${address.port}`;
    const currentConfigPath = join(managerHome, "config.json");
    const messaging = {
      ...(customStateDir ? { enabled: false, stateDir } : {}),
      ...(name ? { name } : {}),
      server,
      token: "legacy-config-token",
      remoteMessages: "refuse",
    };
    const config = { logLevel: "debug", foregroundBudgetMs: 12345, messaging };
    writeFileSync(currentConfigPath, `${JSON.stringify(config)}\n`);
    const devicePath = join(stateDir, "device.json");
    const originalIdentity = {
      device_id: "6da615df-53da-4d9b-9d02-44ad67b85135",
      tenant_id: "existing-tenant",
      credential: "existing-private-device-credential",
      server,
    };
    writeFileSync(devicePath, `${JSON.stringify(originalIdentity)}\n`, { mode: 0o600 });
    const configBefore = readFileSync(currentConfigPath, "utf8");
    const identityBefore = readFileSync(devicePath, "utf8");
    const token = "fixture-one-time-enrollment-token";
    const stdout = vi.spyOn(process.stdout, "write").mockImplementation(() => true);
    const stderr = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    try {
      await expect(exchangeMessagingToken(BIN, currentConfigPath, token)).rejects.toThrow("fixture enrollment refused");
      expect(readFileSync(currentConfigPath, "utf8")).toBe(configBefore);
      expect(readFileSync(devicePath, "utf8")).toBe(identityBefore);
      rejectEnrollment = false;
      await expect(exchangeMessagingToken(BIN, currentConfigPath, token)).resolves.toBeUndefined();
      expect(requests).toHaveLength(2);
      for (const request of requests) {
        expect(request).toMatchObject({
          method: "POST", url: "/v1/enroll",
          body: { token, device_id: originalIdentity.device_id, name: name ?? hostname() },
        });
        expect(request.body.client_version).toEqual(expect.any(String));
      }
      expect(JSON.parse(readFileSync(devicePath, "utf8"))).toMatchObject({
        device_id: originalIdentity.device_id, tenant_id: "fixture-tenant",
        credential: "private-enrolled-device-credential", server,
      });
      if (process.platform !== "win32") {
        expect(statSync(devicePath).mode & 0o777).toBe(0o600);
        expect(statSync(stateDir).mode & 0o777).toBe(0o700);
      }
      expect(readFileSync(currentConfigPath, "utf8")).toBe(configBefore);
      expect(readFileSync(currentConfigPath, "utf8")).not.toContain(token);
      expect(readFileSync(currentConfigPath, "utf8")).not.toContain("private-enrolled-device-credential");
      const output = [...stdout.mock.calls, ...stderr.mock.calls].map((call) => String(call[0])).join("\n");
      for (const secret of [token, originalIdentity.credential, "private-enrolled-device-credential"]) {
        expect(output).not.toContain(secret);
      }
      if (customStateDir) expect(existsSync(join(managerHome, "messaging", "device.json"))).toBe(false);
      expect(existsSync(join(managerHome, "manager.pid"))).toBe(false);
    } finally {
      stdout.mockRestore();
      stderr.mockRestore();
      await new Promise<void>((resolve) => relay.close(() => resolve()));
      rmSync(testRoot, { recursive: true, force: true });
    }
  });
});

describe.skipIf(!RUN)("intercom module over the real pi-famulus binary", () => {
  const injections: { message: any; options?: any }[] = [];
  const notices: string[] = [];
  const bobFrames: ManagerEvent[] = [];
  let current: ManagerClient | null = null;
  let alice: ManagerClient;
  let bob: ManagerClient;
  const handlers = new Map<string, (event: any, ctx: any) => any>();
  let sessionName = "alice-session";
  const pi = {
    on: (name: string, handler: any) => handlers.set(name, handler),
    registerTool: () => {},
    registerCommand: () => {},
    registerMessageRenderer: () => {},
    registerEntryRenderer: () => {},
    getSessionName: () => sessionName,
    setSessionName: (name: string) => {
      sessionName = name;
    },
    sendMessage: (message: any, options?: any) => injections.push({ message, options }),
  };
  let ctxAlice = makeCtx("alice-session", injections, notices);

  const emit = (name: string, event: any = {}, ctx: any = ctxAlice.ctx) => handlers.get(name)?.(event, ctx);

  afterAll(async () => {
    await emit("session_shutdown");
    await alice?.close();
    await bob?.close();
    try {
      const { execFileSync } = await import("node:child_process");
      execFileSync(BIN, ["--home", home, "shutdown"], { stdio: "ignore", timeout: 10_000 });
    } catch { /* already stopped */ }
    rmSync(root, { recursive: true, force: true });
  });

  it("registers, delivers, reports receipts, resets, reconnects, and switches sessions", { timeout: 180_000 }, async () => {
    alice = new ManagerClient({ home, sessionId: "alice-session", managerPath: BIN });
    current = alice;
    registerIntercom(pi as any, { enabled: true, getClient: () => current });
    await alice.connect();
    bob = new ManagerClient({ home, sessionId: "bob-session", managerPath: BIN });
    await bob.connect();
    // Bob needs his own port: receipts forward to the origin's session.
    await bob.intercom({
      op: "register",
      session: { id: "bob-session", name: "bob", caps: ["read_receipts", "idle_state_v1"] },
    });
    bob.onEvent((event) => {
      if (event.event === "intercom" || event.event === "intercom_reset") bobFrames.push(event);
    });

    // register: the module registers on session_start.
    await emit("session_start", {});
    await waitFor(async () => {
      const rows = (await bob.intercom({ op: "list" })).sessions as any[];
      return rows.some((row) => row.id === "pi/alice-session");
    }, "alice's registration");
    expect(notices.some((m) => /intercom active/.test(m))).toBe(true);

    // deliver: bob messages alice; the module injects the envelope.
    await bob.intercom({ op: "send", to: "pi/alice-session", body: "hello alice", reply_to: "pi/bob-session" });
    await waitFor(() => injections.some((i) => i.message.customType === "pi-famulus-cross-session"), "the deliver envelope");
    const envelope = injections.find((i) => i.message.customType === "pi-famulus-cross-session")!;
    // from equals from-session here, so the envelope carries one attribute.
    expect(envelope.message.content).toContain('<cross-session-message from="pi/bob-session">');
    expect(envelope.message.content).toContain("hello alice");

    // receipt: message_end for the injected message reports read; bob gets
    // the receipt frame.
    await emit("message_end", { message: { role: "custom", ...envelope.message } });
    await waitFor(
      () => bobFrames.some((frame) => (frame.frame as any)?.type === "receipt" && (frame.frame as any)?.status === "read"),
      "the read receipt frame on bob",
    );

    // reset: a bridge restart (config change on promotion) pushes
    // intercom_reset; the module re-registers on its own.
    writeConfig("intercom-integ-host-rotated");
    // ManagerClient has no supervise(); speak the two frames on a raw
    // socket (length-prefixed, not newline-delimited).
    await new Promise<void>((resolve, reject) => {
      const raw = connectStream(join(home, "manager.sock"));
      const send = (obj: Record<string, unknown>) => {
        const body = Buffer.from(JSON.stringify({ v: 1, ...obj }), "utf8");
        const header = Buffer.alloc(4);
        header.writeUInt32BE(body.length, 0);
        raw.write(Buffer.concat([header, body]));
      };
      let buffer = Buffer.alloc(0);
      let answered = 0;
      raw.on("data", (chunk) => {
        buffer = Buffer.concat([buffer, chunk]);
        while (buffer.length >= 4) {
          const len = buffer.readUInt32BE(0);
          if (buffer.length < 4 + len) break;
          buffer = buffer.subarray(4 + len);
          answered += 1;
          if (answered === 1) send({ id: "s1", type: "supervise" });
          if (answered === 2) resolve();
        }
      });
      raw.on("error", reject);
      raw.on("close", () => reject(new Error("raw cli connection closed early")));
      send({ id: "h", type: "hello", client_kind: "cli" });
      setTimeout(() => reject(new Error("supervise timed out")), 15_000).unref?.();
    });

    // The module's next activity re-registers on the new bridge.
    await emit("turn_start");
    await waitFor(async () => {
      const rows = (await bob.intercom({ op: "list" })).sessions as any[];
      return rows.some((row) => row.id === "pi/alice-session" && row.name === "alice-session");
    }, "re-registration after the reset");

    // reconnect: kill the daemon; the ManagerClient respawns it and the
    // module re-registers through onReconnect.
    const { execFileSync } = await import("node:child_process");
    const pidInfo = JSON.parse(existsSync(join(home, "manager.pid")) ? (await import("node:fs")).readFileSync(join(home, "manager.pid"), "utf8") : "{}");
    if (pidInfo.pid) {
      try { process.kill(pidInfo.pid, "SIGKILL"); } catch { /* gone */ }
    }
    await waitFor(async () => {
      try {
        const rows = (await bob.intercom({ op: "list" })).sessions as any[];
        return rows.some((row) => row.id === "pi/alice-session");
      } catch {
        return false; // the daemon is mid-respawn
      }
    }, "re-registration after the daemon respawn", 60_000);
    // Bob's port died with the old daemon too.
    await bob.intercom({
      op: "register",
      session: { id: "bob-session", name: "bob", caps: ["read_receipts", "idle_state_v1"] },
    });

    // session switch: a new ManagerClient (new connection, new session);
    // an unread message of the old session is reported dropped to bob.
    injections.length = 0;
    await bob.intercom({ op: "send", to: "pi/alice-session", body: "unread", reply_to: "pi/bob-session" });
    await waitFor(() => injections.length > 0, "the unread deliver");
    const alice2 = new ManagerClient({ home, sessionId: "next-session", managerPath: BIN });
    current = alice2;
    await alice2.connect();
    const ctxNext = makeCtx("next-session", injections, notices);
    pi.setSessionName("next-session");
    await emit("session_start", { reason: "new" }, ctxNext.ctx);
    await waitFor(
      () => bobFrames.some((frame) => (frame.frame as any)?.type === "receipt" && (frame.frame as any)?.message_id && (frame.frame as any)?.status === "dropped"),
      "the dropped receipt for the old session's unread message",
    );
    await waitFor(async () => {
      const rows = (await bob.intercom({ op: "list" })).sessions as any[];
      return rows.some((row) => row.id === "pi/next-session");
    }, "the new session's registration");
    await alice2.close();
  });

  it("shows a real relay HTTP 401 body while the local bridge remains healthy", { timeout: 30_000 }, async () => {
    const body = "fixture relay requires a device credential";
    const relay = createServer((_request, response) => {
      response.writeHead(401, { "Content-Length": Buffer.byteLength(body) });
      response.end(body);
    });
    relay.on("upgrade", (_request, socket) => {
      socket.end(`HTTP/1.1 401 Unauthorized\r\nContent-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`);
    });
    await new Promise<void>((resolve) => relay.listen(0, "127.0.0.1", resolve));
    const address = relay.address() as { port: number };
    const relayHome = join(root, "relay-manager");
    mkdirSync(relayHome, { mode: 0o700 });
    writeFileSync(join(relayHome, "config.json"), JSON.stringify({ messaging: {
      enabled: true,
      localOnly: false,
      server: `http://127.0.0.1:${address.port}`,
      token: "invalid-device-credential",
      sock: join(relayHome, "client.sock"),
      stateDir: join(relayHome, "state"),
    } }));
    const manager = new ManagerClient({ home: relayHome, sessionId: "relay-error-session", managerPath: BIN });
    const handlers = new Map<string, (event: any, ctx: any) => any>();
    const notices: string[] = [];
    const { ctx } = makeCtx("relay-error-session", [], notices);
    registerIntercom({ ...pi, on: (name: string, handler: any) => handlers.set(name, handler) } as any, {
      enabled: true, getClient: () => manager,
    });
    try {
      expect(await manager.connect()).toBe(true);
      await handlers.get("session_start")!({}, ctx);
      await waitFor(() => notices.some((message) => message.includes("401") && message.includes(body)), "the relay rejection notification");
      expect(manager.isAvailable()).toBe(true);
      const local = await manager.intercom({ op: "list" });
      expect((local.sessions as any[]).some((session) => session.id === "pi/relay-error-session")).toBe(true);
      const info = await collectFamulusInfo({
        extensionVersion: "test", configPath: join(relayHome, "config.json"),
        managerPath: null, describeSearch: () => "", getClient: () => manager, messagingEnabled: true,
      });
      expect(info.messaging?.state).toBe("disconnected");
      expect(formatFamulusInfo(info)).toContain(body);
      expect(formatFamulusInfo(info)).toContain("401");
      await delay(3100);
      expect(notices.filter((message) => message.includes("401") && message.includes(body))).toHaveLength(1);
    } finally {
      await handlers.get("session_shutdown")!({}, ctx);
      await manager.close();
      spawnSync(BIN, ["--home", relayHome, "shutdown"], { stdio: "ignore", timeout: 10_000 });
      await new Promise<void>((resolve) => relay.close(() => resolve()));
    }
  });
});
