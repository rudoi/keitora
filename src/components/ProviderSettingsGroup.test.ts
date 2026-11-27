import { describe, expect, it } from "vitest";
import { CLI_KINDS } from "../lib/providers";
import {
  notFoundCopy,
  PROVIDER_COPY,
  projectForcesCopy,
  providerGroupState,
} from "./ProviderSettingsGroup";

describe("providerGroupState", () => {
  it.each(CLI_KINDS)("unavailable %s disables autonomy and the editor, not the paths", (cli) => {
    const st = providerGroupState(cli, false, true);
    expect(st.autonomyDisabled).toBe(true);
    expect(st.showNotFound).toBe(true);
    expect(st.showEditor).toBe(false);
    expect(st.projectPathDisabled).toBe(false);
  });

  it("a missing committed file disables only the project path and shows the note", () => {
    for (const available of [true, false]) {
      const st = providerGroupState("claude", available, false);
      expect(st.projectPathDisabled).toBe(true);
      expect(st.showCommitNote).toBe(true);
    }
  });

  it("available with a committed file disables nothing", () => {
    expect(providerGroupState("antigravity", true, true)).toEqual({
      autonomyDisabled: false,
      projectPathDisabled: false,
      showNotFound: false,
      showCommitNote: false,
      showEditor: true,
    });
  });
});

describe("PROVIDER_COPY", () => {
  it("has an entry for every provider", () => {
    expect(Object.keys(PROVIDER_COPY).sort()).toEqual([...CLI_KINDS].sort());
  });
});

describe("derived copy", () => {
  it("keeps every provider's wording", () => {
    expect(notFoundCopy("claude")).toBe(
      "Claude not found on PATH — install it, then reopen Settings.",
    );
    expect(notFoundCopy("codex")).toBe(
      "Codex not found on PATH — install it, then reopen Settings.",
    );
    expect(notFoundCopy("antigravity")).toBe(
      "Antigravity not found — install it, then reopen Settings.",
    );
    expect(projectForcesCopy("claude")).toBe("enables skip-permissions");
    expect(projectForcesCopy("codex")).toBe("enables Codex autonomy");
    expect(projectForcesCopy("antigravity")).toBe("enables Antigravity autonomy");
  });
});
