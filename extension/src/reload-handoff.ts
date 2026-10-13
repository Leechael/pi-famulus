/**
 * /reload keeps background work alive.
 *
 * pi's /reload emits session_shutdown(reason "reload") on the old extension
 * instance, re-imports every extension (fresh module scope), then emits
 * session_start(reason "reload") on the new instances. Shells, monitors and
 * subagents belong to the pi session, not to one module instance, so the old
 * instance parks its runtime here instead of tearing it down, and the new
 * instance adopts it.
 *
 * The runtime registers its tools, commands and handlers on a PiBinding: a
 * stand-in ExtensionAPI that records registrations and replays them on every
 * pi it is attached to. Other calls go to the attached pi; sendMessage is
 * queued while no live pi can take it (between the old instance going stale
 * and the new one's session_start).
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { realClock, type Clock } from "./clock";

const HANDOFF_KEY = Symbol.for("pi-famulus.reload-handoff");

/** A parked runtime nobody adopts (pi-famulus removed from the config) is torn down after this. */
export const HANDOFF_EXPIRY_MS = 60_000;

const REGISTRATIONS = new Set<PropertyKey>([
  "on",
  "registerTool",
  "registerCommand",
  "registerShortcut",
  "registerFlag",
  "registerMessageRenderer",
  "registerProvider",
]);

export interface PiBinding {
  /** Pass this to the runtime in place of the real pi. */
  readonly api: ExtensionAPI;
  /** Replay every registration on `pi` and route calls to it. */
  attach(pi: ExtensionAPI): void;
  /** Deliver queued messages; call once the attached pi's session has started. */
  ready(): void;
  /** Stop calling the attached pi (it is about to go stale). */
  detach(): void;
}

export function createPiBinding(): PiBinding {
  const registrations: Array<{ method: PropertyKey; args: unknown[] }> = [];
  const queued: unknown[][] = [];
  let current: ExtensionAPI | null = null;
  let live = false;

  const call = (pi: ExtensionAPI, method: PropertyKey, args: unknown[]): unknown =>
    (Reflect.get(pi, method) as (...a: unknown[]) => unknown).apply(pi, args);

  const api = new Proxy({} as ExtensionAPI, {
    get(_target, key) {
      if (REGISTRATIONS.has(key)) {
        return (...args: unknown[]) => {
          registrations.push({ method: key, args });
          return current ? call(current, key, args) : undefined;
        };
      }
      if (key === "sendMessage") {
        return (...args: unknown[]) => {
          if (current && live) return call(current, key, args);
          queued.push(args);
        };
      }
      if (!current) throw new Error(`pi-famulus: pi.${String(key)} used while detached (between reloads)`);
      const value = Reflect.get(current, key) as unknown;
      return typeof value === "function" ? (value as (...a: unknown[]) => unknown).bind(current) : value;
    },
  });

  return {
    api,
    attach(pi) {
      current = pi;
      live = false;
      for (const { method, args } of registrations) call(pi, method, args);
    },
    ready() {
      if (!current) return;
      live = true;
      for (const args of queued.splice(0)) call(current, "sendMessage", args);
    },
    detach() {
      current = null;
      live = false;
    },
  };
}

export interface ParkedRuntime<R> {
  /** Extension version that parked it; another version's runtime is not adopted. */
  version: string;
  runtime: R;
  /** Full teardown (what a non-reload session_shutdown does). */
  dispose(): Promise<void>;
}

interface Slot {
  parked: ParkedRuntime<unknown>;
  expiry: unknown;
}

/** Park a runtime for the next instance; tear it down if nobody takes it in time. */
export function parkRuntime<R>(parked: ParkedRuntime<R>, clock: Clock = realClock): void {
  const store = globalThis as Record<symbol, Slot | undefined>;
  const previous = store[HANDOFF_KEY];
  if (previous) {
    clock.clearTimeout(previous.expiry);
    void previous.parked.dispose().catch(() => {});
  }
  const expiry = clock.setTimeout(() => {
    if (store[HANDOFF_KEY]?.parked !== parked) return;
    store[HANDOFF_KEY] = undefined;
    void parked.dispose().catch(() => {});
  }, HANDOFF_EXPIRY_MS);
  clock.unref?.(expiry);
  store[HANDOFF_KEY] = { parked: parked as ParkedRuntime<unknown>, expiry };
}

/** Take the parked runtime, if any. The caller adopts or disposes it. */
export function takeParkedRuntime<R>(clock: Clock = realClock): ParkedRuntime<R> | null {
  const store = globalThis as Record<symbol, Slot | undefined>;
  const slot = store[HANDOFF_KEY];
  if (!slot) return null;
  store[HANDOFF_KEY] = undefined;
  clock.clearTimeout(slot.expiry);
  return slot.parked as ParkedRuntime<R>;
}
