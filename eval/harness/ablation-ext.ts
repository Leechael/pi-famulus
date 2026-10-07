/**
 * Ablation harness extension. Load it AFTER the extension under test:
 *
 *   pi -e extension -e eval/harness/ablation-ext.ts
 *
 * PI_FAMULUS_ABLATE=<variant id>          baseline | segment id | group id (see ablation/manifest.json)
 * PI_FAMULUS_ABLATION_LOG=<path>          JSONL audit of every removal (hook, segment, hits)
 *
 * Hooks used (pi 1.0.0 — verified unchanged from 0.87; eval typechecks
 * against the 1.0.0 SDK):
 *  - before_agent_start: strips segments from the chained system prompt. Our
 *    extension returns a forced systemPrompt there, and pi projects a forced
 *    prompt onto the request after context hooks, so this is the only place
 *    the BEHAVIOR_GUIDELINES text can be removed.
 *  - context_with_system: strips segments from the full per-request
 *    transcript: system sections, tool declarations (descriptions), tool
 *    results, and injected custom messages (wake lead-ins). Non-destructive:
 *    the session log keeps the original text.
 *  - tool_call: mechanism ablation `mech.sleep-block` (rewrites bare sleep /
 *    idle-loop bash commands so the extension's rejection does not fire).
 * Config-kind mechanisms (mech.autobg) are applied by the runner via config.json.
 */
import { appendFileSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  BARE_SLEEP_PATTERNS,
  compileTextSegments,
  loadManifest,
  removeDeep,
  removeSegments,
  resolveVariantList,
} from "../ablation/manifest.ts";

export default function ablationHarness(pi: ExtensionAPI): void {
  const variantId = process.env.PI_FAMULUS_ABLATE ?? "baseline";
  const logPath = process.env.PI_FAMULUS_ABLATION_LOG;
  const variant = resolveVariantList(loadManifest(process.env.PI_FAMULUS_ABLATION_MANIFEST), variantId);
  const segs = compileTextSegments(variant.segments);
  const blockSleep = variant.segments.some((s) => s.id === "mech.sleep-block");

  const log = (hook: string, hits: Map<string, number>) => {
    if (!logPath) return;
    appendFileSync(logPath, `${JSON.stringify({ ts: Date.now(), hook, variant: variantId, hits: Object.fromEntries(hits) })}\n`);
  };
  log("init", new Map(variant.segments.map((s) => [s.id, 0])));

  if (segs.length > 0) {
    // Edit user-turn initialization in place (never force a replacement prompt).
    // Core may clear sections after wake/tool results. Famulus repairs visibility
    // request-locally; our last-registered context hook strips those copies too.
    pi.on("before_agent_start", async (event) => {
      const hits = new Map<string, number>();
      const o = event.systemPromptOptions as {
        sections?: Record<string, string>;
        promptGuidelines?: string[];
        toolGuidelines?: Record<string, string[]>;
      };
      for (const [name, text] of Object.entries(o.sections ?? {})) o.sections![name] = removeSegments(text, segs, hits);
      if (o.promptGuidelines) {
        o.promptGuidelines.splice(0, o.promptGuidelines.length, ...o.promptGuidelines.map((g) => removeSegments(g, segs, hits)).filter((g) => g.trim()));
      }
      for (const [tool, list] of Object.entries(o.toolGuidelines ?? {})) {
        o.toolGuidelines![tool] = list.map((g) => removeSegments(g, segs, hits)).filter((g) => g.trim());
      }
      log("before_agent_start", hits);
    });

    pi.on("context_with_system", async (event) => {
      const hits = new Map<string, number>();
      const messages = removeDeep(event.messages, segs, hits);
      log("context_with_system", hits);
      return messages === event.messages ? undefined : { messages };
    });
  }

  if (blockSleep) {
    pi.on("tool_call", async (event) => {
      if (event.toolName !== "bash") return;
      const input = event.input as { command?: string };
      if (typeof input.command === "string" && BARE_SLEEP_PATTERNS.some((re) => re.test(input.command!))) {
        input.command = `true && ${input.command}`;
        log("tool_call", new Map([["mech.sleep-block", 1]]));
      }
    });
  }
}
