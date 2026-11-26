/**
 * Decision-point replay of a real pi session (driven by replay/run-replay.ts).
 * REPLAY_SESSION=<jsonl> REPLAY_UPTO=<entry index, exclusive>
 * REPLAY_PROMPTS_FROM / REPLAY_PROMPTS_TO=<prompts.generated.ts>: swap the
 * pi-famulus texts of the session's prompt for another version's
 * (replay/prompt-splice.ts). REPLAY_DUMP=<file>: write the forced system
 * prompt and the system part of each provider payload, to check what the
 * model saw.
 * The run's context is replaced by the session's messages before UPTO plus
 * whatever this run adds; the system prompt is the session's own. Tool calls
 * never reach the outside: bash runs `true`, read reads an empty file, other
 * tools are blocked.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { spliceSections } from "../replay/prompt-splice.ts";

const KICK = "eval-replay-kick";

export default function replay(pi: ExtensionAPI): void {
  const file = process.env.REPLAY_SESSION!;
  const upto = Number(process.env.REPLAY_UPTO);
  const entries = readFileSync(file, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  let sections: Record<string, string> | undefined;
  const history: any[] = [];
  for (const e of entries.slice(0, upto)) {
    if (e.type === "message") {
      const m = e.message;
      if (m.role === "system") {
        sections = m.sections ?? {};
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
  const dump = process.env.REPLAY_DUMP;
  const dumped: unknown[] = [];
  const record = (entry: unknown) => {
    if (!dump) return;
    dumped.push(entry);
    writeFileSync(dump, JSON.stringify(dumped, null, 2));
  };
  if (sections) {
    const recorded = sections;
    pi.on("before_agent_start", async () => {
      let use = recorded;
      const from = process.env.REPLAY_PROMPTS_FROM;
      const to = process.env.REPLAY_PROMPTS_TO;
      if (from || to) {
        if (!from || !to) throw new Error("REPLAY_PROMPTS_FROM and REPLAY_PROMPTS_TO go together");
        use = spliceSections(recorded, await import(from), await import(to));
      }
      const systemPrompt = Object.values(use).join("\n\n");
      record({ systemPrompt });
      return { systemPrompt };
    });
  }
  if (dump) {
    // The provider payload is what the model saw; earlier hooks still see the
    // recorded system messages that pi later replaces with the forced prompt.
    pi.on("before_provider_request", (event) => {
      const payload = event.payload as { messages?: { role?: string; content?: unknown }[]; system?: unknown };
      const head = payload.messages?.filter((m) => m.role === "system" || m.role === "developer") ?? [];
      const tools = (payload as { tools?: { function?: { name?: string; description?: string; parameters?: unknown }; name?: string; description?: string }[] }).tools ?? [];
      const monitor = tools.find((t) => (t.function?.name ?? t.name) === "monitor");
      record({ providerSystem: head.length ? head.map((m) => m.content) : payload.system ?? null, monitorTool: monitor ?? null, toolNames: tools.map((t) => t.function?.name ?? t.name) });
      return undefined;
    });
  }
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
