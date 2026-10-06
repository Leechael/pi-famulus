/** Regression for a known startup boundary, NOT attribution of the historical CI flake.
 * Real Python flock holder + real Rust daemon + public TS client, no spawn mocks.
 * Run alongside real-manager.test.ts with PI_FAMULUS_INTEG=1 and the same binary.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { channel } from "node:diagnostics_channel";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { expect, it } from "vitest";
import { ManagerClient } from "../../src/manager-client";

const RUN = process.env.PI_FAMULUS_INTEG === "1";
const BIN = process.env.PI_FAMULUS_MANAGER_PATH ?? join(__dirname, "../../../manager/target/release/pi-famulus");
if (RUN && !existsSync(BIN)) throw new Error(`Requested integration binary does not exist: ${BIN}`);

function bounded<T>(promise: Promise<T>, label: string, ms = 3000): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${label} did not settle within ${ms}ms`)), ms);
    promise.then(
      (value) => { clearTimeout(timer); resolve(value); },
      (error) => { clearTimeout(timer); reject(error); },
    );
  });
}

function alive(child: ChildProcess): boolean {
  return child.exitCode === null && child.signalCode === null;
}

async function stopAndWait(child: ChildProcess, closed: Promise<void>, label: string): Promise<void> {
  if (alive(child)) child.kill("SIGTERM");
  try {
    await bounded(closed, `${label} close`, 2000);
  } catch {
    if (alive(child)) child.kill("SIGKILL");
    await bounded(closed, `${label} close after SIGKILL`, 500);
  }
}

type Proof = { code: number | null; socketMissing: boolean; pidMissing: boolean };
type Fixture = {
  home: string;
  client: ManagerClient;
  attempts: ChildProcess[];
  proof: Proof;
  loserOutput: string;
  loserHadReadableOutput: boolean;
};
type Cleanup = {
  home: string;
  observerSubscribed: boolean;
  holderClosed: boolean;
  managerChildren: number;
  closedManagerChildren: number;
  clientAvailable: boolean;
};

async function withLockGap(
  binary: string | ((home: string) => string),
  verify: (fixture: Fixture) => Promise<void>,
  onCleanup?: (cleanup: Cleanup) => void,
): Promise<void> {
  const home = mkdtempSync(join(tmpdir(), "fam-real-gap-"));
  const managerPath = typeof binary === "function" ? binary(home) : binary;
  const socketPath = join(home, "manager.sock");
  const pidPath = join(home, "manager.pid");
  let cleaningUp = false;
  let resolveLoser!: (proof: Proof) => void;
  let rejectLoser!: (error: Error) => void;
  const loser = new Promise<Proof>((resolve, reject) => { resolveLoser = resolve; rejectLoser = reject; });
  // A fixture error may arrive before readiness; keep it handled until the race below.
  void loser.catch(() => {});
  const holder = spawn("python3", ["-u", "-c", `import fcntl, sys
with open(sys.argv[1], 'a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    print('locked', flush=True)
    sys.stdin.readline()
    fcntl.flock(lock, fcntl.LOCK_UN)
print('released', flush=True)
`, join(home, "manager.lock")], { stdio: ["pipe", "pipe", "pipe"] });
  let holderClosed = false;
  const holderExit = new Promise<void>((resolve) => holder.once("close", () => { holderClosed = true; resolve(); }));
  const holderLines = createInterface({ input: holder.stdout });
  let holderError = "";
  holder.stderr.on("data", (data: Buffer) => { holderError = (holderError + data.toString()).slice(-2048); });
  holder.stdin.on("error", (error) => { if (!cleaningUp) rejectLoser(error); });
  const holderReady = new Promise<void>((resolve, reject) => {
    holderLines.once("line", (line) => line === "locked" ? resolve() : reject(new Error(`Unexpected flock fixture: ${line}`)));
    holder.once("error", reject);
    holder.once("exit", () => reject(new Error(`Python flock fixture exited before readiness: ${holderError}`)));
  });
  const attempts: ChildProcess[] = [];
  const childCloses = new Map<ChildProcess, Promise<void>>();
  const closedChildren = new Set<ChildProcess>();
  const detachObservers: (() => void)[] = [];
  let loserOutput = "";
  let loserHadReadableOutput = false;
  const processChannel = channel("child_process");
  let observerSubscribed = false;
  const observeProcess = (message: unknown) => {
    const child = (message as { process: ChildProcess }).process;
    // Node publishes before spawnargs are populated. Match again on error:
    // ENOENT/EACCES emit error + close, never spawn.
    const matches = () => child.spawnfile === managerPath
      && child.spawnargs.includes(home) && child.spawnargs.includes("daemon");
    const closed = new Promise<void>((resolve) => child.once("close", () => {
      closedChildren.add(child);
      resolve();
    }));
    const track = () => {
      if (!matches()) return false;
      if (!childCloses.has(child)) {
        attempts.push(child);
        childCloses.set(child, closed);
      }
      return true;
    };
    const capture = (data: Buffer) => { loserOutput = (loserOutput + data.toString()).slice(-2048); };
    const onSpawn = () => {
      if (!track() || attempts[0] !== child) return;
      loserHadReadableOutput = child.stdout !== null;
      child.stdout?.on("data", capture);
    };
    const onError = (error: Error) => {
      if (track()) rejectLoser(new Error(`manager spawn failed: ${error.message}`));
    };
    const onClose = (code: number | null) => {
      if (!track() || attempts[0] !== child) return;
      resolveLoser({ code, socketMissing: !existsSync(socketPath), pidMissing: !existsSync(pidPath) });
      if (!cleaningUp && !holder.stdin.destroyed && holder.stdin.writable) holder.stdin.write("release\n");
    };
    child.once("spawn", onSpawn);
    child.once("error", onError);
    child.once("close", onClose);
    detachObservers.push(() => {
      child.off("spawn", onSpawn);
      child.off("error", onError);
      child.off("close", onClose);
      child.stdout?.off("data", capture);
    });
  };
  const client = new ManagerClient({ home, sessionId: "real-lock-gap", managerPath });
  let connecting: Promise<boolean> | undefined;
  let failure: unknown;
  try {
    await bounded(holderReady, "flock readiness");
    expect(existsSync(socketPath)).toBe(false);
    expect(existsSync(pidPath)).toBe(false);
    processChannel.subscribe(observeProcess);
    observerSubscribed = true;
    // Share the client's ten-second startup budget rather than adding a
    // shorter success-latency requirement to this lock-handoff contract.
    const connectionDeadline = Date.now() + 10_000;
    connecting = client.connect();
    // A false connect must fail now, not leave the fixture awaiting a spawn
    // event that never happened. Both race inputs retain rejection handlers.
    const connectFailure = connecting.then((ok) => {
      if (!ok) throw new Error(client.lastError());
      return loser;
    });
    const proof = await bounded(Promise.race([loser, connectFailure]), "losing manager spawn");
    await bounded(holderExit, "flock release");
    expect(await bounded(connecting, "manager connection", Math.max(1, connectionDeadline - Date.now() + 50)), client.lastError()).toBe(true);
    await verify({ home, client, attempts, proof, loserOutput, loserHadReadableOutput });
  } catch (error) {
    failure = error;
  } finally {
    cleaningUp = true;
    processChannel.unsubscribe(observeProcess);
    observerSubscribed = false;
    holderLines.close();
    try { holder.stdin.end(); } catch { /* an error listener also handles asynchronous EPIPE */ }
    try {
      await stopAndWait(holder, holderExit, "flock holder");
      await client.close();
    } catch (error) {
      failure ??= error;
    } finally {
      // Close even if the holder watchdog failed; only signal our actual children.
      await client.close();
      try {
        await Promise.all(attempts.map((child) => stopAndWait(child, childCloses.get(child)!, "manager child")));
        if (connecting) await bounded(connecting, "cancelled manager connection", 2000);
      } catch (error) {
        failure ??= error;
      } finally {
        for (const detach of detachObservers) detach();
        rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
        onCleanup?.({
          home, observerSubscribed, holderClosed,
          managerChildren: attempts.length,
          closedManagerChildren: attempts.filter((child) => closedChildren.has(child)).length,
          clientAvailable: client.isAvailable(),
        });
      }
    }
  }
  if (failure !== undefined) throw failure;
}

