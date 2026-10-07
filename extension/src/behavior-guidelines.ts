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
// .ts suffix: eval/ loads this module with Node's own TypeScript support.
import { fill, PROMPTS } from "./prompts.generated.ts";

export const BEHAVIOR_GUIDELINES_SECTION = "pi-famulus";

export const MONITOR_IDLE_INSTRUCTION = PROMPTS["guidelines.monitor-idle"];

/** Child-only instructions injected through the contact_supervisor tool guideline. */
export const CHILD_BEHAVIOR_GUIDELINES = PROMPTS["guidelines.child"];

export const BEHAVIOR_GUIDELINES = fill("guidelines.parent", { monitorIdle: MONITOR_IDLE_INSTRUCTION });

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
