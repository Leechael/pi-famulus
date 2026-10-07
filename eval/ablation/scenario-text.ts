/**
 * Scenario texts from eval/scenarios/<id>.md (same format as
 * extension/prompts/): "<id>.tests" and "<id>.prompt", with {{name}}
 * placeholders filled by scenarioText().
 */
import { join } from "node:path";
import { parsePrompts } from "../../extension/scripts/build-prompts.ts";

const TEXT: Record<string, string> = Object.fromEntries(
  parsePrompts(join(import.meta.dirname, "..", "scenarios")).prompts.map((p) => [p.id, String(p.value)]),
);

export function scenarioText(id: string, vars: Record<string, string> = {}): string {
  const text = TEXT[id];
  if (text === undefined) throw new Error(`no scenario text ${id} in eval/scenarios/`);
  return text.replace(/\{\{(\w+)\}\}/g, (_, name: string) => {
    if (vars[name] === undefined) throw new Error(`scenario text ${id}: missing {{${name}}}`);
    return vars[name];
  });
}
