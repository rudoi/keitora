// @ts-nocheck
import { describe, expect, it } from "vitest";

import { isStableIdentity, macBuildMode, unsignedBundleWarning } from "./signing.mjs";

const ID = "Developer ID Application: Jane Smith (AB12CD34EF)";
const APPLE_ID_SET = { APPLE_ID: "user@example.com", APPLE_PASSWORD: "pw", APPLE_TEAM_ID: "AB12CD34EF" };
const API_KEY_SET = {
  APPLE_API_KEY: "ABCD123456",
  APPLE_API_ISSUER: "00000000-0000-0000-0000-000000000000",
  APPLE_API_KEY_PATH: "/Users/me/keys/AuthKey.p8",
};

describe("isStableIdentity", () => {
  it.each([undefined, "", "   ", "-", " - "])("rejects %j", (id) => {
    expect(isStableIdentity(id)).toBe(false);
  });
  it("accepts a named identity", () => {
    expect(isStableIdentity(ID)).toBe(true);
    expect(isStableIdentity("Keitora Local Signing")).toBe(true);
  });
});

describe("unsignedBundleWarning", () => {
  it("warns on darwin without a stable identity", () => {
    expect(unsignedBundleWarning("darwin", {})).toMatch(/ad-hoc signed/);
    expect(unsignedBundleWarning("darwin", { APPLE_SIGNING_IDENTITY: "-" })).toMatch(/Screen Recording/);
  });
  it("is silent on darwin with a stable identity", () => {
    expect(unsignedBundleWarning("darwin", { APPLE_SIGNING_IDENTITY: ID })).toBeNull();
  });
  it.each(["win32", "linux"])("is silent on %s", (platform) => {
    expect(unsignedBundleWarning(platform, {})).toBeNull();
  });
});

describe("macBuildMode", () => {
  const withId = (extra = {}) => ({ APPLE_SIGNING_IDENTITY: ID, ...extra });

  it("needs a stable identity first", () => {
    expect(macBuildMode({})).toBe("no-identity");
    expect(macBuildMode({ APPLE_SIGNING_IDENTITY: "-", ...APPLE_ID_SET })).toBe("no-identity");
    expect(macBuildMode({ APPLE_SIGNING_IDENTITY: "  " })).toBe("no-identity");
  });
  it.each([
    ["an Apple ID set", APPLE_ID_SET],
    ["an API-key set", API_KEY_SET],
    ["both sets", { ...APPLE_ID_SET, ...API_KEY_SET }],
  ])("notarizes with %s", (_, vars) => {
    expect(macBuildMode(withId(vars))).toBe("notarized");
  });
  it("signs only when no notarization variables are set", () => {
    expect(macBuildMode(withId())).toBe("signed-only");
  });
  it("treats blank variables as unset", () => {
    expect(macBuildMode(withId({ APPLE_ID: "", APPLE_API_KEY: "  " }))).toBe("signed-only");
  });
  it.each([...Object.keys(APPLE_ID_SET), ...Object.keys(API_KEY_SET)])(
    "rejects a lone %s as partial",
    (k) => {
      expect(macBuildMode(withId({ [k]: "x" }))).toBe("partial-notarization");
    },
  );
  it("rejects an incomplete set even next to a partial other set", () => {
    const { APPLE_TEAM_ID: _, ...noTeam } = APPLE_ID_SET;
    expect(macBuildMode(withId({ ...noTeam, APPLE_API_KEY: "k" }))).toBe("partial-notarization");
  });
});
