// Shared platform detection flags.

// Desktop OS flag. On Windows, WebView2 delivers a native paste to xterm's textarea
// that our attaching gate can't see, so TerminalPane owns the Ctrl+V/Ctrl+C
// clipboard chords explicitly AND suppresses the native paste duplicate — all gated on
// this. Off Windows the native paste event is intercepted and sent through the same
// paste path instead of xterm's. See TerminalPane's onNativePaste and
// src/lib/terminalClipboard.ts.
export const isWindows = /Windows/i.test(navigator.userAgent);

export const isMac = /Mac/i.test(navigator.userAgent);

// User-facing label for the primary modifier (the keymap's "Cmd" = meta on macOS,
// ctrl elsewhere). Use this anywhere a chord is shown to the user (cheatsheet, hint
// overlays) so Windows/Linux read "Ctrl" instead of the Mac ⌘ glyph.
export const modKeyLabel = isMac ? "⌘" : "Ctrl";
