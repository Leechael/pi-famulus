export interface LocalReservation {
  admit(): void;
  release(): void;
}

export interface AgentAdmissionManager {
  isAvailable(): boolean;
  protocolLevel(): number;
  acquireAgent(childId: string): Promise<{ granted: boolean; rejection?: string }>;
  releaseAgent(childId: string): Promise<void>;
}

export interface AdmissionTicket {
  current(): boolean;
}

export interface AgentAdmissionOptions {
  childId: string;
  reserveLocal(): Promise<LocalReservation>;
  manager: AgentAdmissionManager | null;
  leases: Set<string>;
  ticket?: AdmissionTicket;
  notice(reason: "manager unavailable" | "daemon too old"): void;
  wait?: () => Promise<void>;
}

const defaultWait = () => new Promise<void>((resolve) => setTimeout(resolve, 500));

/** Reserve the per-session slot first, then wait for a machine-wide permit. */
export async function admitAgentChild(options: AgentAdmissionOptions): Promise<(terminal?: boolean) => void> {
  const { childId, reserveLocal, manager, leases, ticket, notice } = options;
  const wait = options.wait ?? defaultWait;
  for (;;) {
    const local = await reserveLocal();
    const release = (terminal = true) => {
      local.release();
      if (terminal || !leases.has(childId)) {
        if (leases.delete(childId)) void manager?.releaseAgent(childId).catch(() => {});
      }
    };

    if (!manager?.isAvailable()) {
      notice("manager unavailable");
      local.admit();
      return release;
    }
    if (manager.protocolLevel() < 4) {
      notice("daemon too old");
      local.admit();
      return release;
    }
    if (leases.has(childId)) {
      local.admit();
      return release;
    }

    let admission: { granted: boolean; rejection?: string };
    try {
      admission = await manager.acquireAgent(childId);
    } catch (error) {
      local.release();
      if (manager.isAvailable()) throw error;
      notice("manager unavailable");
      // The request may have been granted before its response was lost.
      leases.add(childId);
      try {
        const fallback = await reserveLocal();
        fallback.admit();
        return (terminal = true) => {
          fallback.release();
          if (terminal && leases.delete(childId)) void manager.releaseAgent(childId).catch(() => {});
        };
      } catch (fallbackError) {
        if (leases.delete(childId)) void manager.releaseAgent(childId).catch(() => {});
        throw fallbackError;
      }
    }
    if (admission.granted) {
      try {
        if (ticket && !ticket.current()) throw new Error("subagent admission cancelled");
        local.admit();
      } catch (error) {
        local.release();
        void manager.releaseAgent(childId).catch(() => {});
        throw error;
      }
      leases.add(childId);
      return release;
    }

    local.release();
    if (ticket && !ticket.current()) throw new Error("subagent admission cancelled");
    await wait();
  }
}

/** Re-register all extension-held leases after hello/reconnect; acquire is idempotent. */
export async function reregisterAgentLeases(
  manager: AgentAdmissionManager,
  leases: Set<string>,
  wait: () => Promise<void> = defaultWait,
): Promise<void> {
  if (manager.protocolLevel() < 4) return;
  for (const childId of leases) {
    while (manager.isAvailable() && manager.protocolLevel() >= 4) {
      const admission = await manager.acquireAgent(childId);
      if (admission.granted) {
        if (!leases.has(childId)) void manager.releaseAgent(childId).catch(() => {});
        break;
      }
      await wait();
    }
  }
}
