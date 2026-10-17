/**
 * CommsHost adapter over the subagent RunRegistry (integration wiring, §4.7).
 *
 * Lives in comms/ so the M4 module stays self-contained; the registry is
 * accessed structurally (RunRegistry query surface from appendix B) plus the
 * concrete SubagentRegistry.handle() lookup.
 */
import type { SubagentRegistry } from "../subagent/registry";
import { FAMULUS_WAKE_CUSTOM_TYPE } from "../wake";

import type { CommsHost } from "./types";

export interface RegistryHostDeps {
  getRegistry: () => SubagentRegistry | null;
  /** NotifyCenter-compatible sink for supervisor-bound notifications. */
  getNotifyCenter: () => {
    notify(msg: { customType: string; content: string; details?: unknown }): void;
  } | null;
}


export function createRegistryCommsHost(deps: RegistryHostDeps): CommsHost {
  const locate = (childId: string) => {
    const registry = deps.getRegistry();
    if (!registry) return undefined;
    for (const run of registry.list()) {
      const child = run.children.find((c) => c.childId === childId || c.name === childId);
      if (child) return { run, child };
    }
    return undefined;
  };

  return {
    getChild(childId) {
      const found = locate(childId);
      const handle = deps.getRegistry()?.handle(found?.child.childId ?? childId);
      if (!found || !handle) return undefined;
      return {
        handle,
        runId: found.run.runId,
        name: found.child.name,
        status: found.child.status,
      };
    },

    listChildren() {
      const registry = deps.getRegistry();
      if (!registry) return [];
      return registry.list().flatMap((run) =>
        run.children.map((c) => ({
          childId: c.childId,
          runId: run.runId,
          name: c.name,
          status: c.status,
        })),
      );
    },

    sameRun(childIdA, childIdB) {
      const a = locate(childIdA);
      const b = locate(childIdB);
      return a !== undefined && b !== undefined && a.run.runId === b.run.runId;
    },

    notifySupervisor(wake) {
      deps.getNotifyCenter()?.notify({
        customType: FAMULUS_WAKE_CUSTOM_TYPE,
        content: wake.content,
        details: wake.details,
      });
    },

    async resumeChild(childId: string, message: string) {
      const registry = deps.getRegistry();
      if (!registry) throw new Error("subagent system is not initialized");
      await registry.resumeChild(childId, message);
    },
  };
}
