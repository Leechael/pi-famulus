import { describe, expect, it, vi } from "vitest";
import {
  admitAgentChild,
  reregisterAgentLeases,
  type AgentAdmissionManager,
  type LocalReservation,
} from "../../src/subagent/admission";

function setup() {
  let used = 0;
  const order: string[] = [];
  const reserveLocal = async (): Promise<LocalReservation> => {
    order.push("local.acquire");
    used++;
    let released = false;
    return {
      admit: () => { order.push("local.admit"); },
      release: () => {
        if (released) return;
        released = true;
        used--;
        order.push("local.release");
      },
    };
  };
  const manager: AgentAdmissionManager = {
    isAvailable: () => true,
    protocolLevel: () => 4,
    acquireAgent: vi.fn(async () => ({ granted: true })),
    releaseAgent: vi.fn(async () => {}),
  };
  return { manager, order, reserveLocal, used: () => used };
}

describe("machine agent admission", () => {
  it("backs off on structured rejection, frees local slot, then grants", async () => {
    const s = setup();
    vi.mocked(s.manager.acquireAgent)
      .mockResolvedValueOnce({ granted: false, rejection: "global_capacity" })
      .mockResolvedValueOnce({ granted: true });
    const wait = vi.fn(async () => {});
    const release = await admitAgentChild({
      childId: "ch-1", reserveLocal: s.reserveLocal, manager: s.manager,
      leases: new Set(), notice: () => {}, wait,
    });
    expect(wait).toHaveBeenCalledOnce();
    expect(s.manager.acquireAgent).toHaveBeenCalledTimes(2);
    expect(s.used()).toBe(1);
    expect(s.order.slice(0, 4)).toEqual(["local.acquire", "local.release", "local.acquire", "local.admit"]);
    release();
    expect(s.used()).toBe(0);
  });

  it.each([
    ["old daemon", true, 3, "daemon too old"],
    ["no daemon", false, 0, "manager unavailable"],
  ] as const)("uses local-only admission for %s", async (_name, available, protocol, reason) => {
    const s = setup();
    s.manager.isAvailable = () => available;
    s.manager.protocolLevel = () => protocol;
    const notice = vi.fn();
    const release = await admitAgentChild({
      childId: "ch-1", reserveLocal: s.reserveLocal, manager: s.manager,
      leases: new Set(), notice,
    });
    expect(s.manager.acquireAgent).not.toHaveBeenCalled();
    expect(notice).toHaveBeenCalledWith(reason);
    release();
    expect(s.used()).toBe(0);
  });

  it("does not reacquire a held permit on user resume; releases it at terminal settle", async () => {
    const s = setup();
    const leases = new Set<string>();
    const acquire = () => admitAgentChild({ childId: "ch-1", reserveLocal: s.reserveLocal, manager: s.manager, leases, notice: () => {} });
    const interrupted = await acquire();
    interrupted(false);
    expect(leases.has("ch-1")).toBe(true);
    const resumed = await acquire();
    expect(s.manager.acquireAgent).toHaveBeenCalledOnce();
    resumed(true);
    expect(s.manager.releaseAgent).toHaveBeenCalledOnce();
    expect(s.manager.releaseAgent).toHaveBeenCalledWith("ch-1");
    expect(leases.has("ch-1")).toBe(false);
  });

  it("re-registers extension-held running child ids after reconnect", async () => {
    const s = setup();
    const leases = new Set(["ch-running"]);
    await reregisterAgentLeases(s.manager, leases);
    expect(s.manager.acquireAgent).toHaveBeenCalledOnce();
    expect(s.manager.acquireAgent).toHaveBeenCalledWith("ch-running");
  });

  it("stops retrying a child disposed while waiting and restores remaining leases", async () => {
    const s = setup();
    const leases = new Set(["disposed-first", "still-running"]);
    vi.mocked(s.manager.acquireAgent)
      .mockResolvedValueOnce({ granted: false, rejection: "global_capacity" })
      .mockResolvedValueOnce({ granted: true });
    await reregisterAgentLeases(s.manager, leases, async () => { leases.delete("disposed-first"); });
    expect(s.manager.acquireAgent).toHaveBeenCalledTimes(2);
    expect(s.manager.acquireAgent).toHaveBeenNthCalledWith(1, "disposed-first");
    expect(s.manager.acquireAgent).toHaveBeenNthCalledWith(2, "still-running");
  });

  it("gets the local slot before global acquire and frees it while globally denied", async () => {
    const s = setup();
    vi.mocked(s.manager.acquireAgent)
      .mockImplementationOnce(async () => {
        expect(s.used()).toBe(1);
        return { granted: false, rejection: "global_capacity" };
      })
      .mockImplementationOnce(async () => {
        expect(s.used()).toBe(1);
        return { granted: true };
      });
    const release = await admitAgentChild({
      childId: "ch-1", reserveLocal: s.reserveLocal, manager: s.manager,
      leases: new Set(), notice: () => {}, wait: async () => { expect(s.used()).toBe(0); },
    });
    expect(s.order).toEqual(["local.acquire", "local.release", "local.acquire", "local.admit"]);
    release();
    expect(s.used()).toBe(0);
  });
});
