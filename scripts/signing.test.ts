// @ts-nocheck
import { describe, expect, it } from "vitest";

import { isStableIdentity, unsignedBundleWarning } from "./signing.mjs";

const ID = "Developer ID Application: Jane Smith (AB12CD34EF)";

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
