import { WebLinksAddon } from "@xterm/addon-web-links";
import type { ILink, ILinkProvider, Terminal } from "@xterm/xterm";
import { bufferRowCells } from "./bufferText";

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

export async function activateTerminalLink(
  text: string,
  actions: TerminalLinkActions,
  delimited = false,
) {
  const context = actions.context();
  if (!context.active) return;
  const destination = classifyTerminalLink(text, delimited);
  try {
    if (!destination) throw new Error("Unsupported or invalid terminal link.");
    if (destination.kind === "web") await actions.openUrl(destination.url);
    else {
      await actions.openFile(context.project, context.worktree, destination.path);
    }
  } catch (error) {
    actions.showError(
      `Could not open link: ${error instanceof Error ? error.message : String(error)}`,
    );
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

export function createFileLinkProvider(
  terminal: Terminal,
  callbacks: Pick<ILink, "hover" | "leave"> & {
    activate(event: MouseEvent, text: string, delimited?: boolean): void;
  },
): ILinkProvider {
  return {
    provideLinks(y, callback) {
      const buffer = terminal.buffer.active;
      let start = y - 1;
      let end = start;
      // Bound work even for pathological output with thousands of wrapped rows.
      const maxRows = Math.max(1, Math.ceil(65536 / terminal.cols));
      while (start > 0 && end - start < maxRows && buffer.getLine(start)?.isWrapped) start--;
      while (end - start < maxRows && buffer.getLine(end + 1)?.isWrapped) end++;
      // The joined text, and the 1-based cell each of its UTF-16 units sits in.
      let text = "";
      const xs: number[] = [];
      const ys: number[] = [];
      for (let row = start; row <= end; row++) {
        const { text: rowText, cellAt } = bufferRowCells(buffer, row, terminal.cols, row < end);
        text += rowText;
        for (let i = 0; i < rowText.length; i++) {
          xs.push(cellAt[i] + 1);
          ys.push(row + 1);
        }
      }
      callback(
        findFileReferences(text).flatMap((match): ILink[] => {
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
              activate: (event, text) => callbacks.activate(event, text, match.delimited),
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
    activate: (event: MouseEvent, text: string, delimited = false) => {
      hide();
      if (event.button !== 0 || dragged || terminal.hasSelection()) return;
      void activateTerminalLink(text, actions, delimited);
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
  const webLinks = new WebLinksAddon(callbacks.activate, callbacks);
  terminal.loadAddon(webLinks);
  const files = terminal.registerLinkProvider(createFileLinkProvider(terminal, callbacks));
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
      files.dispose();
      webLinks.dispose();
      terminal.options.linkHandler = previousHandler;
    },
  };
}
