/**
 * Fake model for hosted children in the real-daemon integration tests.
 * Loaded into `pi --mode rpc` with -e. Registers provider `faux`, model
 * `faux-1`. PI_FAMULUS_TEST_FAUX picks what the model does on each call:
 *
 * - `hang`: never answers. The child stays in its first run.
 * - `bash-hang`: first call runs `bash` on a command that never exits, then
 *   answers "done" once the tool returns.
 *
 * Each call appends its number to PI_FAMULUS_TEST_FAUX_TRACE when set, so a
 * test can tell that the child is inside a model call.
 *
 * Imports only what pi aliases for extensions, plus node built-ins.
 */
import { appendFileSync } from "node:fs";
import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/** Not a bare sleep, so child bash accepts it. Tests match this text. */
const NEVER_EXITS = "tail -f /dev/null";

export default function fauxChild(pi: ExtensionAPI): void {
  const scenario = process.env.PI_FAMULUS_TEST_FAUX ?? "hang";
  const faux = fauxProvider({ provider: "faux", models: [{ id: "faux-1", name: "Faux Model" }] });
  let call = 0;
  const respond = async () => {
    faux.appendResponses([respond as never]);
    const n = call++;
    const trace = process.env.PI_FAMULUS_TEST_FAUX_TRACE;
    if (trace) appendFileSync(trace, `${n}\n`);
    if (scenario === "bash-hang" && n === 0) {
      return fauxAssistantMessage([fauxToolCall("bash", { command: NEVER_EXITS })], { stopReason: "toolUse" });
    }
    if (scenario === "hang") await new Promise<never>(() => {});
    return fauxAssistantMessage("done");
  };
  faux.setResponses([respond as never]);
  pi.registerProvider(faux.provider);
}
