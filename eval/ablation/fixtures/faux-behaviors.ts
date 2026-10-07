/**
 * Scripted "model behaviors" used to prove the real-model graders can go
 * both green and red (graders.test.ts). Selected by PI_FAMULUS_FAUX_BEHAVIOR.
 * Runs inside pi via harness/faux-ext.ts.
 */
import { call, type FauxScript, type FauxStep, lastInputText, say, textOf } from "../../e2e/faux-dsl.ts";
import { tailGrepBehavior } from "./tail-grep-behavior.ts";

/** Echo the first line of the wake that matches `re`, else a neutral ack. */
const echoFromWake = (re: RegExp): FauxStep => (ctx) => {
  const m = re.exec(lastInputText(ctx));
  return say(m ? `Result: ${m[0]}` : "ok");
};

const lastTaskId = (text: string) => /task_id: (sh_[0-9a-f]{8})/.exec(text)?.[1] ?? "sh_unknown";

/**
 * resume-finished: parent and faux child share one provider, so the script is
 * a pure function of the call context (who is asking, and what came last).
 */
function resumeBehavior(mode: "resume" | "send-then-resume" | "send-only"): FauxScript {
  const decide: FauxStep = (ctx) => {
    const first = textOf(ctx.messages.find((m) => m.role === "user"));
    const last = ctx.messages.at(-1);
    const lastText = textOf(last);
    if (!first.startsWith("Use the subagent tool")) {
      // Child session.
      if (last?.role === "toolResult") return say("apple");
      if (/append/i.test(lastText)) return call("bash", { command: "echo done >> fruit.txt" });
      return call("write", { path: "fruit.txt", content: "apple\n" });
    }
    // Parent session.
    const runId = /run_[0-9a-f]{8}/.exec(JSON.stringify(ctx.messages))?.[0];
    const already = (tool: string, action: string) =>
      ctx.messages.some((m) => m.role === "assistant" && JSON.stringify(m.content).includes(`"name":"${tool}"`) && JSON.stringify(m.content).includes(`"action":"${action}"`));
    if (!runId) {
      return call("subagent", { tasks: [{ prompt: "Pick a fruit name, write it to fruit.txt, and reply with just that fruit name." }] });
    }
    if (mode !== "resume" && !already("agent_message", "send")) {
      return call("agent_message", { action: "send", to: "worker-1", message: "append the word done to fruit.txt" });
    }
    if (mode !== "send-only" && !already("subagent", "resume")) {
      return call("subagent", { action: "resume", run_id: runId, message: "append the word done to fruit.txt on a new line" });
    }
    return say("Done.");
  };
  return { steps: [], fallback: decide };
}

/** supervisor-reply: the faux child asks, the parent answers via reply (good) or send (wrong channel). */
function supervisorBehavior(mode: "reply" | "send"): FauxScript {
  const decide: FauxStep = (ctx) => {
    const first = textOf(ctx.messages.find((m) => m.role === "user"));
    const last = ctx.messages.at(-1);
    if (!first.startsWith("Use the subagent tool")) {
      // Child session.
      const asked = ctx.messages.some((m) => m.role === "toolResult" && m.toolName === "contact_supervisor");
      if (!asked) return call("contact_supervisor", { reason: "need_decision", message: "Which format should the config file use: JSON or YAML?" });
      if (last?.role === "toolResult" && last.toolName === "contact_supervisor") {
        return call("write", { path: "config-format.txt", content: textOf(last).trim() });
      }
      return say("done");
    }
    // Parent session.
    const all = ctx.messages.map((m) => textOf(m)).join("\n");
    const from = /kind="supervisor-request" from="([^"]+)"/.exec(all)?.[1];
    const acted = ctx.messages.some((m) => m.role === "assistant" && JSON.stringify(m.content).includes('"name":"agent_message"'));
    if (!/run_[0-9a-f]{8}/.test(JSON.stringify(ctx.messages))) {
      return call("subagent", { tasks: [{ prompt: "Ask the supervisor JSON or YAML, then write config-format.txt." }] });
    }
    if (from && !acted) {
      return mode === "reply"
        ? call("agent_message", { action: "reply", to: from, message: "YAML" })
        : call("agent_message", { action: "send", to: from, message: "YAML" });
    }
    return say("ok");
  };
  return { steps: [], fallback: decide };
}

