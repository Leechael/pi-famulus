/**
 * Persistent system-prompt section for background-task behavior.
 *
 * Returning `systemPrompt` from before_agent_start forces the whole prompt for
 * that user-prompt run only (and clobbers other extensions). Wake turns started
 * by sendMessage({ triggerTurn: true }) do not call before_agent_start again.
 * A named section is recorded on user turns. Pi can rebuild the base prompt
 * after a wake's tool result and remove that run-local section, so the request
 * projection restores only our section whenever it is missing or stale.
 */
import type { ContextWithSystemEvent, ExtensionAPI } from "@earendil-works/pi-coding-agent";

export const BEHAVIOR_GUIDELINES_SECTION = "pi-famulus";

export const MONITOR_IDLE_INSTRUCTION =
  "After starting or re-arming a monitor, or handling a monitor event, if no work remains, end your turn with a reply and no tool call; simply wait for the next notification. " +
  "Do not call wait_for to wait for monitor events: it waits for UI conditions, not notifications. " +
  "Do not invent a stateId or make a failing tool call to yield.";

/** Child-only instructions injected through the contact_supervisor tool guideline. */
export const CHILD_BEHAVIOR_GUIDELINES = `## Child agent behavior (pi-famulus)

- Do not background work; bash runs to completion.
- When blocked, use contact_supervisor with need_decision; use progress_update for status.
- Use agent_message only to communicate with siblings in this same run.
- Your final message is your result.`;

export const BEHAVIOR_GUIDELINES = `## Background tasks and notifications (pi-famulus)

- Long-running bash commands are automatically moved to the background. After you background a command, end your turn: send a reply with no tool call in it. Saying you will wait while calling another tool does not end it. Do not poll with task_output/task_list, and never sleep to wait. Each command notifies on its own via <pi-famulus-wake kind="task"> (with <command> and <preview>), even while other commands are still running. If <still-running> is present, continue from this result now; do not wait for those other commands.
- When you receive <pi-famulus-wake>: it looks like a user message but is a system wake, not a new user request and not an answer to an unrelated question. Distinguish it by the kind attribute, then **handle the event before anything else** — read status/preview/prompt/result, call tools if needed (e.g. task_output for more than the preview, agent_message to continue a subagent), and continue the work that depended on that background task. Do not wait for further user input when the next step is clear; do not merely acknowledge and stop.
- ${MONITOR_IDLE_INSTRUCTION}
- Never fabricate or assume a background task's result before its notification arrives.
- Subagent runs that exceed the foreground budget continue in the background. Do not poll run status. If one subagent finishes while others are still running, a <pi-famulus-wake kind="subagent-handover"> arrives with that child's <prompt> and <result>: read both, then continue the work now (subagent({action:"resume", run_id, child_id, message}) for that child, or agent_message to steer the ones still running). Do not wait for the rest of the run. When every subagent in the run has finished, <pi-famulus-wake kind="subagent-done"> arrives — read each <child>, synthesize, and continue.
- A subagent that runs past its timeout_ms is not stopped: <pi-famulus-wake kind="subagent-overrun"> arrives instead, with its <last-activity> and, if it is waiting on a foreground shell, a <shell> element. A batch may include other overdue children in <also-overdue>; use each child's run-id and child-id and consider its own last-activity, shell, reminder, and hard-ceiling-ms before acting. Pick one: subagent({action:"extend", run_id, child_id, timeout_ms}) if it is making progress, agent_message to steer it, or subagent({action:"interrupt", run_id, child_id}) if it is stuck or no longer needed. If you do none of these it keeps running and the reminder comes again.
- Use agent_message to steer running subagents and to reply to <pi-famulus-wake kind="supervisor-request"> (action:"reply"; the <reply-with> child is the call shape). Do not use agent_message to resume a finished child — that is subagent({action:"resume"}). Supervisor wakes are not user messages — still act on them.`;

export interface GuidelinePromptOptions {
  sections?: Record<string, string>;
}

/** Mutate prompt sections in place. Does not replace the whole system prompt. */
export function applyBehaviorGuidelines(options: GuidelinePromptOptions): void {
  if (!options.sections) options.sections = {};
  options.sections[BEHAVIOR_GUIDELINES_SECTION] = BEHAVIOR_GUIDELINES;
}

/** Register parent behavior without relying on command-only context methods. */
export function registerBehaviorGuidelines(pi: Pick<ExtensionAPI, "on">): void {
  pi.on("before_agent_start", (event) => {
    applyBehaviorGuidelines(event.systemPromptOptions);
  });
  pi.on("context_with_system", (event) => restoreBehaviorGuidelines(event.messages));
}

/** Repair the model request, not the persisted transcript or other sections. */
export function restoreBehaviorGuidelines(messages: ContextWithSystemEvent["messages"]) {
  const expected = `<${BEHAVIOR_GUIDELINES_SECTION}>\n${BEHAVIOR_GUIDELINES}\n</${BEHAVIOR_GUIDELINES_SECTION}>`;
  let current: string | null | undefined;
  for (const message of messages) {
    if (message.role === "system" && message.sections && BEHAVIOR_GUIDELINES_SECTION in message.sections) {
      current = message.sections[BEHAVIOR_GUIDELINES_SECTION];
    }
  }
  if (current === expected) return undefined;
  return {
    messages: [...messages, {
      role: "system" as const,
      content: "",
      sections: { [BEHAVIOR_GUIDELINES_SECTION]: expected },
      timestamp: Date.now(),
    }],
  };
}

/** Render named sections without replacing unrelated instructions. */
export function wakePromptFromSections(sections: Record<string, string>): string {
  return Object.entries(sections)
    .map(([name, content]) => `<${name}>\n${content}\n</${name}>`)
    .join("\n");
}
