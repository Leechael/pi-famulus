/**
 * @name editor completion: suggest live sessions and insert a readable
 * @name@machine without turning a mention into a forwarding command.
 *
 * Origin: pi-famulus @ 98183fb, plugin/pi autocomplete provider.
 */

import type { SessionRow } from "./types";
import { hostOf } from "./types";
import { fuzzyFilter } from "./fuzzy";

export type AutocompleteItem = {
  value: string;
  label: string;
  description?: string;
};

export type AutocompleteProvider = {
  getSuggestions: (
    lines: string[],
    cursorLine: number,
    cursorCol: number,
    options: { signal: AbortSignal; force?: boolean },
  ) => Promise<{ items: AutocompleteItem[]; prefix: string } | null>;
  applyCompletion: (
    lines: string[],
    cursorLine: number,
    cursorCol: number,
    item: AutocompleteItem,
    prefix: string,
  ) => { lines: string[]; cursorLine: number; cursorCol: number };
  shouldTriggerFileCompletion?: (lines: string[], cursorLine: number, cursorCol: number) => boolean;
  triggerCharacters?: string[];
};

/** Paths stay file completion; quoted peer names can contain spaces. */
export function extractAgentMention(textBeforeCursor: string): { prefix: string; query: string } | undefined {
  const match = textBeforeCursor.match(/(?:^|[ \t([{，。！？；：、])@("(?:[^"\\]|\\.)*"?|[^\s"]*)$/u);
  if (!match) return undefined;
  const raw = match[1];
  let query = raw;
  if (raw.startsWith('"')) {
    try { query = JSON.parse(raw.length > 1 && raw.endsWith('"') ? raw : `${raw}"`); }
    catch { return undefined; }
  }
  if (query.startsWith(".") || query.startsWith("~") || query.includes("/")) return undefined;
  return { prefix: `@${raw}`, query };
}

/** A human address is not an identity: duplicate names must still be resolved. */
export function sessionAddress(row: SessionRow): string {
  const name = (row.name || row.id || "").trim();
  const host = hostOf(row);
  return host ? `${name}@${host}` : name;
}

/** Keep control/bidi bytes out of terminal rows without changing address identity. */
export function displayText(value: string): string {
  return value.replace(/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g,
    (char) => `\\u${char.charCodeAt(0).toString(16).padStart(4, "0")}`);
}

function mentionText(address: string): string {
  return /^[\p{L}\p{N}\p{M}\u200C\u200D_.@:-]+$/u.test(address)
    ? `@${address}`
    : `@${displayText(JSON.stringify(address))}`;
}

/** Producer metadata update time, not a uniform last-active or heartbeat field. */
export function updatedLabel(row: SessionRow, now: number): string | undefined {
  const stamp = row.updated_at;
  if (typeof stamp !== "number" || !Number.isFinite(stamp) || stamp <= 0 || stamp > now) return;
  const seconds = Math.floor((now - stamp) / 1000);
  if (seconds < 60) return "updated just now";
  if (seconds < 3600) return `updated ${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `updated ${Math.floor(seconds / 3600)}h ago`;
  return `updated ${Math.floor(seconds / 86400)}d ago`;
}

export function filterAgentItems(
  rows: SessionRow[],
  query: string,
  self: { id: string; name: string },
  now = Date.now(),
): AutocompleteItem[] {
  // Cross-session peers do not displace file suggestions on a bare @.
  if (!query.trim()) return [];
  const candidates: SessionRow[] = [];
  for (const row of rows) {
    const id = (row.id ?? "").trim();
    const name = (row.name ?? "").trim();
    if (!id && !name) continue;
    if (id && id === self.id) continue;
    candidates.push(row);
  }
  const ownMachine = rows.find((row) => row.id === self.id)?.machine_id;
  const isLocal = (row: SessionRow) => !row.machine_id || (ownMachine !== undefined && row.machine_id === ownMachine);
  candidates.sort((a, b) => {
    const localA = isLocal(a) ? 1 : 0;
    const localB = isLocal(b) ? 1 : 0;
    return localB - localA || (b.updated_at || 0) - (a.updated_at || 0);
  });
  const matched = fuzzyFilter(candidates, query, (row) =>
    `${sessionAddress(row)} ${row.id ?? ""} ${row.machine_id ?? ""} ${row.cwd ?? ""}`,
  );
  const items: AutocompleteItem[] = [];
  for (const row of matched) {
    const description = displayText([row.harness, row.status, hostOf(row), updatedLabel(row, now), row.id].filter(Boolean).join(" "));
    const value = mentionText(sessionAddress(row));
    items.push({
      value,
      label: value,
      ...(description && { description }),
    });
    if (items.length >= 20) break;
  }
  return items;
}