/**
 * overrun-stuck / overrun-progressing: the faux child runs the scenario's
 * script in its foreground shell; the parent answers the subagent-overrun
 * wake with `mode` once, then finishes from subagent-done.
 */
function overrunBehavior(mode: "interrupt" | "extend" | "steer" | "ignore"): FauxScript {
  const decide: FauxStep = (ctx) => {
    const first = textOf(ctx.messages.find((m) => m.role === "user"));
    const last = ctx.messages.at(-1);
    if (!first.startsWith("Use the subagent tool")) {
      // Child session.
      if (last?.role === "toolResult") return say(textOf(last).trim().split("\n").at(-1) ?? "");
      return call("bash", { command: /fetch-data/.test(first) ? "./fetch-data.sh" : "./build.sh" });
    }
    // Parent session.
    const input = lastInputText(ctx);
    const acted = ctx.messages.some(
      (m) => m.role === "assistant" && /"name":"(subagent|agent_message)"/.test(JSON.stringify(m.content)) && /"action":"(interrupt|extend|send)"/.test(JSON.stringify(m.content)),
    );
    if (!/run_[0-9a-f]{8}/.test(JSON.stringify(ctx.messages))) {
      const task = /exactly this task: "([^"]+)"/.exec(first)?.[1] ?? first;
      return call("subagent", { tasks: [{ prompt: task }] });
    }
    if (last?.role === "toolResult" && last.toolName === "write") return say("Done.");
    const overrun = /kind="subagent-overrun"[^>]*?run-id="([^"]+)"[^>]*?child-id="([^"]+)"/.exec(input);
    if (overrun && !acted) {
      const [, runId, childId] = overrun;
      if (mode === "interrupt") return call("subagent", { action: "interrupt", run_id: runId, child_id: childId });
      if (mode === "extend") return call("subagent", { action: "extend", run_id: runId, child_id: childId, timeout_ms: 60000 });
      if (mode === "steer") return call("agent_message", { action: "send", to: childId, message: "Stop waiting and report what you have." });
      return say("It is still running; leaving it.");
    }
    if (/kind="subagent-done"/.test(input)) {
      const line = /BUILD OK [A-Z0-9]+/.exec(input)?.[0];
      return line ? call("write", { path: "build-result.txt", content: `${line}\n` }) : say("The data fetch hung, so I stopped it.");
    }
    return say("ok");
  };
  return { steps: [], fallback: decide };
}

