import type { Terminal } from "@xterm/xterm";
import { type RefObject, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { bufferRowCells, bufferRowText, cellSpan } from "../lib/bufferText";
import { trailingDebounce } from "../lib/debounce";
import {
  appOwnsScroll,
  findInLines,
  historyLines,
  type Match,
  nearestMatch,
  reanchorIndex,
  rowInViewport,
  type SearchFlags,
  stepIndex,
  targetOffset,
} from "../lib/historySearch";
import { ptyReadHistory, ptyScrollOffset } from "../lib/ipc";

/** How much history one search reads: bounds the IPC payload. */
const HISTORY_LINES = 20000;
/** Typing settles this long before history is searched. */
const SEARCH_DEBOUNCE_MS = 150;
/** After a scroll, the repaint is taken as landed once writes pause this long… */
const REPAINT_SETTLE_MS = 40;
/** …or after this long at most. */
const REPAINT_TIMEOUT_MS = 500;

/** What the pane lets the search do to its view of herdr's history. */
export interface HistoryView {
  /**
   * Scroll herdr's history by `lines` (positive = up, toward older output). Resolves
   * false when nothing was sent (the app owns the scroll, or the pane is unavailable);
   * rejects when the scroll failed.
   */
  scrollBy: (lines: number) => Promise<boolean>;
  /** Bring the pane back to its live tail. */
  returnToLive: () => void;
}

interface TerminalSearchProps {
  term: Terminal;
  project: string;
  id: string;
  /** The pane's history view; the pane keeps `current` up to date. */
  history: RefObject<HistoryView>;
  onClose: () => void;
}

/** The searched history and its hits; `index` is the current hit (-1: none). */
interface SearchState {
  lineCount: number;
  matches: Match[];
  index: number;
  error?: "invalid-regex" | "unavailable";
}

const NO_HITS: SearchState = { lineCount: 0, matches: [], index: -1 };

/** Identifies what a result was searched for, so a stale one can be told apart. */
function searchKey(query: string, flags: SearchFlags): string {
  return JSON.stringify([query, flags.caseSensitive, flags.regex]);
}

/** The counter text. Counted from the newest hit, so the first one shown is 1. */
function describe(s: SearchState | null): string {
  if (!s) return "";
  if (s.error === "invalid-regex") return "Invalid regex";
  if (s.error === "unavailable") return "Unavailable";
  const count = s.matches.length;
  if (count === 0) return "No results";
  return `${count - s.index} / ${count}`;
}

/**
 * Scroll, then wait until the repaint it triggers has been written to `term`. Resolves
 * with whether a scroll was sent at all; rejects when it failed.
 */
async function scrollAndSettle(term: Terminal, scroll: () => Promise<boolean>): Promise<boolean> {
  let stop = () => {};
  const settled = new Promise<void>((resolve) => {
    const capAt = performance.now() + REPAINT_TIMEOUT_MS;
    let lastWrite: number | undefined;
    let done = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const finish = () => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      sub.dispose();
      requestAnimationFrame(() => resolve());
    };
    stop = finish;
    // Finish at the cap, or once writes have paused; otherwise check again when due.
    const check = () => {
      const due = lastWrite === undefined ? capAt : Math.min(capAt, lastWrite + REPAINT_SETTLE_MS);
      const now = performance.now();
      if (now >= due) finish();
      else timer = setTimeout(check, due - now);
    };
    const sub = term.onWriteParsed(() => {
      const first = lastWrite === undefined;
      lastWrite = performance.now();
      // Only the first write moves the timer up; later ones are picked up when it fires.
      if (!first) return;
      clearTimeout(timer);
      check();
    });
    timer = setTimeout(check, REPAINT_TIMEOUT_MS);
  });
  let applied: boolean;
  try {
    applied = await scroll();
  } catch (err) {
    stop();
    throw err;
  }
  if (!applied) {
    stop();
    return false;
  }
  await settled;
  return true;
}

/**
 * Find in the pane's history. herdr owns the history, so the box reads it as text, finds
 * hits there, scrolls herdr's viewport to the current one, then highlights it on the
 * repainted screen (best effort: the hit is looked for near where it should land).
 * While the app in the pane owns the scroll, the box does not move the view and only
 * highlights a hit that is already on screen.
 */
