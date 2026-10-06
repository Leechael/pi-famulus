/**
 * Drive a real `pi --mode rpc` subprocess and record its event stream.
 *
 * RPC mode (not print mode) is required: print mode exits as soon as the
 * first agent run settles, so wakes that arrive later (<pi-famulus-wake> task,
 * monitor, subagent-handover…) would never be observed.
 */
import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { EXTENSION_DIR, PI_BIN } from "./paths.ts";

export interface RpcEvent {
  /** ms since the process was spawned */
  t: number;
  seq: number;
  type: string;
  [key: string]: unknown;
}

export interface PiRpcOptions {
  cwd: string;
  env: Record<string, string>;
  /** e.g. "faux/faux-1" or "anthropic/claude-sonnet-4-5:high" */
  model: string;
  /** Extra extensions loaded AFTER ours (faux harness, ablation harness). */
  extensions?: string[];
  /** Isolated SDK fixtures can omit pi-famulus so no manager process is involved. */
  includeExtensionUnderTest?: boolean;
  extraArgs?: string[];
}

export function piArgs(opts: Pick<PiRpcOptions, "model" | "extensions" | "includeExtensionUnderTest" | "extraArgs">, mode: "rpc" | "tui"): string[] {
  const args = [
    // Isolation from the user's setup: no discovered extensions/skills/templates/
    // context files (settings.json `packages` included), no persisted session.
    "-ne",
    "-ns",
    "-np",
    "-nc",
    "--no-session",
    "--offline",
    ...(opts.includeExtensionUnderTest === false ? [] : ["-e", EXTENSION_DIR]),
    ...(opts.extensions ?? []).flatMap((e) => ["-e", e]),
    ...(opts.model ? ["--model", opts.model] : []),
    ...(opts.extraArgs ?? []),
  ];
  return mode === "rpc" ? ["--mode", "rpc", ...args] : args;
}

export class PiRpc {
  readonly events: RpcEvent[] = [];
  readonly stderr: string[] = [];
  private readonly proc: ChildProcessWithoutNullStreams;
  private readonly started = Date.now();
  private buf = "";
  private seq = 0;
  private nextId = 1;
  private readonly waiters = new Set<() => void>();
  private readonly pending = new Map<string, (ev: RpcEvent) => void>();
  exited: Promise<number | null>;

  constructor(opts: PiRpcOptions) {
    this.proc = spawn(PI_BIN, piArgs(opts, "rpc"), {
      cwd: opts.cwd,
      env: { ...process.env, ...opts.env },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.proc.stdout.setEncoding("utf8");
    this.proc.stdout.on("data", (d: string) => this.onData(d));
    this.proc.stderr.setEncoding("utf8");
    this.proc.stderr.on("data", (d: string) => this.stderr.push(d));
    this.exited = new Promise((resolve) => this.proc.on("exit", (code) => resolve(code)));
  }

  /** ms since spawn */
  now(): number {
    return Date.now() - this.started;
  }

  private onData(chunk: string): void {
    this.buf += chunk;
    let i: number;
    // Strict JSONL: split on \n only (U+2028/2029 may appear inside strings).
    while ((i = this.buf.indexOf("\n")) >= 0) {
      let line = this.buf.slice(0, i);
      this.buf = this.buf.slice(i + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);
      if (!line.trim()) continue;
      let parsed: Record<string, unknown>;
      try {
        parsed = JSON.parse(line);
      } catch {
        this.stderr.push(`[non-json stdout] ${line}\n`);
        continue;
      }
      if (parsed.type === "message_update") continue; // token deltas: noise
      const ev = { ...parsed, t: this.now(), seq: this.seq++ } as RpcEvent;
      this.events.push(ev);
      if (ev.type === "response" && typeof ev.id === "string") this.pending.get(ev.id)?.(ev);
      for (const w of [...this.waiters]) w();
    }
  }

  send(command: Record<string, unknown>): Promise<RpcEvent> {
    const id = `c${this.nextId++}`;
    return new Promise((resolve) => {
      this.pending.set(id, (ev) => {
        this.pending.delete(id);
        resolve(ev);
      });
      this.proc.stdin.write(`${JSON.stringify({ ...command, id })}\n`);
    });
  }

  async prompt(message: string): Promise<void> {
    const res = await this.send({ type: "prompt", message });
    if (res.success !== true) throw new Error(`prompt rejected: ${JSON.stringify(res)}`);
  }

  /** Resolve when `pred` holds over the recorded events, or reject on timeout. */
  waitFor(pred: (events: RpcEvent[]) => boolean, timeoutMs: number, what = "condition"): Promise<void> {
    if (pred(this.events)) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const check = () => {
        if (pred(this.events)) {
          done();
          resolve();
        }
      };
      const timer = setTimeout(() => {
        done();
        reject(new Error(`timed out after ${timeoutMs}ms waiting for ${what}`));
      }, timeoutMs);
      const done = () => {
        clearTimeout(timer);
        this.waiters.delete(check);
      };
      this.waiters.add(check);
    });
  }

  /** True when the agent is settled (last lifecycle event is agent_settled). */
  isSettled(): boolean {
    for (let i = this.events.length - 1; i >= 0; i--) {
      const t = this.events[i].type;
      if (t === "agent_settled") return true;
      if (t === "agent_start" || t === "turn_start" || t === "message_start") return false;
    }
    return false;
  }

  /**
   * Wait until the agent is settled and nothing happened for `quietMs`.
   * Resolves `false` if `timeoutMs` elapses first (no throw: graders decide).
   */
  async waitQuiet(quietMs: number, timeoutMs: number): Promise<boolean> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const lastT = this.events.at(-1)?.t ?? 0;
      if (this.isSettled() && this.now() - lastT >= quietMs) return true;
      if (Date.now() >= deadline) return false;
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  /**
   * Resolve once pi answers an RPC request, i.e. it has loaded its
   * extensions and started the session (session_start ran, so the
   * extension's background manager connect is under way). Returns the boot
   * time in ms. Waits that follow (e.g. for the manager) must not also pay
   * for pi's own startup, which is much slower on loaded CI runners.
   */
  async ready(timeoutMs: number): Promise<number> {
    let timer: NodeJS.Timeout | undefined;
    const fail = (why: string) => new Error(`pi not ready: ${why}; stderr: ${this.stderr.join("").slice(-2000)}`);
    try {
      await Promise.race([
        this.send({ type: "get_state" }),
        new Promise((_, reject) => {
          timer = setTimeout(() => reject(fail(`no answer to get_state within ${timeoutMs}ms`)), timeoutMs);
        }),
        this.exited.then((code) => Promise.reject(fail(`exited with ${code}`))),
      ]);
    } finally {
      clearTimeout(timer);
    }
    return this.now();
  }

  async stop(): Promise<void> {
    if (this.proc.exitCode !== null) return;
    this.proc.stdin.end();
    this.proc.kill("SIGTERM");
    const killer = setTimeout(() => this.proc.kill("SIGKILL"), 5000);
    await this.exited;
    clearTimeout(killer);
  }
}
