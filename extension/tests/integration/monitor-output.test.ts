import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ManagerClient, type ManagerEvent } from "../../src/manager-client";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import type { FamulusWake } from "../../src/wake";

const BIN =
  process.env.PI_FAMULUS_MANAGER_PATH ??
  join(__dirname, "../../../manager/target/debug", process.platform === "win32" ? "pi-famulus.exe" : "pi-famulus");
const RUN = process.env.PI_FAMULUS_INTEG === "1";
if (RUN && process.platform !== "win32" && !existsSync(BIN)) throw new Error(`Missing requested manager: ${BIN}`);

// Regression: actual manager framing -> TS window gate -> emitted model context.
// The original incident printed unchanged state with a new timestamp each check.
describe.skipIf(!RUN || process.platform === "win32")("monitor original output and bounded model context", () => {
  it.each(["exact", "ignore-timestamps"] as const)("preserves raw bytes while applying explicit %s fingerprint mode", async (mode) => {
    const home = mkdtempSync(join(tmpdir(), "famulus-monitor-output-"));
    const sent: { content: string; details?: unknown }[] = [];
    const seen: ManagerEvent[] = [];
    const client = new ManagerClient({ home, sessionId: `monitor-${mode}`, managerPath: BIN, cwd: tmpdir() });
    const center = new NotifyCenter({ isIdle: () => true, sendMessage: (m) => sent.push(m) });
    let done!: () => void;
    const exited = new Promise<void>((resolve) => { done = resolve; });
    const registry = new MonitorRegistry({
      getClient: () => client, sessionEnv: () => ({}), getNotifyCenter: () => center, trackTask: () => {}, onExited: done,
    });
    const unsubscribe = client.onEvent((event) => {
      seen.push(event);
      if (event.event === "output" && event.task_id && typeof event.chunk === "string") registry.handleOutput(event.task_id, event.chunk, event.next_cursor, event.monitor_records);
      if (event.event === "task_exited" && event.task_id) registry.handleExit(event.task_id, event);
    });
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const raw = "ssh_login_fail 2026-10-15T23:20:15Z\nssh_login_fail 2026-10-15T23:20:38Z\nssh_login_fail 2026-10-15T23:20:38Z\nHOST_READY\n";
      const { taskId } = await registry.start({
        command: `printf '%s' '${raw}'`, description: "SSH login", fingerprint_mode: mode,
      }, { cwd: tmpdir() } as never);
      await Promise.race([exited, new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("monitor did not exit")), 10_000); })]);
      center.settled();
      const output = await client.output(taskId, 0, 8192);
      expect(output.chunk).toBe(raw);
      expect(output.next_cursor).toBe(Buffer.byteLength(raw));
      expect(seen.flatMap((e) => e.monitor_records ?? []).some((r) => r.fingerprint?.startsWith("blake3:v1:"))).toBe(true);
      const events = sent.map((s) => s.details as FamulusWake).filter((w) => w.kind === "monitor" && !w.status);
      const preview = events.map((w) => w.kind === "monitor" ? w.event : "").join("\n");
      expect(preview.match(/ssh_login_fail/g)).toHaveLength(mode === "exact" ? 2 : 1);
      expect(preview).toContain("HOST_READY");
      const suppressed = sent.reduce((n, s) => {
        const w = s.details as FamulusWake;
        return n + (w.kind === "monitor" ? w.output?.suppressedCount ?? 0 : 0);
      }, 0);
      expect(suppressed).toBe(mode === "exact" ? 1 : 2);
      expect(events.length).toBeLessThanOrEqual(2);
      expect(sent.some((s) => s.content.includes("cursor-start="))).toBe(true);
    } finally {
      if (timer) clearTimeout(timer);
      unsubscribe(); registry.disposeAll(); center.dispose();
      await client.close();
      try { execFileSync(BIN, ["--home", home, "shutdown"], { timeout: 5000, stdio: "ignore" }); } catch { /* already stopped */ }
      rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    }
  }, 20_000);
});
