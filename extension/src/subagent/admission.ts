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
  /** Locally admitted children awaiting a machine permit after reconnect. */
  pendingLeases?: Set<string>;
  ticket?: AdmissionTicket;
  notice(reason: "manager unavailable" | "daemon too old"): void;
  wait?: () => Promise<void>;
}

const defaultWait = () => new Promise<void>((resolve) => setTimeout(resolve, 500));
const MAX_REREGISTER_ATTEMPTS = 40;

/** Reserve the per-session slot first, then wait for a machine-wide permit. */
export async function admitAgentChild(options: AgentAdmissionOptions): Promise<(terminal?: boolean) => void> {
  const { childId, reserveLocal, manager, leases, pendingLeases, ticket, notice } = options;
  const wait = options.wait ?? defaultWait;
  for (;;) {
    const local = await reserveLocal();
    const release = (reservation: LocalReservation) => (terminal = true) => {
      reservation.release();
      if (terminal) {
        pendingLeases?.delete(childId);
        if (leases.delete(childId)) void manager?.releaseAgent(childId).catch(() => {});
      }
    };
    const admitLocally = () => {
      try {
        local.admit();
      } catch (error) {
        local.release();
        throw error;
      }
      pendingLeases?.add(childId);
      return release(local);
    };

    if (!manager?.isAvailable()) {
      notice("manager unavailable");
      return admitLocally();
    }
    if (manager.protocolLevel() < 4) {
      notice("daemon too old");
      return admitLocally();
    }
    if (leases.has(childId)) {
      try {
        local.admit();
      } catch (error) {
        local.release();
        throw error;
      }
      return release(local);
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
        try {
          fallback.admit();
        } catch (fallbackError) {
          fallback.release();
          throw fallbackError;
        }
        return release(fallback);
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
      pendingLeases?.delete(childId);
      leases.add(childId);
      return release(local);
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
  pendingLeases?: Set<string>,
): Promise<void> {
  if (manager.protocolLevel() < 4) return;
  const childIds = new Set([...leases, ...(pendingLeases ?? [])]);
  for (const childId of childIds) {
    const stillHeld = () => leases.has(childId) || pendingLeases?.has(childId) === true;
    for (let attempt = 0; attempt < MAX_REREGISTER_ATTEMPTS && stillHeld() && manager.isAvailable() && manager.protocolLevel() >= 4; attempt++) {
      const admission = await manager.acquireAgent(childId);
      if (!stillHeld()) {
        if (admission.granted) void manager.releaseAgent(childId).catch(() => {});
        break;
      }
      if (admission.granted) {
        pendingLeases?.delete(childId);
        leases.add(childId);
        break;
      }
      if (attempt + 1 < MAX_REREGISTER_ATTEMPTS) await wait();
    }
  }
}
