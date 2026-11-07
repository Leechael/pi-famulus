/**
 * Decision-point replay of a real pi session (driven by replay/run-replay.ts).
 * REPLAY_SESSION=<jsonl> REPLAY_UPTO=<entry index, exclusive>
 * The run's context is replaced by the session's messages before UPTO plus
 * whatever this run adds; the system prompt is the session's own. Tool calls
 * never reach the outside: bash runs `true`, read reads an empty file, other
 * tools are blocked.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const KICK = "eval-replay-kick";

export default function replay(pi: ExtensionAPI): void {
  const file = process.env.REPLAY_SESSION!;
  const upto = Number(process.env.REPLAY_UPTO);
  const entries = readFileSync(file, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  let system = "";
  const history: any[] = [];
  for (const e of entries.slice(0, upto)) {
    if (e.type === "message") {
      const m = e.message;
      if (m.role === "system") {
        system = Object.values(m.sections ?? {}).join("\n\n");
        continue;
      }
      history.push(m);
    } else if (e.type === "custom_message" && typeof e.content === "string" && e.content.trim()) {
      history.push({ role: "custom", customType: e.customType, content: e.content, display: true, details: e.details, timestamp: Date.parse(e.timestamp) });
    }
  }
  const empty = join(process.cwd(), ".replay-empty");
  writeFileSync(empty, "");

  pi.registerCommand("eval-replay", {
    description: "start the replay run",
    handler: async () => {
      pi.sendMessage({ customType: KICK, content: "(replay start)", display: false, details: {} }, { triggerTurn: true });
    },
  });
  if (system) pi.on("before_agent_start", async () => ({ systemPrompt: system }));
  pi.on("context", (event) => ({
    messages: [...history, ...event.messages.filter((m: any) => !(m.role === "custom" && m.customType === KICK))],
  }));
  pi.on("tool_call", async (event) => {
    const input = event.input as Record<string, unknown>;
    if (event.toolName === "bash") {
      input.command = "true";
      delete input.run_in_background;
      return;
    }
    if (event.toolName === "read") {
      input.path = empty;
      delete input.offset;
      delete input.limit;
      return;
    }
    if (event.toolName === "task_list" || event.toolName === "task_output") return;
    return { block: true, reason: "Not run (replay: tool disabled)." };
  });
}
