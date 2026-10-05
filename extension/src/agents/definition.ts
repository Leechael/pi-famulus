/**
 * Agent definition types and markdown parsing (design doc §4.8, appendix B).
 *
 * Pure module: zero pi dependencies, zero npm dependencies. Agent definitions
 * are markdown files with a hand-parsed YAML-subset frontmatter block:
 *
 *   ---
 *   name: explorer
 *   description: Fast codebase exploration
 *   tools: [read, bash, grep, find, ls]   # or: read, bash, grep, find, ls
 *   model: anthropic:claude-haiku-4-5
 *   thinking: high
 *   ---
 *
 *   Body becomes the system prompt (trimmed; may be empty).
 */

import {
  isValidThinkingLevel,
  VALID_THINKING_LEVELS,
  type ThinkingLevel,
} from "../thinking-levels";

export interface AgentDefinition {
  /** Must match ^[a-z][a-z0-9-]*$ */
  name: string;
  /** Required, non-empty. */
  description: string;
  /** Tool allowlist. Defaults to ["read","bash","edit","write"]. */
  tools: string[];
  /** "provider:id" or bare id. */
  model?: string;
  /** Thinking level (pi core parity; src/thinking-levels.ts). */
  thinking?: ThinkingLevel;
  /** Frontmatter body, trimmed. Empty body → "". */
  systemPrompt: string;
  source: "builtin" | "user" | "project";
  /** Filesystem path; builtins have none. */
  path?: string;
}

export const DEFAULT_AGENT_TOOLS: readonly string[] = ["read", "bash", "edit", "write"];

const NAME_PATTERN = /^[a-z][a-z0-9-]*$/;

type Fail = (reason: string) => never;

function makeFail(source: AgentDefinition["source"], path: string | undefined): Fail {
  return (reason) => {
    const where = path ? ` at ${path}` : ` (${source} definition)`;
    throw new Error(`Invalid agent definition${where}: ${reason}`);
  };
}

/** Strip a trailing ` # comment` outside quotes and brackets (yaml-subset). */
function stripInlineComment(value: string): string {
  let quote: '"' | "'" | null = null;
  let depth = 0;
  for (let i = 0; i < value.length; i++) {
    const ch = value[i];
    if (quote) {
      if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") quote = ch;
    else if (ch === "[") depth++;
    else if (ch === "]") depth = Math.max(0, depth - 1);
    else if (ch === "#" && depth === 0 && (i === 0 || /\s/.test(value[i - 1]))) {
      return value.slice(0, i);
    }
  }
  return value;
}

function unquote(value: string): string {
  if (value.length >= 2) {
    const first = value[0];
    const last = value[value.length - 1];
    if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
      return value.slice(1, -1);
    }
  }
  return value;
}

function parseScalar(raw: string, field: string, fail: Fail): string {
  const value = unquote(stripInlineComment(raw).trim());
  if (value.startsWith("[") || value.endsWith("]")) {
    fail(`field '${field}' must be a plain string, not an array`);
  }
  return value;
}

function parseStringArray(raw: string, field: string, fail: Fail): string[] {
  const value = stripInlineComment(raw).trim();
  // Accept both YAML flow arrays `[a, b, c]` and bare comma lists `a, b, c`
  // (single item `a` included). Half-open brackets are still errors.
  let inner: string;
  if (value.startsWith("[") && value.endsWith("]")) {
    inner = value.slice(1, -1).trim();
  } else if (value.startsWith("[") || value.endsWith("]")) {
    fail(`field '${field}' must use array syntax [a, b, c] or a comma list a, b, c`);
  } else {
    inner = value;
  }
  if (inner === "") return [];
  return inner.split(",").map((item) => {
    const scalar = unquote(item.trim());
    if (scalar === "") fail(`field '${field}' contains an empty array item`);
    if (scalar.startsWith("[") || scalar.startsWith("]")) {
      fail(`field '${field}' has a malformed array item: "${item.trim()}"`);
    }
    return scalar;
  });
}

interface Frontmatter {
  fields: Record<string, string>;
  body: string;
}

function parseFrontmatter(content: string, fail: Fail): Frontmatter {
  const text = content.replace(/\r\n/g, "\n");
  const lines = text.split("\n");
  if ((lines[0] ?? "").trim() !== "---") {
    fail("missing frontmatter block (file must start with '---')");
  }
  let end = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i].trim() === "---") {
      end = i;
      break;
    }
  }
  if (end === -1) {
    fail("unterminated frontmatter block (missing closing '---')");
  }
  const fields: Record<string, string> = {};
  for (const line of lines.slice(1, end)) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#")) continue;
    const match = /^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*(.*)$/.exec(trimmed);
    if (!match) fail(`malformed frontmatter line: "${trimmed}"`);
    fields[match[1]] = match[2];
  }
  return { fields, body: lines.slice(end + 1).join("\n") };
}

/**
 * Parse one agent definition markdown file.
 * Throws Error (message includes path and reason) on any validation failure.
 */
export function parseAgentMarkdown(
  content: string,
  source: AgentDefinition["source"],
  path?: string,
): AgentDefinition {
  const fail = makeFail(source, path);
  const { fields, body } = parseFrontmatter(content, fail);

  if (fields.name === undefined) fail("missing required field 'name'");
  const name = parseScalar(fields.name, "name", fail);
  if (!NAME_PATTERN.test(name)) {
    fail(`invalid name "${name}" (must match ^[a-z][a-z0-9-]*$)`);
  }

  if (fields.description === undefined) fail("missing required field 'description'");
  const description = parseScalar(fields.description, "description", fail);
  if (description === "") fail("field 'description' must be non-empty");

  let tools = [...DEFAULT_AGENT_TOOLS];
  if (fields.tools !== undefined) {
    tools = parseStringArray(fields.tools, "tools", fail);
  }

  let model: string | undefined;
  if (fields.model !== undefined) {
    const value = parseScalar(fields.model, "model", fail);
    if (value === "") fail("field 'model' must be non-empty when present");
    model = value;
  }

  let thinking: ThinkingLevel | undefined;
  if (fields.thinking !== undefined) {
    const value = parseScalar(fields.thinking, "thinking", fail);
    if (isValidThinkingLevel(value)) {
      thinking = value;
    } else {
      fail(
        `invalid thinking level "${value}" (expected one of: ${VALID_THINKING_LEVELS.join(", ")})`,
      );
    }
  }

  return {
    name,
    description,
    tools,
    ...(model !== undefined ? { model } : {}),
    ...(thinking !== undefined ? { thinking } : {}),
    systemPrompt: body.trim(),
    source,
    ...(path !== undefined ? { path } : {}),
  };
}

/**
 * Resolve an agent by name. undefined → the default "worker" definition.
 * Unknown names throw an Error listing all available agent names.
 */
export function resolveAgent(
  defs: AgentDefinition[],
  name: string | undefined,
): AgentDefinition {
  const target = name ?? "worker";
  const found = defs.find((def) => def.name === target);
  if (found) return found;
  const available = defs
    .map((def) => def.name)
    .sort()
    .join(", ");
  const list = available === "" ? "(none)" : available;
  if (name === undefined) {
    throw new Error(`Default agent "worker" is not defined. Available agents: ${list}`);
  }
  throw new Error(`Unknown agent "${name}". Available agents: ${list}`);
}
