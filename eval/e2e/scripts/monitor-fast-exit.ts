/**
 * A monitor whose command prints once and exits immediately (`echo noop`).
 * Manual testing (2026-09-24) found it stuck "running" until its timeout: the
 * line and the exit arrived before the extension registered the monitor.
 * After the exit wake the model lists tasks; nothing should still be running.
 */
import { call, type FauxScript, say, textOf } from "../faux-dsl.ts";

let listed = false;
const script: FauxScript = {
  steps: [call("monitor", { command: "echo noop", description: "quick watcher", timeout_ms: 15000 })],
  // The exit can land while the turn that started the monitor is still
  // running (steered into it) or after it (a wake of its own), so look at
  // everything the model has seen, not only the latest input.
  fallback: (ctx) => {
    const ended = ctx.messages.some((m) => m.role !== "assistant" && /exited|timed out/.test(textOf(m)));
    if (!listed && ended) {
      listed = true;
      return call("task_list", {});
    }
    return say(listed ? "LISTED" : "Monitor armed.");
  },
};
export default script;
