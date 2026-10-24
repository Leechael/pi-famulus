/**
 * TUI widgets for intercom message cards.
 *
 * Upstream baseline: agent-intercom @ 98183fb, plugin/pi — verbatim port of
 * the plugin's ChatText/ChatCard renderers and text-measurement helpers.
 */

/** Rough display width: CJK ideographs and wide chars count as 2. */
export function stringWidth(s: string): number {
  let w = 0;
  for (const ch of s) {
    const cp = ch.codePointAt(0) ?? 0;
    w += cp >= 0x2e80 ? 2 : 1;
  }
  return w;
}

/** Word-wrap one line, treating ANSI SGR sequences as zero-width. */
export function wrapAnsiLine(line: string, width: number): string[] {
  if (stringWidth(line.replace(/\x1b\[[0-9;]*m/g, "")) <= width) return [line];
  const tokens = line.match(/(\x1b\[[0-9;]*m|\s+|[^\s\x1b]+)/g) ?? [line];
  const lines: string[] = [];
  let cur = "";
  let curWidth = 0;
  const push = () => {
    lines.push(cur.trimEnd());
    cur = "";
    curWidth = 0;
  };
  for (const token of tokens) {
    if (token.startsWith("\x1b")) {
      cur += token;
      continue;
    }
    if (/^\s+$/.test(token)) {
      if (cur && curWidth + 1 <= width) {
        cur += " ";
        curWidth += 1;
      }
      continue;
    }
    let piece = token;
    let pieceWidth = stringWidth(piece);
    if (curWidth > 0 && curWidth + pieceWidth > width) push();
    while (pieceWidth > width) {
      let cut = 0;
      let cutWidth = 0;
      for (const ch of piece) {
        const w = stringWidth(ch);
        if (cutWidth + w > width) break;
        cutWidth += w;
        cut += ch.length;
      }
      cur += piece.slice(0, cut);
      push();
      piece = piece.slice(cut);
      pieceWidth = stringWidth(piece);
    }
    cur += piece;
    curWidth += pieceWidth;
  }
  if (cur.trimEnd()) push();
  return lines.length ? lines : [""];
}

/** Local wall-clock `YYYY-mm-dd HH:ss`. */
export function formatTimestamp(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** Minimal Text component satisfying pi's Component contract; no pi-tui dependency. */
export class ChatText {
  private text: string;
  private paddingX: number;

  constructor(text: string, paddingX = 1) {
    this.text = text;
    this.paddingX = paddingX;
  }

  invalidate(): void {}

  render(width: number): string[] {
    const padding = Math.min(this.paddingX, Math.max(0, Math.floor((width - 1) / 2)));
    const pad = " ".repeat(padding);
    const contentWidth = Math.max(1, width - padding * 2);
    const out: string[] = [];
    for (const line of this.text.split("\n")) {
      for (const wrapped of wrapAnsiLine(line, contentWidth)) {
        out.push(pad + wrapped);
      }
    }
    return out;
  }
}

/** Boxed card with a full-width background, matching pi's custom message look. */
export class ChatCard {
  private header: string;
  private body: string;
  private bg: (text: string) => string;

  constructor(header: string, body: string, bg: (text: string) => string) {
    this.header = header;
    this.body = body;
    this.bg = bg;
  }

  invalidate(): void {}

  render(width: number): string[] {
    const px = 1;
    const contentWidth = Math.max(1, width - px * 2);
    const out: string[] = [this.bg(" ".repeat(width))];
    const padLine = (s: string) => {
      const visible = stringWidth(s.replace(/\x1b\[[0-9;]*m/g, ""));
      return this.bg(" ".repeat(px) + s + " ".repeat(Math.max(0, width - px - visible)));
    };
    for (const line of `${this.header}\n${this.body}`.split("\n")) {
      for (const wrapped of wrapAnsiLine(line, contentWidth)) {
        out.push(padLine(wrapped));
      }
    }
    out.push(this.bg(" ".repeat(width)));
    // One transparent line below the card as extra bottom margin.
    out.push("");
    return out;
  }
}