it.skipIf(!RUN)("regression: missing socket/pid while the old lifetime lock is held recovers after the real losing spawn", async () => {
  await withLockGap(BIN, async ({ home, client, attempts, proof, loserOutput, loserHadReadableOutput }) => {
    expect(proof).toEqual({ code: 0, socketMissing: true, pidMissing: true });
    if (loserHadReadableOutput) expect(loserOutput).toContain("pi-famulus already running (starting up)");
    // A loaded host may need another contender before Python gets scheduled;
    // assert one live winner rather than assuming release beats a 100ms retry.
    expect(attempts.length).toBeGreaterThanOrEqual(2);
    expect(attempts.slice(0, -1).every((child) => child.exitCode === 0)).toBe(true);
    expect(JSON.parse(readFileSync(join(home, "manager.pid"), "utf8")).pid).toBe(attempts.at(-1)!.pid);
    expect(client.isAvailable()).toBe(true);
    expect(await client.list()).toEqual([]);
    expect(existsSync(join(home, "manager.sock"))).toBe(true);
    expect(existsSync(join(home, "manager.pid"))).toBe(true);
  });
}, 15000);

it.skipIf(!RUN).each([
  ["ENOENT", (home: string) => join(home, "missing-manager")],
  ["EACCES", (home: string) => home], // An existing directory cannot be executed.
] as const)("regression: real %s spawn failure rejects and cleans up the flock fixture", async (code, binary) => {
  let cleanup: Cleanup | undefined;
  let verified = false;
  await expect(withLockGap(binary, async () => { verified = true; }, (result) => { cleanup = result; })).rejects.toThrow(code);
  expect(verified).toBe(false);
  expect(cleanup).toMatchObject({
    observerSubscribed: false, holderClosed: true,
    managerChildren: 1, closedManagerChildren: 1, clientAvailable: false,
  });
  expect(existsSync(cleanup!.home)).toBe(false);
}, 15000);
