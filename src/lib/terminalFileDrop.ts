/**
 * Helpers for injecting filesystem paths into a terminal's PTY — shared by drag &
 * drop and clipboard image paste.
 *
 * Both flows converge on a bracketed paste (`ptyPaste(project, name, text)`); only
 * how the path is obtained differs. Quoting lives here (TS) rather than in Rust because dragged
 * paths arrive as JS strings on the Tauri drag-drop event and never reach the
 * backend — only pasted image *bytes* do, via `save_clipboard_image`.
 */

/**
 * A terminal pane's write identity: the `(projectName, name)` pair TerminalPane
 * already passes to `ptyPaste`/`ptyWrite` (TerminalPane.tsx). `name` is the worktree name.
 */
export interface PaneHit {
  project: string;
  name: string;
}

/**
 * Quote a path for injection into Claude Code's input line.
 *
 * Paths containing whitespace are wrapped in double quotes (embedded quotes
 * escaped); anything else passes through untouched. Separators are deliberately
 * NOT rewritten — native `\` on Windows and `/` elsewhere are what the OS handed
 * us and what Claude resolves against the terminal's cwd.
 */
export function quotePathForInjection(path: string): string {
  // Strip control characters first. A filename may legally contain a newline on
  // macOS/Linux, and the paste path normalises \n to \r — so an embedded newline would
  // *submit* the prompt half-way through the path instead of injecting it. Quoting
  // alone doesn't help: \n also matches \s, so the path would just be quoted and sent.
  // biome-ignore lint/suspicious/noControlCharactersInRegex: stripping them is the point
  const safe = path.replace(/[\u0000-\u001f\u007f]/g, "");
  if (!/\s/.test(safe)) return safe;
  return `"${safe.replace(/"/g, '\\"')}"`;
}

/**
 * The text to write into the PTY for a set of dropped paths: each quoted, space
 * separated, with a trailing space so consecutive drops concatenate cleanly.
 *
 * Deliberately no trailing newline — the user reviews the prompt and submits it,
 * so a dropped file never auto-sends.
 */
export function injectionTextForPaths(paths: string[]): string {
  const quoted = paths.filter((p) => p.length > 0).map(quotePathForInjection);
  if (quoted.length === 0) return "";
  return `${quoted.join(" ")} `;
}

/** Resolves a viewport point to the topmost element; defaults to the real document. */
export type PointResolver = (x: number, y: number) => Element | null;

/**
 * Convert a Tauri drag-drop `position` (physical pixels) to CSS pixels.
 *
 * The webview drag-drop event reports physical device pixels, while
 * `elementFromPoint` works in CSS pixels — on a HiDPI display an unconverted
 * point lands on the wrong pane, or outside the window entirely. A non-positive
 * ratio is treated as 1 so a bogus `devicePixelRatio` can't zero out the point.
 */
export function cssPointFromPhysical(
  position: { x: number; y: number },
  devicePixelRatio: number,
): { x: number; y: number } {
  const dpr = devicePixelRatio > 0 ? devicePixelRatio : 1;
  return { x: position.x / dpr, y: position.y / dpr };
}

/**
 * Which terminal pane sits at a viewport point, or null if none does.
 *
 * Hit-tests via `elementFromPoint` rather than a registry of pane rects because
 * panes are laid out inline in App.tsx with no parent owning them, and every warm
 * project's panes stay mounted — hidden with `display:none` (see paneVisibility).
 * elementFromPoint naturally skips hidden panes; comparing stored rects would
 * happily route a drop to an off-screen background project.
 *
 * `resolve` is injectable so the routing logic is unit-testable without a DOM.
 */
export function paneAtPoint(x: number, y: number, resolve?: PointResolver): PaneHit | null {
  const pane = paneElementAtPoint(x, y, resolve);
  const project = pane?.dataset?.project;
  const name = pane?.dataset?.name;
  if (!project || !name) return null;
  return { project, name };
}

/**
 * The pane element at a viewport point — what a drop is handed to.
 *
 * Same hit-test as {@link paneAtPoint}, returning the element so a drop can be
 * dispatched *at* the owning pane (see {@link FILE_DROP_EVENT}).
 */
export function paneElementAtPoint(
  x: number,
  y: number,
  resolve?: PointResolver,
): HTMLElement | null {
  const resolver = resolve ?? ((px: number, py: number) => document.elementFromPoint(px, py));
  const hit = resolver(x, y);
  return (hit?.closest?.(".pane") as HTMLElement | null | undefined) ?? null;
}

/**
 * DOM event carrying dropped paths to the pane that owns the PTY.
 *
 * The OS drop is observed once at the app level (only the webview sees file paths),
 * but the *write* has to happen inside TerminalPane: that's where `sendPaste`
 * lives, which applies the input gate (attaching, daemon-down) and sends the text
 * as one paste that the backend brackets. Dispatching at the hit-tested element keeps that gating
 * intact without App.tsx holding a registry of panes or their PTY state.
 */
export const FILE_DROP_EVENT = "keitora:filedrop";

/** Payload of a {@link FILE_DROP_EVENT}. */
export interface FileDropDetail {
  paths: string[];
}
