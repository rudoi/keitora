import { describe, expect, it } from "vitest";
import { TERMINAL_THEMES } from "./terminalThemes";
import {
  isThemeMode,
  PALETTES,
  resolveTheme,
  resolveThemeKey,
  THEME_MODES,
  themeLabel,
} from "./theme";

describe("resolveTheme", () => {
  it("follows the OS for system", () => {
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
  });

  it("keeps explicit light and dark regardless of the OS", () => {
    expect(resolveTheme("light", true)).toBe("light");
    expect(resolveTheme("dark", false)).toBe("dark");
  });

  it("resolves every named palette to dark", () => {
    for (const p of PALETTES) {
      expect(resolveTheme(p, false)).toBe("dark");
    }
  });
});

describe("resolveThemeKey", () => {
  it("returns the palette itself for a named palette", () => {
    expect(resolveThemeKey("nord", false)).toBe("nord");
  });

  it("returns the resolved base theme otherwise", () => {
    expect(resolveThemeKey("system", false)).toBe("light");
    expect(resolveThemeKey("dark", false)).toBe("dark");
  });
});

describe("isThemeMode", () => {
  it("accepts every listed mode and rejects anything else", () => {
    for (const m of THEME_MODES) expect(isThemeMode(m)).toBe(true);
    expect(isThemeMode("solarised")).toBe(false);
    expect(isThemeMode(null)).toBe(false);
  });
});

describe("theme catalogue", () => {
  it("labels every mode", () => {
    for (const m of THEME_MODES) expect(themeLabel(m)).toBeTruthy();
  });

  it("has a terminal palette for every theme key", () => {
    for (const key of ["dark", "light", ...PALETTES] as const) {
      expect(TERMINAL_THEMES[key]?.background).toBeTruthy();
    }
  });
});
