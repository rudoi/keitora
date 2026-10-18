// Pure helpers for the terminal paste path (TerminalPane's sendPaste). DOM-free so
// they can be unit-tested. The backend owns bracketing: it strips any embedded
// bracketed-paste markers and wraps the text in one block, so nothing here adds them.

import { formatBytes } from "./format";

/**
 * Newline normalisation for a paste, matching xterm's own paste preparation:
 * `\r\n` and `\n` become `\r` (what Enter sends). A lone `\r` is left alone.
 */
export function preparePasteText(text: string): string {
  return text.replace(/\r?\n/g, "\r");
}

/** A paste the backend refused for size: its measured size and the cap it exceeded. */
export interface PasteRefusal {
  bytes: number;
  limit: number;
}

/**
 * The size refusal carried by a `pty_paste` rejection (`PasteTooLarge: <bytes> > <limit>`),
 * or null for any other error (unreachable backend, no live session, ...).
 */
export function parsePasteRefusal(err: unknown): PasteRefusal | null {
  const m = /PasteTooLarge: (\d+) > (\d+)/.exec(String(err instanceof Error ? err.message : err));
  return m ? { bytes: Number(m[1]), limit: Number(m[2]) } : null;
}

/**
 * The largest bracketed paste block (payload plus both markers, UTF-8 bytes) the backend
 * sends as one paste. Keep in sync with `BRIDGE_PASTE_LIMIT` in
 * `src-tauri/src/backend/herdr/paste.rs`.
 */
export const PASTE_BLOCK_LIMIT = 1024 * 1024;
/** The bytes the two bracketed-paste markers (`ESC[200~`, `ESC[201~`) add to a block. */
export const PASTE_MARKERS_LEN = 12;
/** The largest paste payload (UTF-8 bytes) that fits in one block. */
export const PASTE_PAYLOAD_LIMIT = PASTE_BLOCK_LIMIT - PASTE_MARKERS_LEN;

/** The UTF-8 length of `text` (a lone surrogate counts as U+FFFD, three bytes), without encoding it. */
export function utf8Length(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) bytes += 1;
    else if (c < 0x800) bytes += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length) {
      const d = text.charCodeAt(i + 1);
      if (d >= 0xdc00 && d <= 0xdfff) {
        bytes += 4;
        i++;
      } else bytes += 3;
    } else bytes += 3;
  }
  return bytes;
}

/**
 * The size refusal the backend would give a prepared paste, found without sending it, or
 * null when it may fit (or only the backend can tell). The backend refuses by the size left
 * after it strips embedded markers and NULs, so text holding an ESC or NUL is left to it.
 * Measured as the backend does: the payload plus the markers against the block limit.
 */
export function localPasteRefusal(text: string): PasteRefusal | null {
  // Each UTF-16 unit is at most three UTF-8 bytes: short text fits without counting.
  if (text.length * 3 <= PASTE_PAYLOAD_LIMIT) return null;
  if (text.includes("\x1b") || text.includes("\0")) return null;
  const bytes = utf8Length(text) + PASTE_MARKERS_LEN;
  return bytes > PASTE_BLOCK_LIMIT ? { bytes, limit: PASTE_BLOCK_LIMIT } : null;
}

/** What a paste's delivery reports back: the backend took it, or a toast line for the user. */
export interface PasteHooks {
  accepted(): void;
  refused(message: string): void;
}

/**
 * Send a prepared, non-empty paste through `send` (ptyPaste). One over the size cap is
 * refused locally, without shipping it to the backend. `accepted` runs only once the
 * backend took the paste; a size refusal from either side becomes a `refused` toast line,
 * and other errors stay silent (like ptyWrite).
 */
export async function deliverPaste(
  text: string,
  send: (text: string) => Promise<unknown>,
  hooks: PasteHooks,
): Promise<void> {
  const local = localPasteRefusal(text);
  if (local) {
    hooks.refused(pasteRefusedMessage(local));
    return;
  }
  try {
    await send(text);
  } catch (err) {
    const refusal = parsePasteRefusal(err);
    if (refusal) hooks.refused(pasteRefusedMessage(refusal));
    return;
  }
  hooks.accepted();
}

/** The toast line for a `pty_paste` size rejection (UTF-8 bytes), naming its size and the limit. */
export function pasteRefusedMessage(r: PasteRefusal): string {
  return `Paste refused: ${formatBytes(r.bytes)} is over the ${formatBytes(r.limit)} limit.`;
}

/**
 * The toast line for the writer's refusal event while the bridge is down. Its size is a
 * command-line cost estimate, hence the "~".
 */
export function fallbackPasteRefusedMessage(r: PasteRefusal): string {
  return `Paste too large to send while reconnecting (~${formatBytes(r.bytes)}, limit ${formatBytes(r.limit)}).`;
}
