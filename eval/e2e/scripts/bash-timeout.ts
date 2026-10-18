/**
 * The model gives bash a 1s timeout on a 5s command. Eval batch 4: the manager
 * path returned the partial output as a success, so the model believed the
 * command was still running.
 */
import { call, type FauxScript, say } from "../faux-dsl.ts";

const script: FauxScript = {
  steps: [call("bash", { command: "echo started; sleep 8; echo finished", timeout: 4 }), say("done")],
  fallback: say("ok"),
};
export default script;
