//! `paste` — pure helpers for sending a paste as one bracketed block.
//!
//! herdr's attach input treats a message as a paste only when it is exactly one
//! complete `ESC[200~ … ESC[201~` block; it then re-wraps the text per the pane's
//! real bracketed-paste mode (or strips the markers when the app never enabled
//! it). So the backend owns the bracketing: it removes any markers embedded in
//! the pasted text (an embedded end marker would end the block early and let the
//! rest run as keystrokes) and wraps the result in exactly one block.
//!
//! A paste is refused, never truncated, above either size cap: the bridge one
//! (herdr's own per-message input limit, which counts the markers) and the CLI
//! fallback one (one `pane send-text` argument, bounded by the Windows
//! command-line length).

use std::borrow::Cow;

/// Bracketed-paste start marker.
pub const BRACKET_START: &str = "\x1b[200~";
/// Bracketed-paste end marker.
pub const BRACKET_END: &str = "\x1b[201~";

/// The bytes both markers add to a payload.
const MARKERS_LEN: usize = BRACKET_START.len() + BRACKET_END.len();

/// The largest bracketed block (payload plus both markers, in bytes) sent down
/// the bridge as one paste: herdr's 1 MiB per-message input limit, which it
/// enforces as `len > limit`, so a block of exactly this size is accepted.
/// The frontend refuses a paste over this (less the markers) before sending it:
/// keep `PASTE_PAYLOAD_LIMIT` in `src/lib/terminalPaste.ts` in sync.
pub const BRIDGE_PASTE_LIMIT: usize = 1024 * 1024;

/// The largest [`fallback_cost`] sent as one `pane send-text` argument, well
/// under the Windows `CreateProcess` 32,767-unit command line.
pub const FALLBACK_PASTE_LIMIT: usize = 24 * 1024;

/// `text` with every bracketed-paste marker and NUL removed. No marker is left,
/// even one spliced from its neighbours when another is deleted (`ESC[20` +
/// `ESC[200~` + `0~`). A NUL would make the fallback's argv invalid. Everything
/// else (`\r`, tabs, other control bytes) passes through unchanged. Text with no
/// NUL and no ESC (so no marker) is borrowed as is.
///
/// One linear pass: each char is appended to the output, and a marker the output
/// then ends with is cut off. The output before each append holds no marker, so
/// after the cut it holds none either (what remains is a prefix of it). The
/// markers cannot overlap one another, so this is the same text as deleting
/// markers until none is left, without a pass per level of nesting.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if !text.bytes().any(|b| b == 0 || b == 0x1b) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\0' => {}
            '~' => {
                out.push(c);
                if out.ends_with(BRACKET_START) || out.ends_with(BRACKET_END) {
                    out.truncate(out.len() - BRACKET_START.len());
                }
            }
            _ => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The length in bytes of `payload` once [`bracketed`].
pub fn bracketed_len(payload: &str) -> usize {
    payload.len() + MARKERS_LEN
}

/// `payload` wrapped in one bracketed-paste block (one allocation).
pub fn bracketed(payload: &str) -> String {
    let mut block = String::with_capacity(bracketed_len(payload));
    block.push_str(BRACKET_START);
    block.push_str(payload);
    block.push_str(BRACKET_END);
    block
}

/// What `payload` costs on the fallback's command line: its UTF-16 units plus
/// one per `"` and per `\` (the worst-case argv quoting expansion).
pub fn fallback_cost(payload: &str) -> usize {
    payload
        .encode_utf16()
        .count()
        .saturating_add(payload.chars().filter(|&c| c == '"' || c == '\\').count())
}

