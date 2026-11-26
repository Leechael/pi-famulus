import { describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { MonitorRegistry } from "../../src/monitor";
import { NotifyCenter } from "../../src/notify";
import type { ManagerClient } from "../../src/manager-client";

// A monitor with no lifetime runs until its command exits or it is stopped.
// In the 2026-11-07 log-monitor session the agent sized timeouts to waits it
// could not predict (10 of 17 monitors were timed) and re-armed them; a
// replay of that session showed wording alone did not move the choice.
function registry() {
  const clock = new ManualClock();
  const center = new NotifyCenter({ clock, isIdle: () => true, sendMessage: () => {} });
  let n = 0;
  const client = { ensureAvailable: async () => true, start: async () => ({ task_id: `mon_${++n}` }), watch: async () => {} } as unknown as ManagerClient;
  return new MonitorRegistry({ clock, getClient: () => client, sessionEnv: () => ({}), getNotifyCenter: () => center, trackTask: () => {} });
}
const start = (params: Record<string, unknown>) => registry().start({ command: "follow", description: "watch", ...params } as never, { cwd: "/tmp" } as never);

describe("monitor default lifetime", () => {
  it("is persistent when neither timeout_ms nor persistent is given", async () => {
    expect((await start({})).timeoutMs).toBeNull();
  });
  it("keeps an explicit timeout_ms, and persistent:false falls back to the 5-minute default", async () => {
    expect((await start({ timeout_ms: 60_000 })).timeoutMs).toBe(60_000);
    expect((await start({ persistent: false })).timeoutMs).toBe(300_000);
  });
  it("still rejects persistent:true with timeout_ms", async () => {
    await expect(start({ persistent: true, timeout_ms: 60_000 })).rejects.toThrow(/exclusive/);
  });
});
