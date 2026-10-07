/**
 * Built-in agent definitions (design doc §4.8).
 *
 * These form the lowest precedence layer: user and project definitions with
 * the same name replace them wholesale.
 */
import { PROMPTS } from "../prompts.generated";
import type { AgentDefinition } from "./definition";

export const BUILTIN_AGENTS: AgentDefinition[] = [
  {
    name: "explorer",
    description: PROMPTS["agents.explorer.description"],
    tools: ["read", "grep", "find", "ls", "bash"],
    systemPrompt: PROMPTS["agents.explorer.system-prompt"],
    source: "builtin",
  },
  {
    name: "worker",
    description: PROMPTS["agents.worker.description"],
    tools: ["read", "bash", "edit", "write"],
    systemPrompt: PROMPTS["agents.worker.system-prompt"],
    source: "builtin",
  },
];