export function TerminalSearch({ term, project, id, history, onClose }: TerminalSearchProps) {
  const [query, setQuery] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [regex, setRegex] = useState(false);
  const flags = useMemo<SearchFlags>(() => ({ caseSensitive, regex }), [caseSensitive, regex]);
  const [result, setResult] = useState<SearchState | null>(null);
  // Mirrors `result` for the async search and jump callbacks.
  const resultRef = useRef<SearchState | null>(null);
  // The search key `result` was found for; null until a search has produced one.
  const searchedRef = useRef<string | null>(null);
  // Drops the debounced search waiting to run, if any.
  const cancelPendingRef = useRef<() => void>(() => {});
  const inputRef = useRef<HTMLInputElement | null>(null);

  // The history last read; null once output has arrived since, so it must be re-read.
  const linesRef = useRef<string[] | null>(null);
  // How many of the box's own scrolls are in flight; writes meanwhile are their repaint.
  const ownScrollsRef = useRef(0);
  // Bumped by each search or jump, so a slower earlier one drops its result.
  const seqRef = useRef(0);
  const movedRef = useRef(false);
  // The selection this box made, so closing clears only its own.
  const highlightRef = useRef<{ x: number; y: number; width: number } | null>(null);

  const show = useCallback((s: SearchState | null) => {
    resultRef.current = s;
    setResult(s);
  }, []);

  // Focus input on mount
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const clearHighlight = useCallback(() => {
    const h = highlightRef.current;
    highlightRef.current = null;
    if (!h) return;
    const sel = term.getSelectionPosition();
    if (sel && sel.start.x === h.x && sel.start.y === h.y) term.clearSelection();
  }, [term]);

  // Output (not our own scroll's repaint) makes the read history stale.
  useEffect(() => {
    const sub = term.onWriteParsed(() => {
      if (ownScrollsRef.current === 0) linesRef.current = null;
    });
    return () => sub.dispose();
  }, [term]);

  // Closing: drop the highlight and bring the pane back to live if a jump moved it.
  useEffect(
    () => () => {
      seqRef.current++;
      clearHighlight();
      if (movedRef.current) history.current.returnToLive();
    },
    [clearHighlight, history],
  );

  const readHistory = useCallback(async () => {
    const lines = historyLines(await ptyReadHistory(project, id, HISTORY_LINES));
    linesRef.current = lines;
    return lines;
  }, [project, id]);

  // herdr's scroll offset; only meaningful (and only fetched) on the normal screen.
  const readOffset = useCallback(
    (): Promise<number | null> =>
      term.buffer.active.type === "normal"
        ? ptyScrollOffset(project, id).catch(() => null)
        : Promise.resolve(null),
    [term, project, id],
  );

  /**
   * Make hit `index` of `next` current: show it, scroll it into view and highlight it on
   * the visible screen. `offset` is herdr's scroll offset when already being fetched.
   */
  const jumpTo = useCallback(
    async (seq: number, next: SearchState, offset?: Promise<number | null>) => {
      show(next);
      clearHighlight();
      const match = next.matches[next.index];
      if (!match) return;
      const rows = term.rows;
      const current = (await (offset ?? readOffset())) ?? 0;
      if (seq !== seqRef.current) return;
      // The offset the screen shows once the jump is done: unchanged unless it scrolled.
      let shownAt = current;
      if (!appOwnsScroll(term.buffer.active.type, term.modes.mouseTrackingMode)) {
        const target = targetOffset(match.line, next.lineCount, rows);
        const delta = target - current;
        if (delta !== 0) {
          let applied: boolean;
          ownScrollsRef.current++;
          try {
            applied = await scrollAndSettle(term, () => history.current.scrollBy(delta));
          } catch {
            if (seq === seqRef.current) show({ ...next, error: "unavailable" });
            return;
          } finally {
            ownScrollsRef.current--;
          }
          if (seq !== seqRef.current) return;
          if (applied) {
            movedRef.current = true;
            shownAt = target;
          }
        }
      }
      // Only a hit on the screen as it now stands is highlighted.
      const expected = rowInViewport(match.line, next.lineCount, rows, shownAt);
      if (expected === null) return;
      // herdr's text and its screen rows can disagree a little (output in between), so
      // find the hit on the screen near where it should be.
      const buf = term.buffer.active;
      const top = buf.viewportY;
      const screen: string[] = [];
      for (let y = 0; y < rows; y++) screen.push(bufferRowText(buf, top + y, term.cols));
      const hits = findInLines(screen, query, flags).matches;
      const hit = nearestMatch(hits, expected, match.col);
      if (!hit) return;
      const { x, width } = cellSpan(
        bufferRowCells(buf, top + hit.line, term.cols),
        hit.col,
        hit.len,
      );
      const y = top + hit.line;
      term.select(x, y, width);
      highlightRef.current = { x, y, width };
    },
    [term, query, flags, history, show, clearHighlight, readOffset],
  );

  /** Search the history for the current query and land on the newest hit. */
  const runSearch = useCallback(
    async (seq: number) => {
      try {
        // A re-read and the scroll offset the jump needs are fetched together.
        const offset = linesRef.current ? undefined : readOffset();
        const lines = linesRef.current ?? (await readHistory());
        if (seq !== seqRef.current) return;
        const r = findInLines(lines, query, flags);
        searchedRef.current = searchKey(query, flags);
        await jumpTo(seq, { ...r, index: r.matches.length - 1 }, offset);
      } catch {
        if (seq !== seqRef.current) return;
        show({ ...NO_HITS, error: "unavailable" });
      }
    },
    [query, flags, readHistory, readOffset, jumpTo, show],
  );

  // Search on open and whenever the query or a flag changes (debounced). The current
  // hit starts at the newest one, nearest the live tail.
  useEffect(() => {
    const seq = ++seqRef.current;
    if (query === "") {
      clearHighlight();
      show(null);
      return;
    }
    const search = trailingDebounce(() => void runSearch(seq), SEARCH_DEBOUNCE_MS);
    search.call();
    cancelPendingRef.current = search.cancel;
    return search.cancel;
  }, [query, runSearch, clearHighlight, show]);

  /** Step through the hits: -1 toward older output, +1 toward newer, wrapping. */
  const step = async (delta: number) => {
    if (query === "") return;
    // The shown hits are for an earlier query (its search is still debouncing or in
    // flight): search for this one now instead of stepping through the old hits.
    if (searchedRef.current !== searchKey(query, flags)) {
      cancelPendingRef.current();
      await runSearch(++seqRef.current);
      return;
    }
    const seq = ++seqRef.current;
    const before = resultRef.current ?? NO_HITS;
    let { lineCount, matches, index } = before;
    try {
      let offset: Promise<number | null> | undefined;
      // Output since the last read moved the hits: re-read and keep the current one.
      if (!linesRef.current) {
        offset = readOffset();
        const r = findInLines(await readHistory(), query, flags);
        if (seq !== seqRef.current) return;
        const old = matches[index];
        index = old ? reanchorIndex(old, lineCount, r.matches, r.lineCount) : -1;
        if (index < 0) index = r.matches.length - 1 - delta;
        ({ lineCount, matches } = r);
        if (r.error) {
          show({ lineCount, matches: [], index: -1, error: r.error });
          return;
        }
      }
      if (matches.length === 0) return;
      index = stepIndex(index, matches.length, delta);
      await jumpTo(seq, { lineCount, matches, index }, offset);
    } catch {
      if (seq !== seqRef.current) return;
      show({ ...before, error: "unavailable" });
    }
  };

  const findOlder = () => void step(-1);
  const findNewer = () => void step(1);

  return (
    <div className="search-bar">
      <input
        ref={inputRef}
        type="text"
        value={query}
        placeholder="Find..."
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if ((e.key === "Enter" && !e.shiftKey) || e.key === "ArrowUp") {
            e.preventDefault();
            findOlder();
          } else if ((e.key === "Enter" && e.shiftKey) || e.key === "ArrowDown") {
            e.preventDefault();
            findNewer();
          } else if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
      />
      <span className="search-count">{describe(result)}</span>
      <button onClick={findOlder} title="Older match (Enter)">
        ↑
      </button>
      <button onClick={findNewer} title="Newer match (Shift+Enter)">
        ↓
      </button>
      <button
        className={caseSensitive ? "active" : undefined}
        onClick={() => setCaseSensitive((v) => !v)}
        title="Case sensitive"
      >
        Aa
      </button>
      <button
        className={regex ? "active" : undefined}
        onClick={() => setRegex((v) => !v)}
        title="Regular expression"
      >
        .*
      </button>
      <button onClick={onClose} title="Close (Esc)">
        ✕
      </button>
    </div>
  );
}