/// The cost of a [`bracketed`] `block` on the fallback when it is over
/// [`FALLBACK_PASTE_LIMIT`], or `None` when it fits one `send-text`. Measured
/// over the payload inside the markers. A payload of more than three bytes per
/// allowed unit is over without counting (a UTF-8 char is at most three bytes
/// per UTF-16 unit); its byte length stands in for the cost then.
pub fn fallback_overage(block: &str) -> Option<usize> {
    let payload = block
        .strip_prefix(BRACKET_START)
        .and_then(|b| b.strip_suffix(BRACKET_END))
        .unwrap_or(block);
    if payload.len() > 3 * FALLBACK_PASTE_LIMIT {
        return Some(payload.len());
    }
    let cost = fallback_cost(payload);
    (cost > FALLBACK_PASTE_LIMIT).then_some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_marker(s: &str) -> bool {
        s.contains(BRACKET_START) || s.contains(BRACKET_END)
    }

    /// Marker removal by deleting markers until none is left (quadratic on
    /// nested splices): the reference [`sanitize`] must agree with.
    fn sanitize_by_repeat(text: &str) -> String {
        let mut out: String = text.chars().filter(|&c| c != '\0').collect();
        while has_marker(&out) {
            out = out.replace(BRACKET_START, "").replace(BRACKET_END, "");
        }
        out
    }

    #[test]
    fn sanitize_matches_repeated_deletion() {
        let cases = [
            "a\x1b[200~b\x1b[201~c\x1b[201~",
            "\x1b[20\x1b[200~0~",
            "x\x1b[20\x1b[20\x1b[201~1~1~y",
            "line1\rline2\r\tind\u{1F600} \x1b[A \x1b[20",
            "a\x1b[Ab",
            "a\0b\0",
            "a\x1b[2\x0000~b",
            "\x1b[200~\x1b[201~\0",
            "a\x1b[201~b\x1b[200~c\x1b[20\x1b[201~1~d",
            "\x1b[\x1b[200~200~~",
            "é\x1b[20\u{1F600}0~\x1b[2\x1b[201~01~",
        ];
        for text in cases {
            assert_eq!(sanitize(text), sanitize_by_repeat(text), "{text:?}");
        }
    }

    #[test]
    fn sanitize_matches_repeated_deletion_over_every_short_text() {
        // Every text of up to six chars over the markers' alphabet (plus NUL and
        // a plain char), so every short way of splicing markers is covered.
        let alphabet = ['\x1b', '[', '2', '0', '1', '~', '\0', 'a'];
        let mut level = vec![String::new()];
        let mut texts = Vec::new();
        for _ in 0..6 {
            level = level
                .iter()
                .flat_map(|t| alphabet.iter().map(move |&c| format!("{t}{c}")))
                .collect();
            texts.extend(level.iter().cloned());
        }
        // Nested splices longer than six chars.
        for k in 1..4 {
            texts.push(format!(
                "{}\x1b[200~{}",
                "\x1b[20".repeat(k),
                "0~".repeat(k)
            ));
            texts.push(format!(
                "{}\x1b[201~{}",
                "\x1b[20".repeat(k),
                "1~".repeat(k)
            ));
        }
        for text in &texts {
            assert_eq!(sanitize(text), sanitize_by_repeat(text), "{text:?}");
        }
    }

    #[test]
    fn sanitize_is_linear_on_deeply_nested_splices() {
        // Deleting markers until none is left takes one pass per level here.
        let k = 50_000;
        let text = format!("x{}\x1b[200~{}y", "\x1b[20".repeat(k), "0~".repeat(k));
        let s = sanitize(&text);
        assert!(!has_marker(&s));
        assert_eq!(s, "xy");
    }

    #[test]
    fn sanitize_removes_both_markers() {
        let s = sanitize("a\x1b[200~b\x1b[201~c\x1b[201~");
        assert_eq!(s, "abc");
    }

    #[test]
    fn sanitize_removes_a_marker_spliced_from_its_neighbours() {
        let s = sanitize("\x1b[20\x1b[200~0~");
        assert!(!has_marker(&s), "{s:?}");
        assert_eq!(s, "");
        let s = sanitize("x\x1b[20\x1b[20\x1b[201~1~1~y");
        assert!(!has_marker(&s), "{s:?}");
        assert_eq!(s, "xy");
    }

    #[test]
    fn sanitize_keeps_other_text_and_control_bytes() {
        let text = "line1\rline2\r\tind\u{1F600} \x1b[A \x1b[20";
        assert_eq!(sanitize(text), text);
    }

    #[test]
    fn sanitize_borrows_text_without_nul_or_esc() {
        assert!(matches!(
            sanitize("plain\r\ttext \u{1F600}"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(sanitize("a\x1b[Ab"), Cow::Owned(_)));
    }

    #[test]
    fn sanitize_drops_nul() {
        assert_eq!(sanitize("a\0b\0"), "ab");
        // A NUL inside a marker does not hide it.
        assert_eq!(sanitize("a\x1b[2\x0000~b"), "ab");
    }

    #[test]
    fn bracketed_wraps_in_one_block() {
        assert_eq!(bracketed("p\rq"), "\x1b[200~p\rq\x1b[201~");
        assert_eq!(bracketed(""), "\x1b[200~\x1b[201~");
        assert_eq!(bracketed_len("p\rq"), bracketed("p\rq").len());
        assert_eq!(bracketed_len("é"), 14);
    }

    #[test]
    fn fallback_cost_counts_utf16_units_and_quoting() {
        assert_eq!(fallback_cost("abc"), 3);
        // An astral char is a surrogate pair.
        assert_eq!(fallback_cost("\u{1F600}"), 2);
        assert_eq!(fallback_cost("é"), 1);
        assert_eq!(fallback_cost("\"a\\"), 5);
    }

    #[test]
    fn fallback_overage_boundary_includes_quote_expansion() {
        assert_eq!(
            fallback_overage(&bracketed(&"a".repeat(FALLBACK_PASTE_LIMIT))),
            None
        );
        let over = "a".repeat(FALLBACK_PASTE_LIMIT + 1);
        assert_eq!(
            fallback_overage(&bracketed(&over)),
            Some(FALLBACK_PASTE_LIMIT + 1)
        );
        // Under the cap raw, over it once each quote counts twice.
        let quotes = "\"".repeat(FALLBACK_PASTE_LIMIT / 2 + 1);
        assert!(quotes.len() < FALLBACK_PASTE_LIMIT);
        assert_eq!(
            fallback_overage(&bracketed(&quotes)),
            Some(fallback_cost(&quotes))
        );
    }

    #[test]
    fn fallback_overage_measures_the_payload_not_the_markers() {
        // The 12 marker bytes do not count against the cap.
        let at_cap = bracketed(&"a".repeat(FALLBACK_PASTE_LIMIT));
        assert!(at_cap.len() > FALLBACK_PASTE_LIMIT);
        assert_eq!(fallback_overage(&at_cap), None);
    }

    #[test]
    fn fallback_overage_short_circuits_a_long_payload() {
        // Three-byte chars at one unit each: exactly at the short-circuit bound
        // the cost is counted (and fits); one byte more is over without counting.
        let wide = "\u{20AC}".repeat(FALLBACK_PASTE_LIMIT);
        assert_eq!(wide.len(), 3 * FALLBACK_PASTE_LIMIT);
        assert_eq!(fallback_overage(&bracketed(&wide)), None);
        let long = format!("{wide}a");
        assert_eq!(fallback_overage(&bracketed(&long)), Some(long.len()));
    }
}
