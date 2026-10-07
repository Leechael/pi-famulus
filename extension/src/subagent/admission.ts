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
  /** Locally held or potentially-held daemon permits, keyed by their kind. */
  leases: Map<string, string>;
  /** Locally admitted children still waiting for a daemon permit. */
  pendingLeases?: Set<string>;
  /** Reconnect-time acquires to abort if the child settles while queued. */
  pendingReregistrations?: Map<string, AbortController>;
  ticket?: AdmissionTicket;
  notice(reason: "manager unavailable" | "daemon too old"): void;
  wait?: (signal?: AbortSignal) => Promise<void>;
}

const MAX_REREGISTER_ATTEMPTS = 40;

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
  const { childId, reserveLocal, manager, leases, pendingLeases, ticket, notice } = options;
  const workKind = options.workKind ?? "other";
  const wait = options.wait ?? defaultWait;
  const cancelReregistration = () => options.pendingReregistrations?.get(childId)?.abort();
  const finish = (reservation: LocalReservation) => (terminal = true) => {
    reservation.release();
    if (terminal) {
      cancelReregistration();
      pendingLeases?.delete(childId);
      if (leases.delete(childId)) void manager?.releaseAgent(childId).catch(() => {});
    }
  };
  const admitLocally = (reservation: LocalReservation) => {
    try {
      reservation.admit();
    } catch (error) {
      reservation.release();
      throw error;
    }
    // Keep locally admitted fallback children in the reconciliation set too.
    // A daemon reconnect can then acquire their permit before a later resume.
    leases.set(childId, workKind);
    pendingLeases?.add(childId);
    return finish(reservation);
  };

  for (;;) {
    const local = await reserveLocal();
    if (!manager?.isAvailable()) {
      notice("manager unavailable");
      return admitLocally(local);
    }
    if (manager.protocolLevel() < 4) {
      notice("daemon too old");
      return admitLocally(local);
    }
    // A local fallback lease is not yet a daemon permit. Reacquire it, while a
    // previously granted machine permit survives normal generation resumes.
    if (leases.has(childId) && !pendingLeases?.has(childId)) {
      try {
        local.admit();
      } catch (error) {
        local.release();
        throw error;
      }
      return finish(local);
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
      // Remember it as pending so reconnect's idempotent acquire can reconcile
      // that uncertain server-side result without consuming another slot.
      let fallback: LocalReservation;
      try {
        fallback = await reserveLocal();
      } catch (fallbackError) {
        void manager.releaseAgent(childId).catch(() => {});
        throw fallbackError;
      }
      try {
        return admitLocally(fallback);
      } catch (fallbackError) {
        void manager.releaseAgent(childId).catch(() => {});
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
      pendingLeases?.delete(childId);
      return finish(local);
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
  wait: (signal?: AbortSignal) => Promise<void> = defaultWait,
  pendingLeases?: Set<string>,
  pendingReregistrations?: Map<string, AbortController>,
): Promise<void> {
  if (manager.protocolLevel() < 4) return;
  const childIds = new Set([...leases.keys(), ...(pendingLeases ?? [])]);
  await Promise.all([...childIds].map(async (childId) => {
    const stillHeld = () => leases.has(childId) || pendingLeases?.has(childId) === true;
    for (let attempt = 0; attempt < MAX_REREGISTER_ATTEMPTS && stillHeld() && manager.isAvailable() && manager.protocolLevel() >= 4; attempt++) {
      if (pendingReregistrations?.has(childId)) break;
      const workKind = leases.get(childId) ?? "other";
      const controller = new AbortController();
      pendingReregistrations?.set(childId, controller);
      let admission: { granted: boolean; rejection?: string };
      try {
        admission = await (pendingReregistrations
          ? manager.acquireAgent(childId, workKind, controller.signal)
          : manager.acquireAgent(childId, workKind));
      } catch (error) {
        if (!stillHeld() || controller.signal.aborted) break;
        if (
          error instanceof Error
          && error.message.startsWith("pi-famulus request timed out: acquire_agent")
          && manager.isAvailable()
        ) {
          if (attempt + 1 < MAX_REREGISTER_ATTEMPTS) await waitForRetry(wait, controller.signal);
          continue;
        }
        throw error;
      } finally {
        if (pendingReregistrations?.get(childId) === controller) pendingReregistrations.delete(childId);
      }
      if (!stillHeld()) {
        // The child may have settled after the grant was made but before the
        // acquire response reached this continuation.
        if (admission.granted) void manager.releaseAgent(childId).catch(() => {});
        break;
      }
      if (admission.granted) {
        leases.set(childId, workKind);
        pendingLeases?.delete(childId);
        break;
      }
      if (attempt + 1 < MAX_REREGISTER_ATTEMPTS) await waitForRetry(wait, controller.signal);
    }
  }));
}
