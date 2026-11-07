/**
 * A model that cannot end its turn: it says "完成。" and calls a no-op, again
 * and again (incident 2026-11-07, k3.1-pro). Four scripted no-ops, then a
 * reply, so an unguarded run still ends.
 */
import { call, type FauxScript, say } from "../faux-dsl.ts";

const script: FauxScript = {
  steps: [
    call("bash", { command: "true" }, "完成。"),
    call("task_list", {}, "完成。"),
    call("bash", { command: "date -u +'%H:%M:%S'" }, "完成。"),
    call("bash", { command: "true" }, "完成。"),
    say("Done."),
  ],
};
export default script;
