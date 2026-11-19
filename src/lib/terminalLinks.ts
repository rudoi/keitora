import type { IBuffer, ILink, ILinkProvider, Terminal } from "@xterm/xterm";
import { bufferRowCells, isBlank, isFilled, type RowCells } from "./bufferText";

export type TerminalDestination =
  | { kind: "web"; url: string }
  | { kind: "file"; path: string; line?: number; column?: number };

function hasControlCharacters(text: string) {
  return Array.from(text).some((char) => char.charCodeAt(0) < 32 || char.charCodeAt(0) === 127);
}

export function classifyTerminalLink(text: string, delimited = false): TerminalDestination | null {
  if (!text || text !== text.trim() || hasControlCharacters(text)) return null;
  if (/^https?:\/\//i.test(text)) {
    if (/\s|\\/.test(text)) return null;
    try {
      const url = new URL(text);
      return url.hostname ? { kind: "web", url: url.href } : null;
    } catch {
      return null;
    }
  }
  let path = text;
  let fragment = "";
  if (/^file:/i.test(text)) {
    // Validate before URL normalization can erase a remote authority or traversal.
    const uri = /^file:\/\/([^/]*)(\/[^?#]*)(#[^?]*)?$/i.exec(text);
    if (!uri || (uri[1] && uri[1].toLowerCase() !== "localhost")) return null;
    try {
      path = decodeURIComponent(uri[2]);
    } catch {
      return null;
    }
    fragment = uri[3] ?? "";
    if (fragment && !/^#L?\d+(?:(?::|C)\d+)?$/.test(fragment)) return null;
  } else if (
    /^[a-z][a-z\d+.-]*:/i.test(text) &&
    !/^\/?[a-z]:[\\/]/i.test(text) &&
    !/^[^\s:]+\.[^\s:]+:\d+(?::\d+)?$/.test(text)
  ) {
    return null;
  }
  if (/^\/[a-z]:[\\/]/i.test(path)) path = path.slice(1);
  const location = fragment
    ? /^#L?(\d+)(?:(?::|C)(\d+))?$/.exec(fragment)
    : /:(\d+)(?::(\d+))?$/.exec(path);
  if (location && !fragment) path = path.slice(0, location.index);
  if (
    !path ||
    hasControlCharacters(path) ||
    /^[\\/]{2}/.test(path) ||
    /^[a-z]:[^\\/]/i.test(path) ||
    /:/.test(path.replace(/^[a-z]:/i, "")) ||
    (!/[\\/]/.test(path) &&
      !(delimited ? /^[^.]+(?:\.[^.]+)+$/ : /^[^\s.]+(?:\.[^\s.]+)+$/).test(path) &&
      !location &&
      !/^file:/i.test(text))
  )
    return null;
  const line = location ? Number(location[1]) : undefined;
  const column = location?.[2] ? Number(location[2]) : undefined;
  if (
    (line !== undefined && (!Number.isSafeInteger(line) || line < 1)) ||
    (column !== undefined && (!Number.isSafeInteger(column) || column < 1))
  )
    return null;
  return {
    kind: "file",
    path,
    ...(line === undefined ? {} : { line }),
    ...(column === undefined ? {} : { column }),
  };
}

export interface TerminalLinkActions {
  context(): { project: string; worktree: string; active: boolean };
  openUrl(url: string): Promise<unknown>;
  openFile(project: string, worktree: string, path: string): Promise<void>;
  showError(message: string): void;
}

/** One way to read a link's text: the text, and whether delimiters made spaces in it unambiguous. */
export interface LinkReading {
  text: string;
  delimited: boolean;
}

/**
 * How the backend's terminal file open (`resolve_target` in src-tauri/src/terminal_links.rs)
 * starts its error when the path does not exist or cannot be resolved. Every other error is a
 * deliberate refusal (an executable, a network share, not a regular file...).
 */
export const FILE_UNAVAILABLE_ERROR = "File is unavailable";

const errorMessage = (error: unknown) => (error instanceof Error ? error.message : String(error));

/**
 * Open `text`. A file link that fails to open because the file is unavailable tries each of
 * `fallbacks` (other readings of the same link) in order, stopping at the first that opens or
 * fails for any other reason; if none opens, the original error is reported.
 */
export async function activateTerminalLink(
  text: string,
  actions: TerminalLinkActions,
  delimited = false,
  fallbacks: readonly LinkReading[] = [],
) {
  const context = actions.context();
  if (!context.active) return;
  const destination = classifyTerminalLink(text, delimited);
  try {
    if (!destination) throw new Error("Unsupported or invalid terminal link.");
    if (destination.kind === "web") await actions.openUrl(destination.url);
    else await openFirstAvailable(destination.path, fallbacks, actions, context);
  } catch (error) {
    actions.showError(`Could not open link: ${errorMessage(error)}`);
  }
}

async function openFirstAvailable(
  path: string,
  fallbacks: readonly LinkReading[],
  actions: TerminalLinkActions,
  context: { project: string; worktree: string },
) {
  const open = (target: string) => actions.openFile(context.project, context.worktree, target);
  const unavailable = (error: unknown) => errorMessage(error).startsWith(FILE_UNAVAILABLE_ERROR);
  try {
    await open(path);
  } catch (error) {
    if (!unavailable(error)) throw error;
    const tried = new Set([path]);
    for (const reading of fallbacks) {
      const other = classifyTerminalLink(reading.text, reading.delimited);
      if (other?.kind !== "file" || tried.has(other.path)) continue;
      tried.add(other.path);
      try {
        await open(other.path);
        return;
      } catch (fallbackError) {
        if (!unavailable(fallbackError)) break;
      }
    }
    throw error;
  }
}

export function findFileReferences(
  text: string,
): { text: string; index: number; delimited: boolean }[] {
  const results: { text: string; index: number; delimited: boolean }[] = [];
  // Delimiters make spaces unambiguous; bare references stop at whitespace.
  const tokens =
    /`([^`\r\n]+)`|"([^"\r\n]+)"|'([^'\r\n]+)'|\]\((?:<([^>\r\n]+)>|([^\r\n]+?))\)|[^\s`"'<>()[\]{}]+/g;
  for (const match of text.matchAll(tokens)) {
    const delimited = match.slice(1).find((value) => value !== undefined);
    const candidate = delimited ?? match[0].replace(/[.,;!?]+$/, "");
    if (classifyTerminalLink(candidate, delimited !== undefined)?.kind !== "file") continue;
    results.push({
      text: candidate,
      index: match.index + match[0].indexOf(candidate),
      delimited: delimited !== undefined,
    });
  }
  return results;
}

// From @xterm/addon-web-links (WebLinksAddon.ts, MIT), minus two redundant escapes the
// linter strips: everything from http(s):// up to the first whitespace or quote, with unsafe
// characters, brackets and trailing punctuation excluded from the end.
const strictUrlRegex = /(https?|HTTPS?):[/]{2}[^\s"'!*(){}|\\^<>`]*[^\s"':,.!?{}|\\^~[\]`()<>]/;

// From @xterm/addon-web-links (WebLinkProvider.ts, MIT), verbatim: the match must start with
// the scheme, credentials and host the URL parser reads back.
function isUrl(urlString: string): boolean {
  try {
    const url = new URL(urlString);
    const parsedBase =
      url.password && url.username
        ? `${url.protocol}//${url.username}:${url.password}@${url.host}`
        : url.username
          ? `${url.protocol}//${url.username}@${url.host}`
          : `${url.protocol}//${url.host}`;
    return urlString.toLocaleLowerCase().startsWith(parsedBase.toLocaleLowerCase());
  } catch {
    return false;
  }
}

/** Web URLs in `text`, matched as the xterm web links addon matches them. */
export function findWebLinks(text: string): LineLink[] {
  const results: LineLink[] = [];
  for (const match of text.matchAll(new RegExp(strictUrlRegex.source, "g"))) {
    if (!isUrl(match[0]) || classifyTerminalLink(match[0])?.kind !== "web") continue;
    results.push({ text: match[0], index: match.index, delimited: false });
  }
  return results;
}

/** How a row continues onto the next: a real soft wrap, or a row filled to its edge. */
type RowJoin = "wrap" | "filled" | null;

/** The rows of a terminal as link detection reads them (0-based rows). */
export interface LinkRowReader {
  /** The row's text and cell map, read as continuing on the next row or not; undefined past the buffer. */
  read(row: number, joinsNext: boolean): RowCells | undefined;
  /** Whether the terminal marked the row as continuing the row above. */
  wrapped(row: number): boolean;
  /** Whether the row is filled to its last cell. */
  filled(row: number): boolean;
  /** Whether the row is only spaces; undefined past the buffer. */
  blank(row: number): boolean | undefined;
}

export function bufferRowReader(buffer: IBuffer, cols: number): LinkRowReader {
  return {
    read: (row, joinsNext) =>
      buffer.getLine(row) ? bufferRowCells(buffer, row, cols, joinsNext) : undefined,
    wrapped: (row) => !!buffer.getLine(row)?.isWrapped,
    filled: (row) => isFilled(buffer, row, cols),
    blank: (row) => {
      const line = buffer.getLine(row);
      if (!line) return undefined;
      const cell = buffer.getNullCell();
      for (let col = 0; col < cols; col++) {
        if (!isBlank(line.getCell(col, cell)?.getChars() ?? "")) return false;
      }
      return true;
    },
  };
}

/**
 * Whether `row` continues on `row + 1`. A next row the terminal marked wrapped joins
 * verbatim. A repainted screen never marks rows wrapped, so a row filled to its last cell
 * also joins a non-blank next row.
 */
export function joinsNext(reader: LinkRowReader, row: number): RowJoin {
  const blank = reader.blank(row + 1);
  if (blank === undefined) return null;
  if (reader.wrapped(row + 1)) return "wrap";
  if (!blank && reader.filled(row)) return "filled";
  return null;
}

/**
 * A logical line: its text, the 1-based cell (`xs`) and row (`ys`) of each UTF-16 unit,
 * `breaks`, the offsets in `text` where a row joined because the row above was filled begins,
 * and `indents`, the leading spaces dropped at each of those breaks.
 */
export interface LogicalLine {
  text: string;
  xs: number[];
  ys: number[];
  breaks: number[];
  indents: string[];
}

/**
 * The logical line through `row` (0-based), spanning at most `maxRows + 1` rows. A row joined
 * because the row above was filled loses its leading spaces and no-break spaces (the indent
 * agent TUIs print under their gutter), so they are absent from the text and the maps.
 */
export function logicalLineAt(reader: LinkRowReader, row: number, maxRows: number): LogicalLine {
  const joins = new Map<number, RowJoin>();
  const join = (r: number) => {
    let kind = joins.get(r);
    if (kind === undefined) {
      kind = joinsNext(reader, r);
      joins.set(r, kind);
    }
    return kind;
  };
  let start = row;
  let end = row;
  while (start > 0 && end - start < maxRows && join(start - 1)) start--;
  while (end - start < maxRows && join(end)) end++;
  const line: LogicalLine = { text: "", xs: [], ys: [], breaks: [], indents: [] };
  for (let r = start; r <= end; r++) {
    const cells = reader.read(r, r < end && join(r) !== null);
    if (!cells) break;
    let { text, cellAt } = cells;
    if (r > start && join(r - 1) === "filled") {
      const indent = /^[  ]*/.exec(text)?.[0] ?? "";
      text = text.slice(indent.length);
      cellAt = cellAt.slice(indent.length);
      line.breaks.push(line.text.length);
      line.indents.push(indent);
    }
    line.text += text;
    for (let i = 0; i < text.length; i++) {
      line.xs.push(cellAt[i] + 1);
      line.ys.push(r + 1);
    }
  }
  return line;
}

/** A link found in a logical line. */
export interface LineLink extends LinkReading {
  index: number;
  /**
   * For a file link read across a filled-row join: other readings of it to open, in order,
   * when it turns out to be missing.
   */
  fallbacks?: LinkReading[];
}

const linkEnd = (link: LineLink) => link.index + link.text.length;
const overlaps = (a: LineLink, b: LineLink) => a.index < linkEnd(b) && b.index < linkEnd(a);

/** The web links and the file links not overlapping one in `text`, offset by `offset`. */
function linksIn(text: string, offset = 0): { web: LineLink[]; files: LineLink[] } {
  const shift = (link: LineLink) => ({ ...link, index: link.index + offset });
  const web = findWebLinks(text);
  const files = findFileReferences(text).filter((link) => !web.some((w) => overlaps(w, link)));
  return { web: web.map(shift), files: files.map(shift) };
}

/**
 * A web link crossing filled-row joins, cut back to before the last join it crosses while the
 * text after that join is only letters: a URL ending at the right edge would otherwise absorb
 * the next row's first word. Undefined when nothing reads as a URL before the cut.
 */
function webAcrossJoins(text: string, breaks: number[], link: LineLink): LineLink | undefined {
  let match: LineLink | undefined = link;
  while (match) {
    const index: number = match.index;
    const end = linkEnd(match);
    const cut = breaks.filter((at) => at > index && at < end).pop();
    if (cut === undefined || /[^\p{L}]/u.test(text.slice(cut, end))) return match;
    const head: LineLink | undefined = findWebLinks(text.slice(index, cut)).find(
      (w) => w.index === 0,
    );
    match = head && { ...head, index };
  }
  return undefined;
}

/**
 * The web and file links in `line`, in order. A file link overlapping a web link is dropped.
 *
 * A filled-row join is a guess: it may glue unrelated text onto a link, or drop real spaces
 * with the indent. So links are also read per row (filled-row joins left unjoined), and:
 * - a file link crossing a join carries fallbacks: its span with the indents kept (when
 *   delimited, so spaces are part of it), then the per-row file links it overlaps;
 * - a web link crossing a join is kept only while the text past the join is not just letters;
 * - a per-row link no kept link overlaps (one the joined reading lost) is kept as is.
 */
export function findLineLinks(
  line: Pick<LogicalLine, "text" | "breaks"> & Partial<Pick<LogicalLine, "indents">>,
): LineLink[] {
  const { text, breaks, indents = [] } = line;
  const joined = linksIn(text);
  if (!breaks.length) return [...joined.web, ...joined.files].sort((a, b) => a.index - b.index);
  const bounds = [0, ...breaks, text.length];
  const rows = bounds.slice(1).map((end, i) => linksIn(text.slice(bounds[i], end), bounds[i]));
  const rowFiles = rows.flatMap((row) => row.files);
  const crosses = (link: LineLink) => breaks.some((at) => at > link.index && at < linkEnd(link));

  const kept: LineLink[] = [];
  for (const link of joined.web) {
    const web = crosses(link) ? webAcrossJoins(text, breaks, link) : link;
    if (web) kept.push(web);
  }
  for (const link of joined.files) {
    if (!crosses(link)) {
      kept.push(link);
      continue;
    }
    const fallbacks: LinkReading[] = [];
    if (link.delimited) {
      let verbatim = "";
      let from = link.index;
      breaks.forEach((at, i) => {
        if (at <= link.index || at >= linkEnd(link)) return;
        verbatim += text.slice(from, at) + (indents[i] ?? "");
        from = at;
      });
      verbatim += text.slice(from, linkEnd(link));
      if (verbatim !== link.text) fallbacks.push({ text: verbatim, delimited: true });
    }
    for (const part of rowFiles) {
      if (overlaps(part, link)) fallbacks.push({ text: part.text, delimited: part.delimited });
    }
    kept.push(fallbacks.length ? { ...link, fallbacks } : link);
  }
  const rowLinks = rows.flatMap((row) => [...row.web, ...row.files]);
  const lost = rowLinks.filter((part) => !kept.some((link) => overlaps(link, part)));
  return [...kept, ...lost].sort((a, b) => a.index - b.index);
}

/**
 * Web and file links on a terminal row, read across the rows its logical line spans. A
 * link's range runs from its first to its last cell, so hovering any of its rows underlines
 * and opens the whole link.
 */
export function createTerminalLinkProvider(
  terminal: Terminal,
  callbacks: Pick<ILink, "hover" | "leave"> & {
    activate(
      event: MouseEvent,
      text: string,
      delimited?: boolean,
      fallbacks?: readonly LinkReading[],
    ): void;
  },
): ILinkProvider {
  return {
    provideLinks(y, callback) {
      const buffer = terminal.buffer.active;
      // Bound work even for pathological output with thousands of joined rows.
      const maxRows = Math.max(1, Math.ceil(65536 / terminal.cols));
      const line = logicalLineAt(bufferRowReader(buffer, terminal.cols), y - 1, maxRows);
      const { xs, ys } = line;
      callback(
        findLineLinks(line).flatMap((match): ILink[] => {
          const first = match.index;
          const last = match.index + match.text.length - 1;
          if (last >= xs.length || y < ys[first] || y > ys[last]) return [];
          const lastWidth =
            buffer
              .getLine(ys[last] - 1)
              ?.getCell(xs[last] - 1)
              ?.getWidth() ?? 1;
          return [
            {
              text: match.text,
              range: {
                start: { x: xs[first], y: ys[first] },
                end: { x: xs[last] + lastWidth - 1, y: ys[last] },
              },
              ...callbacks,
              activate: (event, text) =>
                callbacks.activate(event, text, match.delimited, match.fallbacks),
            },
          ];
        }),
      );
    },
  };
}

export function installTerminalLinks(terminal: Terminal, actions: TerminalLinkActions) {
  let mouseDown: { x: number; y: number } | undefined;
  let dragged = false;
  const onMouseDown = (event: MouseEvent) => {
    mouseDown = { x: event.clientX, y: event.clientY };
    dragged = false;
  };
  const onMouseMove = (event: MouseEvent) => {
    if (
      mouseDown &&
      event.buttons &&
      Math.hypot(event.clientX - mouseDown.x, event.clientY - mouseDown.y) > 4
    )
      dragged = true;
  };
  window.addEventListener("mousedown", onMouseDown, true);
  window.addEventListener("mousemove", onMouseMove, true);
  const tooltip = document.createElement("div");
  tooltip.setAttribute("role", "tooltip");
  Object.assign(tooltip.style, {
    position: "fixed",
    bottom: "12px",
    left: "12px",
    zIndex: "100",
    maxWidth: "calc(100vw - 24px)",
    padding: "6px 10px",
    borderRadius: "4px",
    background: "#14201d",
    color: "#ffffff",
    fontSize: "12px",
    overflowWrap: "anywhere",
    pointerEvents: "none",
  });
  const hide = () => tooltip.remove();
  const callbacks = {
    activate: (
      event: MouseEvent,
      text: string,
      delimited = false,
      fallbacks?: readonly LinkReading[],
    ) => {
      hide();
      if (event.button !== 0 || dragged || terminal.hasSelection()) return;
      void activateTerminalLink(text, actions, delimited, fallbacks);
    },
    hover: (_event: MouseEvent, text: string) => {
      if (!actions.context().active) return;
      tooltip.textContent = text;
      document.body.appendChild(tooltip);
    },
    leave: hide,
  };
  const previousHandler = terminal.options.linkHandler;
  terminal.options.linkHandler = {
    allowNonHttpProtocols: true,
    ...callbacks,
    activate: (event, text) => callbacks.activate(event, text),
  };
  const links = terminal.registerLinkProvider(createTerminalLinkProvider(terminal, callbacks));
  const scroll = terminal.onScroll(hide);
  const resize = terminal.onResize(hide);
  window.addEventListener("blur", hide);
  return {
    hide,
    dispose() {
      hide();
      window.removeEventListener("blur", hide);
      window.removeEventListener("mousedown", onMouseDown, true);
      window.removeEventListener("mousemove", onMouseMove, true);
      scroll.dispose();
      resize.dispose();
      links.dispose();
      terminal.options.linkHandler = previousHandler;
    },
  };
}