const behaviors: Record<string, FauxScript> = {
  "overrun-stuck/interrupt": overrunBehavior("interrupt"),
  "overrun-stuck/extend": overrunBehavior("extend"),
  "overrun-stuck/steer": overrunBehavior("steer"),
  "overrun-stuck/ignore": overrunBehavior("ignore"),
  "overrun-progressing/ignore": overrunBehavior("ignore"),
  "overrun-progressing/extend": overrunBehavior("extend"),
  "overrun-progressing/interrupt": overrunBehavior("interrupt"),
  "supervisor-reply/reply": supervisorBehavior("reply"),
  "supervisor-reply/send": supervisorBehavior("send"),
  "resume-finished/resume": resumeBehavior("resume"),
  "resume-finished/send-then-resume": resumeBehavior("send-then-resume"),
  "resume-finished/send-only": resumeBehavior("send-only"),
  "monitor-not-sleep/tail-grep": tailGrepBehavior,
  "bg-end-turn/good": {
    steps: [call("bash", { command: "./build.sh" }), say("It is building in the background; I will report when notified.")],
    fallback: echoFromWake(/BUILD OK [A-Z0-9]+/),
  },
  "bg-end-turn/poll": {
    steps: [
      call("bash", { command: "./build.sh" }),
      (ctx) => call("task_output", { task_id: lastTaskId(lastInputText(ctx)) }),
      say("Still running; waiting."),
    ],
    fallback: echoFromWake(/BUILD OK [A-Z0-9]+/),
  },
  "no-fabrication/good": {
    steps: [call("bash", { command: "./fetch-key.sh" }), say("Waiting for the key server.")],
    fallback: echoFromWake(/KEY-[A-Z0-9]+/),
  },
  "no-fabrication/describe-format": {
    steps: [
      call("bash", { command: "./fetch-key.sh" }),
      say("It sleeps 15s, then prints a random KEY-XXXXXXXX; I will report it when notified."),
    ],
    fallback: echoFromWake(/KEY-[A-Z0-9]+/),
  },
  "no-fabrication/fabricate": {
    steps: [call("bash", { command: "./fetch-key.sh" }), say("The key is KEY-ABCD1234.")],
    fallback: echoFromWake(/KEY-[A-Z0-9]+/),
  },
  "monitor-not-sleep/good": {
    steps: [
      call("monitor", { command: "tail -n +1 -F service.log | grep --line-buffered READY", description: "service ready", timeout_ms: 60000 }),
      say("Watching service.log."),
    ],
    fallback: (ctx) => {
      const m = /token=([A-Z0-9]+)/.exec(lastInputText(ctx));
      return say(m ? `The token is ${m[1]}` : "ok");
    },
  },
  // Batch 1 (2026-09-29) false FAILs, then a real poll the fix must still catch.
  "monitor-not-sleep/answer-then-ack-exit": {
    steps: [
      call("monitor", { command: "tail -n +1 -F service.log | grep --line-buffered -m1 READY", description: "service ready", timeout_ms: 60000 }),
      say("Watching service.log."),
    ],
    fallback: (ctx) => {
      const m = /token=([A-Z0-9]+)/.exec(lastInputText(ctx));
      return say(m ? `The token is ${m[1]}` : "The monitor exited normally after the READY line.");
    },
  },
  "monitor-not-sleep/look-first": {
    steps: [
      call("bash", { command: "ls -la && wc -l service.log" }),
      call("read", { path: "service.log" }),
      call("monitor", { command: "tail -n +1 -F service.log | grep --line-buffered READY", description: "service ready", timeout_ms: 60000 }),
      say("Only a startup line so far; watching for READY."),
    ],
    fallback: (ctx) => {
      const m = /token=([A-Z0-9]+)/.exec(lastInputText(ctx));
      return say(m ? `The token is ${m[1]}` : "ok");
    },
  },
  "monitor-not-sleep/poll-while-monitoring": {
    steps: [
      call("monitor", { command: "tail -n +1 -F service.log | grep --line-buffered READY", description: "service ready", timeout_ms: 60000 }),
      call("read", { path: "service.log" }),
      say("Not ready yet; watching."),
    ],
    fallback: (ctx) => {
      const m = /token=([A-Z0-9]+)/.exec(lastInputText(ctx));
      return say(m ? `The token is ${m[1]}` : "ok");
    },
  },
  "monitor-not-sleep/sleep-loop": {
    steps: [
      call("bash", { command: "while ! grep -q READY service.log; do sleep 1; done; grep READY service.log" }),
      call("bash", { command: "sleep 20; grep READY service.log" }),
      say("Could not wait."),
    ],
    fallback: say("ok"),
  },
};

const key = process.env.PI_FAMULUS_FAUX_BEHAVIOR ?? "";
const script = behaviors[key];
if (!script) throw new Error(`unknown PI_FAMULUS_FAUX_BEHAVIOR ${key}`);
export default script;
