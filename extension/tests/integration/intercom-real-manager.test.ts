/**
 * End-to-end integration: the extension's intercom port <-> real pi-famulus
 * binary (two ManagerClients speaking protocol 7 `intercom` over manager.sock,
 * plus a legacy client.sock JSONL peer).
 *
 * Opt-in (spawns real processes):
 *   PI_FAMULUS_INTEG=1 npx vitest run tests/integration/intercom-real-manager.test.ts
 *
 * Uses the binary at ../../manager/target/debug/pi-famulus unless
 * PI_FAMULUS_MANAGER_PATH overrides it. The messaging bridge binds an
 * isolated client.sock configured in the temp home's config.json.
 */
import { spawnSync } from "node:child_process";
import { connect as connectStream } from "node:net";
import { createInterface } from "node:readline";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { ManagerClient, type ManagerEvent } from "../../src/manager-client";

const RUN = process.env.PI_FAMULUS_INTEG === "1";
const BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/debug", process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus");
if (RUN && !existsSync(BIN)) {
  throw new Error(`Requested pi-famulus integration binary does not exist: ${BIN}`);
}
if (RUN && process.platform === "win32") {
  // The intercom bridge is Linux/macOS only; the daemon reports the
  // structured disabled/unavailable error there instead of serving ops.
}

const root = mkdtempSync(join(tmpdir(), "pi-famulus-intercom-integ-"));
const home = join(root, "manager");
const bridgeSock = join(root, "bridge", "client.sock");
mkdirSync(join(root, "bridge"), { recursive: true, mode: 0o700 });
mkdirSync(join(root, "state"), { recursive: true, mode: 0o700 });
mkdirSync(home, { recursive: true, mode: 0o700 });
writeFileSync(
  join(home, "config.json"),
  JSON.stringify({
    messaging: {
      enabled: true,
      localOnly: true,
      name: "intercom-integ-host",
      sock: bridgeSock,
      stateDir: join(root, "state"),
      mcpListen: "",
    },
  }),
);

function delay(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitFor(predicate: () => boolean, what: string, budgetMs = 20_000): Promise<void> {
  const deadline = Date.now() + budgetMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await delay(50);
  }
}

