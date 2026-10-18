/**
 * Drives every model-facing text surface the faux model can reach, so the
 * ablation harness self-test can check each manifest segment against what the
 * model really receives:
 *   system prompt sections + <rules> + tool declarations, backgrounded-bash
 *   result, sleep-block error, task wake (+ <still-running>), monitor wake,
 *   subagent backgrounded result, subagent-handover / subagent-done /
 *   supervisor-request wakes, and the agent_message "finished child" error.
 *
 * Parent and children share the faux provider, so every decision is a pure
 * function of the call context (not a step index).
 */
import { call, calls, type CtxMessage, type FauxCallContext, type FauxScript, say, textOf } from "../faux-dsl.ts";

const PARENT_PROMPT = "go";
const CHILD_ONE = "child one: ask your supervisor which color to use";
const CHILD_TWO = "child two: reply done";

const called = (ctx: FauxCallContext, pred: (name: string, args: Record<string, unknown>) => boolean) =>
  ctx.messages.some(
    (m) =>
      m.role === "assistant" &&
      Array.isArray(m.content) &&
      (m.content as Array<{ type: string; name?: string; arguments?: Record<string, unknown> }>).some(
        (b) => b.type === "toolCall" && pred(String(b.name), b.arguments ?? {}),
      ),
  );

const wakeText = (m: CtxMessage) => (m.role === "user" || m.role === "custom" ? textOf(m) : "");

function child(ctx: FauxCallContext, prompt: string) {
  if (prompt.includes(CHILD_ONE)) {
    if (!called(ctx, (n) => n === "contact_supervisor")) {
      return call("contact_supervisor", { reason: "need_decision", message: "RED or BLUE?" });
    }
    return say("one done");
  }
  return say("two done");
}

function parent(ctx: FauxCallContext) {
  if (!called(ctx, (n) => n === "bash")) {
    return calls([
      ["bash", { command: "echo fast-a; sleep 1; echo done-a" }],
      ["bash", { command: "echo slow-b; sleep 3; echo done-b" }],
    ]);
  }
  if (!called(ctx, (n, a) => n === "bash" && a.command === "sleep 1")) return call("bash", { command: "sleep 1" });
  if (!called(ctx, (n) => n === "monitor")) {
    return call("monitor", { command: "echo mon-line; sleep 30", description: "surface monitor", timeout_ms: 1500 });
  }
  if (!called(ctx, (n) => n === "subagent")) {
    return call("subagent", {
      async: true,
      concurrency: 1,
      tasks: [
        { prompt: CHILD_ONE, name: "one" },
        { prompt: CHILD_TWO, name: "two" },
      ],
    });
  }
  const texts = ctx.messages.map(wakeText);
  const request = texts.map((t) => /kind="supervisor-request" from="([^"]+)"/.exec(t)?.[1]).find(Boolean);
  if (request && !called(ctx, (n, a) => n === "agent_message" && a.action === "reply")) {
    return call("agent_message", { action: "reply", to: request, message: "RED" });
  }
  if (texts.some((t) => t.includes('kind="subagent-done"')) && !called(ctx, (n, a) => n === "agent_message" && a.action === "send")) {
    // Finished child: send starts a new turn instead of erroring.
    return call("agent_message", { action: "send", to: "one", message: "one more thing" });
  }
  return say("ok");
}

const script: FauxScript = {
  steps: [],
  fallback: (ctx) => {
    const first = textOf(ctx.messages.find((m) => m.role === "user"));
    return first === PARENT_PROMPT ? parent(ctx) : child(ctx, first);
  },
};
export default script;
