// Pure decision logic for macOS code signing, used by `stage-sidecars.mjs` for the
// unsigned-bundle warning.
//
// Why it matters: macOS privacy grants (Screen Recording in particular) are stored
// against an app's designated requirement. An ad-hoc signed build's requirement is its
// cdhash, which changes on every rebuild, so a grant made to one build never matches the
// next. Signing with a real identity, even a self-signed one, pins the requirement to the
// bundle identifier plus certificate, which survives rebuilds.

/** @param {string | undefined} v */
function isSet(v) {
  return typeof v === "string" && v.trim().length > 0;
}

/**
 * Whether a signing identity yields a designated requirement that is stable across
 * rebuilds. `-` (ad-hoc) does not: its requirement is the binary's cdhash.
 * @param {string | undefined} id
 */
export function isStableIdentity(id) {
  return isSet(id) && id.trim() !== "-";
}

/**
 * The warning `tauri:bundle` prints when a macOS bundle will be ad-hoc signed, or
 * `null` when there is nothing to warn about.
 * @param {string} platform `process.platform`
 * @param {Record<string, string | undefined>} env
 * @returns {string | null}
 */
export function unsignedBundleWarning(platform, env) {
  if (platform !== "darwin" || isStableIdentity(env.APPLE_SIGNING_IDENTITY)) return null;
  return [
    "warning: APPLE_SIGNING_IDENTITY is not set, so this bundle will be ad-hoc signed.",
    "  macOS privacy grants (Screen Recording for agent screenshots) will not survive the",
    "  next build. Set APPLE_SIGNING_IDENTITY to a code-signing identity, or create one",
    "  with scripts/macos-local-signing-cert.sh. See CONTRIBUTING.md, \"macOS permissions\".",
  ].join("\n");
}
