export interface LocalReservation {
  admit(): void;
  release(): void;
}

export interface AgentAdmissionManager {
  isAvailable(): boolean;
  protocolLevel(): number;
  acquireAgent(
    childId: string,
    workKind?: string,
    signal?: AbortSignal,
  ): Promise<{ granted: boolean; rejection?: string }>;
  releaseAgent(childId: string): Promise<void>;
}

export interface AdmissionTicket {
  current(): boolean;
  signal: AbortSignal;
}

export interface AgentAdmissionOptions {
  childId: string;
  workKind?: string;
  reserveLocal(): Promise<LocalReservation>;
  manager: AgentAdmissionManager | null;
  leases: Map<string, string>;
  /** Reconnect-time acquires to abort if the child settles while queued. */
  pendingReregistrations?: Map<string, AbortController>;
  ticket?: AdmissionTicket;
  notice(reason: "manager unavailable" | "daemon too old"): void;
  wait?: (signal?: AbortSignal) => Promise<void>;
}

const defaultWait = (signal?: AbortSignal) => new Promise<void>((resolve, reject) => {
  const timer = setTimeout(() => {
    signal?.removeEventListener("abort", onAbort);
    resolve();
  }, 500);
  const onAbort = () => {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onAbort);
    reject(new Error("subagent admission cancelled"));
  };
  if (signal?.aborted) onAbort();
  else signal?.addEventListener("abort", onAbort, { once: true });
});

function waitForRetry(wait: (signal?: AbortSignal) => Promise<void>, signal?: AbortSignal): Promise<void> {
  if (!signal) return wait();
  if (signal.aborted) return Promise.reject(new Error("subagent admission cancelled"));
  return new Promise<void>((resolve, reject) => {
    const cleanup = () => signal.removeEventListener("abort", onAbort);
    const onAbort = () => {
      cleanup();
      reject(new Error("subagent admission cancelled"));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    wait(signal).then(
      () => {
        cleanup();
        resolve();
      },
      (error) => {
        cleanup();
        reject(error);
      },
    );
  });
}

/** Reserve the per-session slot first, then obtain a machine-wide work-kind permit. */
export async function admitAgentChild(options: AgentAdmissionOptions): Promise<(terminal?: boolean) => void> {
  const { childId, reserveLocal, manager, leases, ticket, notice } = options;
  const cancelReregistration = () => options.pendingReregistrations?.get(childId)?.abort();
  const workKind = options.workKind ?? "other";
  const wait = options.wait ?? defaultWait;
  for (;;) {
    const local = await reserveLocal();
    const release = (terminal = true) => {
      local.release();
      if (terminal || !leases.has(childId)) {
        cancelReregistration();
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
      admission = await manager.acquireAgent(childId, workKind, ticket?.signal);
    } catch (error) {
      local.release();
      if (ticket?.signal.aborted || (ticket && !ticket.current())) {
        throw new Error("subagent admission cancelled");
      }
      if (manager.isAvailable()) throw error;
      notice("manager unavailable");
      // The request may have been granted before its response was lost.
      leases.set(childId, workKind);
      try {
        const fallback = await reserveLocal();
        fallback.admit();
        return (terminal = true) => {
          fallback.release();
          if (terminal) cancelReregistration();
          if (terminal && leases.delete(childId)) void manager.releaseAgent(childId).catch(() => {});
        };
      } catch (fallbackError) {
        cancelReregistration();
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
      leases.set(childId, workKind);
      return release;
    }

    local.release();
    if (ticket && !ticket.current()) throw new Error("subagent admission cancelled");
    await waitForRetry(wait, ticket?.signal);
  }
}

/** Re-register extension-held leases after hello/reconnect, preserving kind. */
export async function reregisterAgentLeases(
  manager: AgentAdmissionManager,
  leases: Map<string, string>,
  wait: () => Promise<void> = defaultWait,
  pendingReregistrations?: Map<string, AbortController>,
): Promise<void> {
  if (manager.protocolLevel() < 4) return;
  await Promise.all([...leases].map(async ([childId, workKind]) => {
    while (leases.has(childId) && manager.isAvailable() && manager.protocolLevel() >= 4) {
      if (pendingReregistrations?.has(childId)) break;
      const controller = new AbortController();
      pendingReregistrations?.set(childId, controller);
      let admission: { granted: boolean; rejection?: string };
      try {
        admission = await (pendingReregistrations
          ? manager.acquireAgent(childId, workKind, controller.signal)
          : manager.acquireAgent(childId, workKind));
      } catch (error) {
        if (!leases.has(childId)) break;
        if (
          error instanceof Error
          && error.message.startsWith("pi-famulus request timed out: acquire_agent")
          && manager.isAvailable()
        ) {
          await wait();
          continue;
        }
        throw error;
      } finally {
        if (pendingReregistrations?.get(childId) === controller) pendingReregistrations.delete(childId);
      }
      if (!leases.has(childId)) {
        if (admission.granted) void manager.releaseAgent(childId).catch(() => {});
        break;
      }
      if (admission.granted) break;
      await wait();
    }
  }));
}
