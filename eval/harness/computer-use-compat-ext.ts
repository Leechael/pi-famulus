/** Opt-in compatibility fixture ONLY; never loads the real computer-use extension. */
import { appendFileSync, readFileSync } from "node:fs";
import { randomBytes } from "node:crypto";
import { VERSION, type ContextEvent, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createStubUiTools, FIXTURE_VERSION, WAIT_FOR_SCHEMA_SHA256 } from "../ablation/fixtures/computer-use-0.5.1.ts";

export default function computerUseCompatibility(pi: ExtensionAPI): void {
  // Fail closed: an accidental -e without explicit scenario setup installs no tools.
  if (process.env.PI_FAMULUS_COMPAT_FIXTURE !== FIXTURE_VERSION) return;
  const enabled = process.env.PI_FAMULUS_COMPAT_UI === "1";
  const token = enabled ? randomBytes(12).toString("hex") : "";
  const historyPath = process.env.PI_FAMULUS_COMPAT_HISTORY;
  const auditPath = process.env.PI_FAMULUS_COMPAT_AUDIT;
  if (!historyPath || !auditPath) throw new Error("compat fixture missing scenario setup");
  const history = JSON.parse(readFileSync(historyPath, "utf8")) as ContextEvent["messages"];
  const historyJson = JSON.stringify(history);
  const cachedHistoryJsonChars = historyJson.length;
  const cachedHistoryBytes = Buffer.byteLength(historyJson);
  if (history.some((m) => m.role === "system")) throw new Error("compat history must not replace native system/tool declarations");
  for (const tool of createStubUiTools(enabled, token)) pi.registerTool(tool);

  pi.on("context", (event) => {
    // Request-local prefix, not session entries. Insert exactly once per request,
    // never cumulatively; native system/tool state is restored by pi afterwards.
    return history.length ? { messages: [...history, ...event.messages] } : undefined;
  });
  pi.on("context_with_system", (event, ctx) => {
    // The immutable synthetic prefix is measured once. Confirm and slice that
    // exact prefix so only current messages are serialized on each request.
    if (event.messages.length < history.length || history.some((message, i) => event.messages[i] !== message)) {
      throw new Error("compat context history prefix changed; cannot produce an honest size audit");
    }
    const liveMessages = event.messages.slice(history.length);
    const liveJson = JSON.stringify(liveMessages);
    const liveJsonChars = liveJson.length;
    const contextCharsEstimate = cachedHistoryJsonChars + liveJsonChars;
    appendFileSync(auditPath, `${JSON.stringify({
      contextCharsEstimate, contextBytesEstimate: cachedHistoryBytes + Buffer.byteLength(liveJson),
      contextEstimatedTokens: Math.ceil(contextCharsEstimate / 4), contextMessages: history.length + liveMessages.length,
      contextSizeMethod: "cached JSON chars of immutable fixture history + JSON chars of live messages; estimate, excludes system/tool serialization",
      modelProvider: ctx.model?.provider ?? "unknown", modelId: ctx.model?.id ?? "unknown", modelApi: ctx.model?.api ?? "unknown",
      nodeVersion: process.version, piVersion: VERSION, fixtureVersion: FIXTURE_VERSION,
      waitForSchemaSha256: WAIT_FOR_SCHEMA_SHA256,
      activeToolCount: pi.getActiveTools().length, allToolCount: pi.getAllTools().length,
      activeToolNames: pi.getActiveTools().join(",").slice(0, 4096),
      waitForLoaded: pi.getActiveTools().includes("wait_for"), observeUiLoaded: pi.getActiveTools().includes("observe_ui"),
    })}\n`);
    // Audit only. Keep the native leading system, current tool declarations,
    // and live turn ordering intact; ablation runs after this extension.
  });
}
