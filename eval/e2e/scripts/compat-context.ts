/** Faux-only protocol exercise for the request-local computer-use compatibility extension. */
import { call, say, textOf, type CtxMessage, type FauxScript } from "../faux-dsl.ts";

function promptIndex(ctx: { messages: CtxMessage[] }): number {
  return ctx.messages.findLastIndex((message) => message.role === "user" && textOf(message) === "compat-protocol");
}

function called(ctx: { messages: CtxMessage[] }, name: string, afterIndex: number): boolean {
  if (afterIndex < 0) return false;
  return ctx.messages.slice(afterIndex + 1).some((message) => message.role === "assistant" && Array.isArray(message.content) &&
    message.content.some((block: { type?: string; name?: string }) => block.type === "toolCall" && block.name === name));
}

const script: FauxScript = {
  steps: [],
  fallback(ctx) {
    const livePromptIndex = promptIndex(ctx);
    if (livePromptIndex < 0) return say("SDK cache warmup");
    if (!called(ctx, "observe_ui", livePromptIndex)) return call("observe_ui", {});
    if (!called(ctx, "wait_for", livePromptIndex)) return call("wait_for", { stateId: "eval-ui-1", ref: "@e2", text: "Export ready", timeoutMs: 100 });
    if (!called(ctx, "emit_compat_wake", livePromptIndex)) return call("emit_compat_wake", {});
    return say("faux compatibility context exercise complete");
  },
};

export default script;
