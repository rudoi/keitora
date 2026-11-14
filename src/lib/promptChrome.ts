// Removing a CLI agent's prompt box (and its scrolled-up hint) from copied terminal text.
//
// The agent CLIs draw their input as a box at the bottom of the screen:
//
// - Claude Code: a horizontal rule, the prompt line(s) starting with "❯", another rule,
//   then a status footer.
// - Codex: the prompt line(s) starting with "›", then indented status lines (model and
//   folder, shortcuts), with no rules.
//
// While scrolled up, each also overlays a "back to the bottom" hint on its transcript.
// A selection that reaches the live screen takes the box along, and because the box is
// redrawn in place, history can hold stale copies of it between the real output. None of
// that is text anyone means to copy, so it is cut out; a selection of nothing but a box is
// left as it is.

/** A full-width horizontal rule: a Claude Code box's top or bottom edge. */
const RULE = /^[─━]{10,}$/;
/** Claude Code's first prompt line: the prompt glyph, then whatever is typed. */
const CLAUDE_PROMPT = /^[❯>](\s|$)/;
/** Codex's first prompt line. Its past messages start the same way, in the transcript. */
const CODEX_PROMPT = /^›(\s|$)/;
/** Status footer lines under a box (mode, shortcuts, background agents, warnings). */
const FOOTER =
  /⏵|shift\+tab|for shortcuts|\? shortcuts|esc to|enter\/esc|ctrl\+|f2 to view|←\s*\d+\s+agent|\bmode on\b/i;
/** The hints shown while scrolled up, possibly drawn over a transcript line. */
const HINTS = [/\s*Jump to bottom \(ctrl\+End\) ↓\s*/g, /\s*↓ Back to bottom · esc\s*/g];
const HINT_TEXT = ["Jump to bottom (ctrl+End)", "Back to bottom · esc"];

/** Most lines a typed prompt spans before the box is not taken for one. */
const MAX_PROMPT_LINES = 8;
/** Most footer lines taken with a box. */
const MAX_FOOTER_LINES = 3;

const trimmed = (lines: string[], k: number) => lines[k]?.trim() ?? "";

/** End (exclusive) of the footer lines from `j`. */
function footerEnd(lines: string[], j: number): number {
  let end = j;
  for (let n = 0; n < MAX_FOOTER_LINES && end < lines.length; n++) {
    if (!FOOTER.test(trimmed(lines, end))) break;
    end++;
  }
  return end;
}

/** A Claude Code box: rule, prompt line(s), rule, footer. */
function claudeBoxEnd(lines: string[], i: number): number {
  if (!RULE.test(trimmed(lines, i)) || !CLAUDE_PROMPT.test(trimmed(lines, i + 1))) return -1;
  let j = i + 2;
  while (j < lines.length && j <= i + MAX_PROMPT_LINES && !RULE.test(trimmed(lines, j))) j++;
  if (!RULE.test(trimmed(lines, j))) return -1;
  return footerEnd(lines, j + 1);
}

/**
 * A Codex box: prompt line, then only blank or indented lines up to its footer. A past
 * message in the transcript also starts with "›", but its reply follows at the left
 * margin ("• …") before any footer could, so it never matches.
 */
function codexBoxEnd(lines: string[], i: number): number {
  const first = lines[i] ?? "";
  if (/^\s/.test(first) || !CODEX_PROMPT.test(first)) return -1;
  for (let j = i + 1; j < lines.length && j <= i + MAX_PROMPT_LINES + MAX_FOOTER_LINES; j++) {
    const line = lines[j];
    if (line.trim() !== "" && !/^\s/.test(line)) return -1;
    if (FOOTER.test(line)) return footerEnd(lines, j);
  }
  return -1;
}

/**
 * The end (exclusive) of a prompt box starting at `i`, or -1 when `i` does not start one.
 * The box takes its footer lines with it.
 */
function boxEnd(lines: string[], i: number): number {
  const claude = claudeBoxEnd(lines, i);
  return claude !== -1 ? claude : codexBoxEnd(lines, i);
}

/** Whether `line` carries a scrolled-up hint. */
function hasHint(line: string): boolean {
  return HINT_TEXT.some((h) => line.includes(h));
}

/** `line` without any scrolled-up hint; null when the hint was all it held. */
function withoutHint(line: string): string | null {
  if (!hasHint(line)) return line;
  let rest = line;
  for (const hint of HINTS) rest = rest.replace(hint, " ");
  rest = rest.replace(/ +$/, "");
  return rest.trim() === "" ? null : rest;
}

/**
 * `text` (lines joined by `eol`) without any prompt boxes or scrolled-up hints, trimmed of
 * the blank lines left.
 */
export function stripPromptChrome(text: string, eol: string): string {
  const lines: string[] = [];
  let removed = false;
  for (const line of text.split(eol)) {
    const rest = withoutHint(line);
    if (rest !== line) removed = true;
    if (rest !== null) lines.push(rest);
  }
  const kept: string[] = [];
  for (let i = 0; i < lines.length; ) {
    const end = boxEnd(lines, i);
    if (end === -1) {
      kept.push(lines[i]);
      i++;
    } else {
      removed = true;
      i = end;
    }
  }
  if (!removed) return text;
  while (kept.length > 0 && kept[0].trim() === "") kept.shift();
  while (kept.length > 0 && kept[kept.length - 1].trim() === "") kept.pop();
  // Nothing but the box was selected: that is what was meant.
  return kept.length === 0 ? text : kept.join(eol);
}

/**
 * Where the CLI's fixed chrome starts on a screen of rows: the prompt box, or a scrolled-up
 * hint just above it. `rows.length` when the screen shows no box. The chrome stays put
 * while the transcript above it scrolls, so it must not be lined up or kept as scrolled
 * text.
 */
export function chromeStart(rows: string[]): number {
  for (let i = rows.length - 1; i >= 0; i--) {
    if (boxEnd(rows, i) === -1) continue;
    for (let k = i - 1; k >= Math.max(0, i - 2); k--) {
      if (hasHint(rows[k])) return k;
    }
    return i;
  }
  return rows.length;
}
