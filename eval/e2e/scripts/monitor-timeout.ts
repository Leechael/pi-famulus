/**
 * (c) A monitor emits three lines (each becomes a <pi-famulus-wake kind="monitor">) and
 * then goes silent until its timeout fires the re-arm notice.
 */
import { call, type FauxScript, lastInputText, say } from "../faux-dsl.ts";

export const MONITOR_COMMAND = "for i in 1 2 3; do echo tick-$i; sleep 0.3; done; sleep 30";

const script: FauxScript = {
  steps: [
    call("monitor", { command: MONITOR_COMMAND, description: "tick watcher", timeout_ms: 8000 }),
    say("Monitor armed; waiting for events."),
  ],
  // Every wake (event batch or timeout) gets a turn; echo what woke us.
  fallback: (ctx) => say(`WOKE: ${lastInputText(ctx).includes("timed out") ? "timeout" : "event"}`),
};
export default script;
