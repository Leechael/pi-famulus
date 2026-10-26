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
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { registerIntercom } from "../../src/intercom/intercom";
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
mkdirSync(join(root, "bridge"), { recursive: true });
mkdirSync(join(root, "state"), { recursive: true });
mkdirSync(home, { recursive: true });
const configPath = join(home, "config.json");
const writeConfig = (name = "intercom-integ-host") =>
  writeFileSync(
    configPath,
    JSON.stringify({
      messaging: {
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
    registerIntercom(pi as any, { getClient: () => current });
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
});
