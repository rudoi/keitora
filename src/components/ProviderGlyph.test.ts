import { describe, expect, it } from "vitest";
import { CLI_KINDS } from "../lib/providers";
import { glyphFor } from "./ProviderGlyph";

describe("glyphFor", () => {
  it("labels every provider", () => {
    expect(glyphFor("claude").label).toBe("Claude");
    expect(glyphFor("codex").label).toBe("Codex");
    expect(glyphFor("antigravity").label).toBe("Antigravity");
  });

  it("draws a distinct, non-empty path per provider", () => {
    const paths = CLI_KINDS.map((c) => glyphFor(c).path);
    for (const p of paths) expect(p.length).toBeGreaterThan(0);
    expect(new Set(paths).size).toBe(paths.length);
  });
});
