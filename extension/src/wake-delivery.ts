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
import { FAMULUS_WAKE_CUSTOM_TYPE, renderWakeXml, wakeIds, wakeRootIndex, type FamulusWake } from "./wake";

export interface WakeDeliveryDeps {
  now: () => number;
  logEvent?: (type: string, fields?: Record<string, unknown>) => void;
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

/**
 * The wake as the model should see it on entering the context: the root
 * gains age-ms (how old its snapshot is now). Returns undefined when nothing
 * changes: not a wake, not stamped, or content that is not the canonical
 * rendering of its details (never rewrite text this module did not make).
 */
export function wakeAtInjection<M extends CustomMessageLike>(message: M, now: number): M | undefined {
  const details = famulusWakeDetails(message);
  if (!details || details.asOf === undefined || typeof message.content !== "string") return undefined;
  const root = wakeRootIndex(message.content);
  if (root < 0 || message.content.slice(root) !== renderWakeXml(details)) return undefined;
  const next: FamulusWake = { ...details, ageMs: Math.max(0, now - details.asOf) };
  return { ...message, content: message.content.slice(0, root) + renderWakeXml(next), details: next };
}

/**
 * pi `message_end` handler body: log the injection and return the
 * replacement message (same role), if any.
 */
export function onWakeMessageEnd<M extends CustomMessageLike>(message: M, deps: WakeDeliveryDeps): M | undefined {
  logWakeInjected(message, deps);
  try {
    return wakeAtInjection(message, deps.now());
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
