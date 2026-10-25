import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { describe, it } from "node:test";
import { observeProcessExit } from "../lib/process-exit-observer.ts";

const scriptedObserver = (script: string) => () => spawn(process.execPath, ["-e", script], { stdio: ["ignore", "pipe", "pipe"] });

async function target() {
  const child = spawn(process.execPath, ["-e", "console.log('ready'); setInterval(() => {}, 1000)"], { stdio: ["ignore", "pipe", "pipe"] });
  await once(child.stdout!, "data");
  return child;
}

describe("process exit observer", () => {
  it("observes the captured process exiting after readiness", async () => {
    const child = await target();
    let observer;
    try {
      observer = await observeProcessExit(child.pid!, 5000);
      assert.match(observer.identity, new RegExp(`^${child.pid}(?::|$)`));
      const closed = once(child, "close");
      child.kill();
      await closed;
      assert.equal(await observer.waitForExit(), true);
    } finally {
      observer?.dispose();
      child.kill();
    }
  });

  it("does not report an exit while the captured process is still running", async () => {
    const child = await target();
    let observer;
    try {
      observer = await observeProcessExit(child.pid!, 150);
      assert.equal(await observer.waitForExit(), false);
      assert.equal(child.exitCode, null);
      assert.equal(child.signalCode, null);
    } finally {
      observer?.dispose();
      child.kill();
    }
  });

  // Regression counterexample: the OS observer reports the captured handle
  // exited while that numeric PID now resolves to a different live process.
  it("never replaces the captured incarnation's result with a fresh PID probe", async () => {
    const child = await target();
    let observer;
    try {
      observer = await observeProcessExit(child.pid!, 1000, scriptedObserver(`console.log('ready:${child.pid}:original'); process.exitCode = 0;`));
      assert.equal(await observer.waitForExit(), true);
      assert.doesNotThrow(() => process.kill(child.pid!, 0), "synthetic replacement is still live");
    } finally {
      observer?.dispose();
      child.kill();
    }
  });

  it("fails capture errors instead of claiming the target exited", async () => {
    await assert.rejects(
      observeProcessExit(42, 1000, scriptedObserver("console.error('cannot open process handle'); process.exitCode = 1;")),
      /exited before readiness.*cannot open process handle/,
    );
  });

  it("rejects readiness for a different PID", async () => {
    await assert.rejects(
      observeProcessExit(42, 1000, scriptedObserver("console.log('ready:420:other'); setInterval(() => {}, 1000);")),
      /invalid process observer readiness/,
    );
  });

  it("disposes a live observer without reporting that the target exited", async () => {
    const observer = await observeProcessExit(42, 1000, scriptedObserver("console.log('ready:42:original'); setInterval(() => {}, 1000);"));
    observer.dispose();
    await assert.rejects(observer.waitForExit(), /process observer failed/);
  });

  it("fails observer errors after readiness instead of claiming an exit or timeout", async () => {
    const observer = await observeProcessExit(42, 1000, scriptedObserver("console.log('ready:42:original'); console.error('wait failed'); process.exitCode = 1;"));
    try {
      await assert.rejects(observer.waitForExit(), /process observer failed.*wait failed/);
    } finally {
      observer.dispose();
    }
  });
});
