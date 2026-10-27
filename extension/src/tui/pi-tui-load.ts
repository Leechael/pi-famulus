/**
 * Resolve `@earendil-works/pi-tui` from the host pi install.
 * Typed structurally so this package does not depend on pi-tui's declarations.
 */
import * as bundledPiTui from "@earendil-works/pi-tui";

export interface PiTuiText {
  setText(text: string): void;
  invalidate(): void;
}

export interface PiTuiScroll {
  viewportHeight: number;
  scrollBy(lines: number): number;
  scrollToEnd(): void;
  invalidate(): void;
}

export interface PiTuiComponent {
  render(width: number): string[];
  invalidate(): void;
}

export interface PiTuiModule {
  Text: new (text?: string, paddingX?: number, paddingY?: number) => PiTuiText & PiTuiComponent;
  ScrollView: new (
    component: PiTuiComponent,
    options?: {
      primary?: boolean;
      scrollbar?: "hidden" | "auto" | "always";
      follow?: "none" | "end";
      overscroll?: "chain" | "contain";
    },
  ) => PiTuiScroll & PiTuiComponent;
  VStack: new (
    children?: Array<
      | PiTuiComponent
      | {
          component: PiTuiComponent;
          basis?: number | "auto";
          grow?: number;
          shrink?: number;
          minSize?: number;
        }
    >,
    options?: { gap?: number },
  ) => PiTuiComponent;
  matchesKey(data: string, key: string): boolean;
  SettingsList: new (
    items: PiTuiSettingItem[],
    maxVisible: number,
    theme: PiTuiSettingsListTheme,
    onChange: (id: string, newValue: string) => void,
    onCancel: () => void,
  ) => PiTuiInteractive & { updateValue(id: string, newValue: string): void };
  Input: new () => PiTuiInteractive & {
    focused: boolean;
    onSubmit?: (value: string) => void;
    onEscape?: () => void;
    getValue(): string;
    setValue(value: string): void;
  };
  Box: new (
    paddingX?: number,
    paddingY?: number,
    bgFn?: (text: string) => string,
  ) => { addChild(component: unknown): void };
}

export interface PiTuiInteractive extends PiTuiComponent {
  handleInput(data: string): void;
}

export interface PiTuiSettingItem {
  id: string;
  label: string;
  description?: string;
  currentValue: string;
  /** Enter cycles through these. */
  values?: string[];
  /** Enter opens this; `done(value)` sets the shown value, `done()` cancels. */
  submenu?: (currentValue: string, done: (selectedValue?: string) => void) => PiTuiInteractive;
}

export interface PiTuiSettingsListTheme {
  label: (text: string, selected: boolean) => string;
  value: (text: string, selected: boolean) => string;
  description: (text: string) => string;
  cursor: string;
  hint: (text: string) => string;
}

export interface PiTuiWidth {
  visibleWidth(text: string): number;
  truncateToWidth(text: string, maxWidth: number, ellipsis?: string): string;
  wrapTextWithAnsi(text: string, width: number): string[];
}

let fallbackWarningLogged = false;

function warnFallbackOnce(): void {
  if (fallbackWarningLogged) return;
  fallbackWarningLogged = true;
  console.warn(
    "pi-famulus: pi-tui is unavailable; using the reduced text fallback. " +
      "Load this extension through pi to enable interactive task views and full-width rendering.",
  );
}

export function loadPiTui(): PiTuiModule | null {
  if (forcedTui !== undefined) {
    if (forcedTui === null) warnFallbackOnce();
    return forcedTui;
  }
  // pi's extension loader aliases this static specifier to its bundled TUI in
  // both jiti and compiled-binary modes. Resolving it relative to argv[1] is
  // incorrect when pi is launched through a package-manager shim.
  return bundledPiTui as unknown as PiTuiModule;
}

let widthFns: PiTuiWidth | null | undefined;
let forcedTui: PiTuiModule | null | undefined;

/** Test hook. `null` forces the no-pi-tui fallback; `undefined` restores normal resolution. */
export function setPiTuiForTests(mod: PiTuiModule | null | undefined): void {
  forcedTui = mod;
  widthFns = undefined;
}

function widthApi(): PiTuiWidth | null {
  if (widthFns !== undefined) return widthFns;
  const tui = (forcedTui !== undefined ? forcedTui : loadPiTui()) as (PiTuiModule & Partial<PiTuiWidth>) | null;
  if (tui && typeof tui.visibleWidth === "function" && typeof tui.truncateToWidth === "function" && typeof tui.wrapTextWithAnsi === "function") {
    widthFns = {
      visibleWidth: tui.visibleWidth.bind(tui),
      truncateToWidth: tui.truncateToWidth.bind(tui),
      wrapTextWithAnsi: tui.wrapTextWithAnsi.bind(tui),
    };
    return widthFns;
  }
  if (!tui) warnFallbackOnce();
  widthFns = null;
  return null;
}

/** Visible columns, ANSI stripped. Falls back to a CJK-aware counter if pi-tui is absent. */
export function visibleWidth(text: string): number {
  const api = widthApi();
  if (api) return api.visibleWidth(text);
  return fallbackVisibleWidth(text);
}

export function truncateToWidth(text: string, maxWidth: number, ellipsis = "…"): string {
  const api = widthApi();
  if (api) return api.truncateToWidth(text, maxWidth, ellipsis);
  return fallbackTruncate(text, maxWidth, ellipsis);
}

export function wrapTextWithAnsi(text: string, width: number): string[] {
  const api = widthApi();
  if (api) return api.wrapTextWithAnsi(text, Math.max(1, width));
  return fallbackWrap(text, Math.max(1, width));
}

/** Wrap, then hard-truncate any line that is still too wide (ANSI-safe). */
export function fitLines(text: string, width: number): string[] {
  const limit = Math.max(1, width);
  const wrapped = wrapTextWithAnsi(text, limit);
  return wrapped.map((line) => (visibleWidth(line) > limit ? truncateToWidth(line, limit, "…") : line));
}

function stripAnsi(text: string): string {
  return text.replace(/\x1b\[[0-9;]*m/g, "");
}

function fallbackVisibleWidth(text: string): number {
  let n = 0;
  for (const ch of stripAnsi(text)) {
    const cp = ch.codePointAt(0) ?? 0;
    n += cp > 0xff ? 2 : 1;
  }
  return n;
}

function fallbackTruncate(text: string, maxWidth: number, ellipsis: string): string {
  if (maxWidth <= 0) return "";
  if (fallbackVisibleWidth(text) <= maxWidth) return text;
  const ell = fallbackVisibleWidth(ellipsis) >= maxWidth ? "" : ellipsis;
  const budget = maxWidth - fallbackVisibleWidth(ell);
  let out = "";
  let n = 0;
  for (const ch of stripAnsi(text)) {
    const w = (ch.codePointAt(0) ?? 0) > 0xff ? 2 : 1;
    if (n + w > budget) break;
    out += ch;
    n += w;
  }
  return out + ell;
}

function fallbackWrap(text: string, width: number): string[] {
  const out: string[] = [];
  for (const raw of text.split("\n")) {
    if (fallbackVisibleWidth(raw) <= width) {
      out.push(raw);
      continue;
    }
    let cur = "";
    let n = 0;
    for (const ch of raw) {
      const w = (ch.codePointAt(0) ?? 0) > 0xff ? 2 : 1;
      if (n + w > width && cur) {
        out.push(cur);
        cur = "";
        n = 0;
      }
      cur += ch;
      n += w;
    }
    out.push(cur);
  }
  return out.length > 0 ? out : [""];
}
