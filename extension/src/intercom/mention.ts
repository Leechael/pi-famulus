/**
 * @name editor completion: suggest live sessions, insert a
 * `<RequestSendMessage to=… host=… />` marker.
 *
 * Upstream baseline: pi-famulus @ 98183fb, plugin/pi — verbatim port of
 * the plugin's autocomplete provider.
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
};

/** @file / @. / @~ stay file completion; bare @name is an agent mention. */
export function extractAgentMention(textBeforeCursor: string): { prefix: string; query: string } | undefined {
  const match = textBeforeCursor.match(/(?:^|[ \t])@([^\s]*)$/);
  if (!match) return undefined;
  const query = match[1];
  if (query.startsWith('"') || query.startsWith(".") || query.startsWith("~") || query.includes("/")) {
    return undefined;
  }
  return { prefix: `@${query}`, query };
}

function xmlAttr(s: string): string {
  return s.replace(/["<>]/g, "");
}

export function filterAgentItems(
  rows: SessionRow[],
  query: string,
  self: { id: string; name: string },
): AutocompleteItem[] {
  const candidates: { row: SessionRow; label: string }[] = [];
  for (const row of rows) {
    const id = (row.id ?? "").trim();
    const name = (row.name ?? "").trim();
    if (!id && !name) continue;
    if ((id && id === self.id) || (name && name === self.name)) continue;
    candidates.push({ row, label: name || id });
  }
  const matched = fuzzyFilter(candidates, query, (c) => `${c.row.name ?? ""} ${c.row.id ?? ""}`);
  const items: AutocompleteItem[] = [];
  for (const { row, label } of matched) {
    const description = [row.harness, row.status, hostOf(row)].filter(Boolean).join(" ");
    const host = hostOf(row);
    items.push({
      // Insert a self-describing marker so the model sends to the exact
      // session id; the name alone is ambiguous across duplicate names.
      // The host is written next to it so the human reading their own
      // message can tell which machine the opaque id belongs to.
      value: host
        ? `<RequestSendMessage to="${row.id ?? label}" host="${xmlAttr(host)}" />`
        : `<RequestSendMessage to="${row.id ?? label}" />`,
      label,
      ...(description && { description }),
    });
    if (items.length >= 20) break;
  }
  return items;
}
