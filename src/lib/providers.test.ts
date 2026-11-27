import { describe, expect, it } from "vitest";
import {
  byCli,
  CLI_KINDS,
  cliBinary,
  cliTitle,
  initialAvailability,
  parseCliKind,
  switchTargets,
  TRANSFER_PAIRS,
  transferTargets,
  withClaudeAvailable,
} from "./providers";

describe("availability", () => {
  it("starts with only Claude available", () => {
    expect(initialAvailability()).toEqual({ claude: true, codex: false, antigravity: false });
  });

  it("keeps Claude available whatever the backend reports", () => {
    expect(withClaudeAvailable({ claude: false, codex: true, antigravity: false })).toEqual({
      claude: true,
      codex: true,
      antigravity: false,
    });
  });
});

describe("providers", () => {
  it("CLI_KINDS is claude, codex, antigravity in that order", () => {
    expect(CLI_KINDS).toEqual(["claude", "codex", "antigravity"]);
  });

  it("cliTitle maps every provider", () => {
    expect(CLI_KINDS.map(cliTitle)).toEqual(["Claude", "Codex", "Antigravity"]);
  });

  it("cliBinary maps every provider", () => {
    expect(CLI_KINDS.map(cliBinary)).toEqual(["claude", "codex", "agy"]);
  });

  it("transferTargets", () => {
    expect(transferTargets("claude")).toEqual(["codex"]);
    expect(transferTargets("codex")).toEqual(["claude"]);
    expect(transferTargets("antigravity")).toEqual([]);
  });

  it("switchTargets", () => {
    expect(switchTargets("claude")).toEqual(["antigravity"]);
    expect(switchTargets("codex")).toEqual(["antigravity"]);
    expect(switchTargets("antigravity")).toEqual(["claude", "codex"]);
  });

  it("TRANSFER_PAIRS is the only source", () => {
    for (const [from, to] of TRANSFER_PAIRS) expect(transferTargets(from)).toContain(to);
    for (const from of CLI_KINDS) {
      const covered = [from, ...transferTargets(from), ...switchTargets(from)];
      expect([...covered].sort()).toEqual([...CLI_KINDS].sort());
      expect(new Set(covered).size).toBe(covered.length);
    }
  });
});

describe("parseCliKind", () => {
  it("accepts provider names and rejects anything else", () => {
    for (const c of CLI_KINDS) expect(parseCliKind(c)).toBe(c);
    expect(parseCliKind("gemini")).toBeNull();
    expect(parseCliKind("")).toBeNull();
    expect(parseCliKind(null)).toBeNull();
    expect(parseCliKind(undefined)).toBeNull();
  });
});

describe("byCli", () => {
  it("builds one entry per provider in display order", () => {
    const r = byCli((c) => c.length);
    expect(Object.keys(r)).toEqual([...CLI_KINDS]);
    expect(r).toEqual({ claude: 6, codex: 5, antigravity: 11 });
  });
});
