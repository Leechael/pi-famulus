/**
 * The model gives bash a 10s timeout on a 30s command. Eval batch 4: the manager
 * path returned the partial output as a success, so the model believed the
 * command was still running.
 *
 * The budget is 10s rather than a tight 4s because the deadline covers the
 * shell's start-up too, and Windows' Git Bash can spend several seconds there
 * while every e2e file starts its sandboxes at once (CI run 43761268902: a 4s
 * budget expired before `echo started` ran at all, so the test failed on an
 * empty output rather than on what it measures). The episode disables the wait
 * guard: a 30s sleep would otherwise be refused as an agent waiting, and the
 * 10s deadline needs a workload that outlasts it.
 */
import { call, type FauxScript, say } from "../faux-dsl.ts";

const script: FauxScript = {
  steps: [call("bash", { command: "echo started; sleep 30; echo finished", timeout: 10 }), say("done")],
  fallback: say("ok"),
};
export default script;
