// App theme: light / dark / system, or a named palette (Nord, Gruvbox, ...). The choice is
// resolved to a `data-theme` attribute (light|dark) on <html> that drives the
// CSS-variable overrides in index.css + App.css; a named palette additionally sets
// `data-palette`, whose overrides live in themes.css. Every named palette is dark.
//
// A single MODULE-LEVEL store owns everything: the persisted preference (localStorage),
// the one `prefers-color-scheme` listener, and the one writer of <html data-theme>.
// `systemDark` is tracked unconditionally, so switching back to "system" never renders a
// stale OS value. React components read the store via useSyncExternalStore — useTheme()
// for the mode + setter, or useResolvedTheme() for read-only consumers like terminal
// panes — so there are no per-component matchMedia listeners or DOM writes.
//
// LOCKSTEP: STORAGE_KEY + the resolution logic are mirrored by the anti-FOUC inline
// script in index.html (which runs before this bundle to set the initial data-theme).
// Keep the two in sync.

import { useSyncExternalStore } from "react";

export const PALETTES = [
  "ayu",
  "catppuccin",
  "gruvbox",
  "hacker",
  "nord",
  "solarized",
  "synthwave",
  "tokyonight",
] as const;
export type Palette = (typeof PALETTES)[number];

export type ThemeMode = "light" | "dark" | "system" | Palette;
export type ResolvedTheme = "light" | "dark";
/** The concrete look in effect: a base theme, or a named palette. */
export type ThemeKey = ResolvedTheme | Palette;

export const THEME_MODES: readonly ThemeMode[] = ["system", "dark", "light", ...PALETTES];

const THEME_LABELS: Record<ThemeMode, string> = {
  system: "System",
  dark: "Dark",
  light: "Light",
  ayu: "Ayu",
  catppuccin: "Catppuccin",
  gruvbox: "Gruvbox",
  hacker: "Hacker",
  nord: "Nord",
  solarized: "Solarized",
  synthwave: "SynthWave '84",
  tokyonight: "Tokyo Night",
};

export function themeLabel(mode: ThemeMode): string {
  return THEME_LABELS[mode];
}

export function isThemeMode(value: unknown): value is ThemeMode {
  return typeof value === "string" && (THEME_MODES as readonly string[]).includes(value);
}

function isPalette(mode: ThemeMode): mode is Palette {
  return (PALETTES as readonly string[]).includes(mode);
}

const STORAGE_KEY = "keitora.theme";
const DEFAULT_MODE: ThemeMode = "system";
const MEDIA_QUERY = "(prefers-color-scheme: dark)";

// Pure: resolve a mode + OS preference to a concrete theme. Exported for unit testing.
export function resolveTheme(mode: ThemeMode, prefersDark: boolean): ResolvedTheme {
  if (mode === "system") return prefersDark ? "dark" : "light";
  return mode === "light" ? "light" : "dark";
}

// Pure: the concrete look for a mode — the palette itself, or the resolved base theme.
export function resolveThemeKey(mode: ThemeMode, prefersDark: boolean): ThemeKey {
  return isPalette(mode) ? mode : resolveTheme(mode, prefersDark);
}

function readMode(): ThemeMode {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    return isThemeMode(raw) ? raw : DEFAULT_MODE;
  } catch {
    return DEFAULT_MODE;
  }
}

function osPrefersDark(): boolean {
  try {
    return window.matchMedia(MEDIA_QUERY).matches;
  } catch {
    return true; // fall back to dark if matchMedia is unavailable
  }
}

// ── Module store ────────────────────────────────────────────────────────────────
let mode: ThemeMode = readMode();
let systemDark = osPrefersDark();
const listeners = new Set<() => void>();

function currentResolved(): ResolvedTheme {
  return resolveTheme(mode, systemDark);
}

function currentKey(): ThemeKey {
  return resolveThemeKey(mode, systemDark);
}

function applyToDom(): void {
  try {
    const root = document.documentElement;
    root.dataset.theme = currentResolved();
    if (isPalette(mode)) root.dataset.palette = mode;
    else delete root.dataset.palette;
  } catch {
    // no DOM (e.g. a test env) — nothing to reflect onto
  }
}

function emit(): void {
  applyToDom();
  for (const l of listeners) l();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

// Set the preference: persist, reflect to <html>, and notify same-window subscribers.
// Other windows converge via the `storage` listener below.
export function setThemeMode(next: ThemeMode): void {
  mode = isThemeMode(next) ? next : DEFAULT_MODE;
  try {
    window.localStorage.setItem(STORAGE_KEY, mode);
  } catch {
    // preference just won't persist this session
  }
  emit();
}

// One OS-preference listener for the whole app; keeps `systemDark` current regardless of
// the active mode, so a later switch to "system" resolves correctly with no stale frame.
try {
  window.matchMedia(MEDIA_QUERY).addEventListener("change", (e) => {
    systemDark = e.matches;
    emit();
  });
} catch {
  // matchMedia unavailable — systemDark stays at its initial value
}

// Cross-window sync: another window changing the preference updates this one.
try {
  window.addEventListener("storage", (e) => {
    if (e.key === STORAGE_KEY) {
      mode = readMode();
      emit();
    }
  });
} catch {
  // no window (tests)
}

// Reflect the initial theme immediately (matches the anti-FOUC script; a harmless re-set).
applyToDom();

// ── React bindings ──────────────────────────────────────────────────────────────

// Read-only resolved theme (light|dark) — for consumers that only render colors, e.g.
// the xterm palette. Snapshot is a primitive, so it's referentially stable when unchanged.
export function useResolvedTheme(): ResolvedTheme {
  return useSyncExternalStore(subscribe, currentResolved, currentResolved);
}

// The concrete look (base theme or palette) — for consumers that carry their own colors
// per palette, e.g. the xterm ANSI palette.
export function useThemeKey(): ThemeKey {
  return useSyncExternalStore(subscribe, currentKey, currentKey);
}

// Full theme control: current mode, a setter, and the resolved theme.
export function useTheme(): {
  mode: ThemeMode;
  setMode: (mode: ThemeMode) => void;
  resolved: ResolvedTheme;
} {
  const getMode = () => mode;
  const m = useSyncExternalStore(subscribe, getMode, getMode);
  const resolved = useResolvedTheme();
  return { mode: m, setMode: setThemeMode, resolved };
}
