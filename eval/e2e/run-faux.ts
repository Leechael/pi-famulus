/** Run one faux-model episode against real pi + real pi-famulus + our extension. */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { FAUX_EXT } from "../lib/paths.ts";
import { PiRpc, type RpcEvent } from "../lib/rpc.ts";
import { createSandbox, type Sandbox, waitManagerReady } from "../lib/sandbox.ts";
import { type Item, itemsFromEvents } from "../lib/transcript.ts";

export const SCRIPTS = join(import.meta.dirname, "scripts");

export interface FauxCall {
  call: number;
  scripted: boolean;
  fallback: boolean;
  messages: Array<Record<string, unknown>>;
}

export interface FauxEpisode {
  items: Item[];
  events: RpcEvent[];
  calls: FauxCall[];
  stderr: string;
  sandbox: Sandbox;
  pi: PiRpc;
  /** Set when the midway phase failed; the episode still ran to completion. */
  midwayError?: string;
}

export interface FauxRunOptions {
  script: string;
  prompt?: string;
  famulusConfig?: Record<string, unknown>;
  /** Extra extensions after the faux harness (e.g. the ablation harness). */
  extensions?: string[];
  env?: Record<string, string>;
  /** Wait for a live manager connection before prompting (default true). */
  warm?: boolean;
  /** Resolve once this holds (in addition to quiet). */
  until?: (items: Item[]) => boolean;
  untilTimeoutMs?: number;
  /** Quiet window after settling. */
  quietMs?: number;
  /** Do something to the environment once `when` holds (before `until`). */
  midway?: { when: (items: Item[]) => boolean; act: (sandbox: Sandbox, pi: PiRpc) => void | Promise<void> };
}

export function readTrace(path: string): FauxCall[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l) as FauxCall);
}

export async function runFaux(opts: FauxRunOptions): Promise<FauxEpisode> {
  const sandbox = createSandbox({ famulusConfig: opts.famulusConfig });
  const pi = new PiRpc({
    cwd: sandbox.cwd,
    env: {
      ...sandbox.env,
      PI_FAMULUS_FAUX_SCRIPT: join(SCRIPTS, opts.script),
      PI_FAMULUS_FAUX_TRACE: sandbox.tracePath,
      ...(opts.env ?? {}),
    },
    model: "faux/faux-1",
    extensions: [FAUX_EXT, ...(opts.extensions ?? [])],
  });
  let midwayError: string | undefined;
  try {
    if (opts.warm !== false) {
      // macOS CI runners take close to 60 s to answer get_state (62-66 s per pi-spawning test on main, 2026-11-07).
      const boot = await pi.ready(120_000);
      await waitManagerReady(sandbox).catch((err: Error) => {
        throw new Error(`${err.message} (pi booted in ${boot}ms; pi stderr: ${pi.stderr.join("").slice(-2000)})`);
      });
    }
    await pi.prompt(opts.prompt ?? "go");
    if (opts.midway) {
      const { when, act } = opts.midway;
      try {
        await pi.waitFor((evs) => when(itemsFromEvents(evs)), 15_000, "midway condition");
        await act(sandbox, pi);
      } catch (err) {
        // Fail in-band: a rejection here would leave the caller without an
        // episode, so its after() hook never cleans up the sandbox (or the
        // orphaned process group the midway act may have created). Keep
        // running the until/quiet phases and report via explain().
        midwayError = err instanceof Error ? (err.stack ?? err.message) : String(err);
      }
    }
    if (opts.until) {
      const until = opts.until;
      await pi.waitFor((evs) => until(itemsFromEvents(evs)), opts.untilTimeoutMs ?? 15_000, "scenario condition").catch(
        () => {}, // assertions report what is missing
      );
    }
    await pi.waitQuiet(opts.quietMs ?? 1500, 20_000);
  } finally {
    await pi.stop();
  }
  return {
    items: itemsFromEvents(pi.events),
    events: pi.events,
    calls: readTrace(sandbox.tracePath),
    stderr: pi.stderr.join(""),
    sandbox,
    pi,
    midwayError,
  };
}