describe.skipIf(!RUN)("real pi-famulus intercom integration", () => {
  let alice: ManagerClient;
  let bob: ManagerClient;
  const aliceEvents: ManagerEvent[] = [];
  const bobEvents: ManagerEvent[] = [];

  afterAll(async () => {
    await alice?.close();
    await bob?.close();
    // Stop the daemon (a best-effort CLI shutdown; the temp home is removed anyway).
    spawnSync(BIN, ["--home", home, "shutdown"], { stdio: "ignore", timeout: 10_000 });
    rmSync(root, { recursive: true, force: true });
  });

  it("serves register/send/deliver/receipt over manager.sock", { timeout: 60_000 }, async () => {
    alice = new ManagerClient({ home, sessionId: "alice-session", managerPath: BIN });
    bob = new ManagerClient({ home, sessionId: "bob-session", managerPath: BIN });
    expect(await alice.connect()).toBe(true);
    expect(await bob.connect()).toBe(true);
    expect(alice.protocolLevel()).toBeGreaterThanOrEqual(7);
    alice.onEvent((event) => aliceEvents.push(event));
    bob.onEvent((event) => bobEvents.push(event));

    const aliceRegistered = await alice.intercom({
      op: "register",
      session: { id: "alice-session", name: "alice", caps: ["read_receipts", "idle_state_v1"] },
    });
    expect(aliceRegistered.session_id).toBe("pi/alice-session");
    const bobRegistered = await bob.intercom({
      op: "register",
      session: { id: "bob-session", name: "bob", caps: ["read_receipts", "idle_state_v1"] },
    });
    expect(bobRegistered.session_id).toBe("pi/bob-session");

    const sent = await alice.intercom({
      op: "send",
      to: "pi/bob-session",
      body: "hello over manager.sock",
      reply_to: "pi/alice-session",
    });
    expect(sent.status).toBe("delivered");

    await waitFor(
      () => bobEvents.some((event) => event.event === "intercom" && (event.frame?.type as string) === "deliver"),
      "bob's deliver frame",
    );
    const deliver = bobEvents.find((event) => event.event === "intercom" && (event.frame?.type as string) === "deliver")!;
    expect(deliver.frame?.body).toBe("hello over manager.sock");
    expect(deliver.frame?.from).toBe("pi/alice-session");
    expect(deliver.frame?.message_id).toBe(sent.message_id);

    await bob.intercom({ op: "receipt", message_id: sent.message_id, status: "read" });
    await waitFor(
      () => aliceEvents.some((event) => event.event === "intercom" && (event.frame?.type as string) === "receipt"),
      "alice's receipt frame",
    );
    const receipt = aliceEvents.find((event) => event.event === "intercom" && (event.frame?.type as string) === "receipt")!;
    expect(receipt.frame?.status).toBe("read");
  });

  it("exchanges with a legacy client.sock peer and refuses identity mismatches", { timeout: 60_000 }, async () => {
    // Legacy JSONL peer on the bridge's client.sock.
    await waitFor(() => existsSync(bridgeSock), "the bridge client.sock");
    const socket = connectStream(bridgeSock);
    const rl = createInterface({ input: socket });
    const pushed: Record<string, unknown>[] = [];
    let seq = 0;
    const pending = new Map<string, (frame: Record<string, unknown>) => void>();
    rl.on("line", (line) => {
      const frame = JSON.parse(line) as Record<string, unknown>;
      if (frame.id && pending.has(frame.id as string)) {
        pending.get(frame.id as string)!(frame);
        pending.delete(frame.id as string);
      } else if (frame.type) {
        pushed.push(frame);
      }
    });
    const call = (request: Record<string, unknown>) =>
      new Promise<Record<string, unknown>>((resolve, reject) => {
        seq += 1;
        const id = `legacy-${seq}`;
        pending.set(id, resolve);
        socket.write(JSON.stringify({ id, ...request }) + "\n");
        setTimeout(() => {
          if (pending.delete(id)) reject(new Error("legacy peer request timed out"));
        }, 10_000).unref?.();
      });
    const registered = await call({
      op: "register",
      session: { id: "legacy-peer", name: "legacy", caps: ["read_receipts", "idle_state_v1"] },
    });
    expect(registered.ok).toBe(true);

    // alice messages the legacy peer; it answers with a receipt.
    const sent = await alice.intercom({ op: "send", to: "pi/legacy-peer", body: "cross-transport" });
    await waitFor(() => pushed.some((frame) => frame.type === "deliver"), "legacy deliver frame");
    const deliver = pushed.find((frame) => frame.type === "deliver")!;
    expect(deliver.message_id).toBe(sent.message_id);
    await call({ op: "receipt", message_id: sent.message_id, status: "read" });
    await waitFor(
      () => aliceEvents.some((event) => event.event === "intercom" && (event.frame as Record<string, unknown> | undefined)?.type === "receipt"),
      "alice's receipt from the legacy peer",
    );

    // A manager connection cannot register as another session.
    const mismatch = await alice.intercom({
      op: "register",
      session: { id: "not-alice", name: "impostor" },
    }).then(
      () => undefined,
      (error) => error,
    );
    expect(mismatch).toBeDefined();
    expect(String(mismatch.message ?? mismatch)).toMatch(/session id mismatch/);

    // A legacy register cannot claim the manager port's session.
    const theft = await call({
      op: "register",
      session: { id: "alice-session", name: "thief" },
    });
    expect(theft.ok).not.toBe(true);
    expect(theft.error).toMatch(/served by the pi-famulus extension/);
    socket.end();
  });
});
