/** Faux-only protocol exercise for the request-local computer-use compatibility extension. */
import { call, say, textOf, type CtxMessage, type FauxScript } from "../faux-dsl.ts";

function called(ctx: { messages: CtxMessage[] }, name: string): boolean {
  const promptIndex = ctx.messages.findLastIndex((message) => message.role === "user" && textOf(message) === "compat-protocol");
  return ctx.messages.slice(promptIndex + 1).some((message) => message.role === "assistant" && Array.isArray(message.content) &&
    message.content.some((block: { type?: string; name?: string }) => block.type === "toolCall" && block.name === name));
}

const script: FauxScript = {
  steps: [],
  fallback(ctx) {
    if (!called(ctx, "observe_ui")) return call("observe_ui", {});
    if (!called(ctx, "wait_for")) return call("wait_for", { stateId: "eval-ui-1", ref: "@e2", text: "Export ready", timeoutMs: 100 });
    if (!called(ctx, "emit_compat_wake")) return call("emit_compat_wake", {});
    return say("faux compatibility context exercise complete");
  },
};

export default script;
