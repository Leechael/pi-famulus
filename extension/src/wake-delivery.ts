/**
 * The moment a wake enters the model's context (design.md §4.5).
 *
 * NotifyCenter hands a wake to pi.sendMessage; pi may hold it for minutes
 * (a steer waits for the current tool calls and turn to end). pi emits the
 * extension `message_end` event for a custom message right before it is
 * pushed into the agent's context: in agent-core's loop for steered
 * messages, and in the prompt for a triggerTurn message. That is where the
 * real delivery is logged (`wake.inject`).
 *
 * Passive wakes (`triggerTurn: false`) never reach extension `message_end`:
 * pi appends them through a path that only notifies its own listeners. The
 * NotifyCenter sends them only when the agent is idle, so they are appended
 * at send time and `wake.deliver mode=passive` is the proxy for them.
 *
 * Zero pi dependency: the handler takes the message object pi passes.
 */
import {
  FAMULUS_WAKE_CUSTOM_TYPE,
  renderWakeXml,
  wakeIds,
  wakeRootIndex,
  type FamulusWake,
  type WakeChildStatus,
} from "./wake";
import type { RunRegistry } from "./subagent/registry";

export interface WakeDeliveryDeps {
  now: () => number;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
  /** Current subagent statuses; null/absent when no registry is live. */
  lookup?: () => WakeStatusLookup | null;
}

interface CustomMessageLike {
  role?: string;
  customType?: string;
  content?: unknown;
  details?: unknown;
}

/** The wake's details when `message` is a pi-famulus-wake custom message. */
export function famulusWakeDetails(message: unknown): FamulusWake | undefined {
  const m = message as CustomMessageLike | null | undefined;
  if (!m || m.role !== "custom" || m.customType !== FAMULUS_WAKE_CUSTOM_TYPE) return undefined;
  const details = m.details as FamulusWake | undefined;
  return details && typeof details === "object" && typeof details.kind === "string" ? details : undefined;
}

/** Current statuses, read from the subagent registry at injection. */
export interface WakeStatusLookup {
  childStatus: (runId: string, childId: string) => WakeChildStatus | undefined;
  runStatus: (runId: string) => string | undefined;
}

/** Lookup over the run records of a subagent registry. */
export function registryStatusLookup(registry: Pick<RunRegistry, "get">): WakeStatusLookup {
  return {
    childStatus: (runId, childId) => registry.get(runId)?.children.find((c) => c.childId === childId)?.status,
    runStatus: (runId) => registry.get(runId)?.status,
  };
}

/**
 * Subagent wakes are snapshots. Re-check each child's status against the
 * registry: a changed child shows its current status, with the snapshot's
 * status kept as statusAsOf (rendered status-as-of, plus a
 * <changed-since-as-of> line). Policy: mark, never drop: the snapshot's
 * results stay useful (design.md §4.5).
 */
function recheckStatuses(details: FamulusWake, lookup: WakeStatusLookup): FamulusWake {
  if (details.kind === "subagent-handover") {
    const now = lookup.childStatus(details.runId, details.childId);
    if (now === undefined || now === details.status) return details;
    return { ...details, status: now, statusAsOf: details.status };
  }
  if (details.kind === "subagent-done") {
    let changed = false;
    const children = details.children.map((child) => {
      const now = lookup.childStatus(details.runId, child.childId);
      if (now === undefined || now === child.status) return child;
      changed = true;
      return { ...child, status: now, statusAsOf: child.status };
    });
    if (!changed) return details;
    const runNow = lookup.runStatus(details.runId);
    return { ...details, children, ...(runNow && runNow !== details.status ? { runStatusNow: runNow } : {}) };
  }
  return details;
}

/**
 * The wake as the model should see it on entering the context: the root
 * gains age-ms (how old its snapshot is now), and subagent statuses are
 * re-checked when a lookup is given. Returns undefined when nothing
 * changes: not a wake, not stamped, or content that is not the canonical
 * rendering of its details (never rewrite text this module did not make).
 */
export function wakeAtInjection<M extends CustomMessageLike>(
  message: M,
  now: number,
  lookup?: WakeStatusLookup,
): M | undefined {
  const details = famulusWakeDetails(message);
  if (!details || details.asOf === undefined || typeof message.content !== "string") return undefined;
  const root = wakeRootIndex(message.content);
  if (root < 0 || message.content.slice(root) !== renderWakeXml(details)) return undefined;
  let next: FamulusWake = { ...details, ageMs: Math.max(0, now - details.asOf) };
  if (lookup) next = recheckStatuses(next, lookup);
  return { ...message, content: message.content.slice(0, root) + renderWakeXml(next), details: next };
}

/**
 * pi `message_end` handler body: log the injection and return the
 * replacement message (same role), if any.
 */
export function onWakeMessageEnd<M extends CustomMessageLike>(message: M, deps: WakeDeliveryDeps): M | undefined {
  logWakeInjected(message, deps);
  try {
    return wakeAtInjection(message, deps.now(), deps.lookup?.() ?? undefined);
  } catch {
    // Never break pi's message handling over a wake annotation.
    return undefined;
  }
}

/** Log `wake.inject` for a wake entering the context. Non-wake messages are ignored. */
export function logWakeInjected(message: unknown, deps: WakeDeliveryDeps): void {
  const details = famulusWakeDetails(message);
  if (!details) return;
  const now = deps.now();
  deps.logEvent?.("wake.inject", {
    kind: details.kind,
    ids: wakeIds(details),
    ...(details.asOf !== undefined ? { as_of: details.asOf, lag_ms: Math.max(0, now - details.asOf) } : {}),
  });
}
