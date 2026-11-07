/**
 * Swap the pi-famulus texts inside a recorded session's system prompt.
 *
 * A decision-point replay reuses the session's own system prompt, so it shows
 * the model the pi-famulus wording of the version the session ran, while the
 * tool definitions come from the extension under test. To compare two
 * wordings, both arms need the session's prompt with only the pi-famulus parts
 * replaced: the bash, monitor and subagent rule lines in the `rules` section
 * and the whole `pi-famulus` section. Everything else (project context,
 * skills, other extensions' rules) stays as recorded.
 *
 * `from` is the prompts module of the version that wrote the session, `to`
 * the one under test. A line that `from` owns but the session does not
 * contain throws: a silent miss would leave the old wording in place and make
 * both arms identical.
 */

export interface PromptsModule {
  PROMPTS: Record<string, unknown>;
}

const RULE_OWNERS = ["tools.bash.rules", "tools.monitor.rules", "tools.subagent.rules"] as const;

function rendered(mod: PromptsModule): { rules: Record<string, string[]>; guidelines: string } {
  const p = mod.PROMPTS as Record<string, string | string[]>;
  const monitorIdle = p["guidelines.monitor-idle"] as string;
  const fill = (s: string) => s.replaceAll("{{monitorIdle}}", monitorIdle);
  const rules: Record<string, string[]> = {};
  for (const id of RULE_OWNERS) rules[id] = ((p[id] as string[] | undefined) ?? []).map(fill);
  return { rules, guidelines: fill(p["guidelines.parent"] as string) };
}

export function spliceSections(
  sections: Record<string, string>,
  from: PromptsModule,
  to: PromptsModule,
): Record<string, string> {
  const old = rendered(from);
  const neu = rendered(to);
  const out = { ...sections };

  if (typeof out.rules !== "string") throw new Error("session prompt has no rules section");
  let lines = out.rules.split("\n");
  for (const id of RULE_OWNERS) {
    const owned = new Set(old.rules[id].map((r) => `- ${r}`));
    if (owned.size === 0) continue;
    const at = lines.findIndex((l) => owned.has(l));
    const found = lines.filter((l) => owned.has(l)).length;
    if (at < 0 || found !== owned.size) {
      throw new Error(`${id}: ${found}/${owned.size} of the old rule lines are in the session's rules section`);
    }
    const replacement = neu.rules[id].map((r) => `- ${r}`);
    lines = [...lines.slice(0, at).filter((l) => !owned.has(l)), ...replacement, ...lines.slice(at).filter((l) => !owned.has(l))];
  }
  out.rules = lines.join("\n");

  const expectedOld = `<pi-famulus>\n${old.guidelines}\n</pi-famulus>`;
  if (out["pi-famulus"] !== expectedOld) throw new Error("session's pi-famulus section is not the `from` version's");
  out["pi-famulus"] = `<pi-famulus>\n${neu.guidelines}\n</pi-famulus>`;
  return out;
}
