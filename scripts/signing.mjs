// Pure decision logic for macOS code signing, shared by `stage-sidecars.mjs` (the
// unsigned-bundle warning) and `release.ts` (choosing notarized vs sign-only builds).
//
// Why it matters: macOS privacy grants (Screen Recording in particular) are stored
// against an app's designated requirement. An ad-hoc signed build's requirement is its
// cdhash, which changes on every rebuild, so a grant made to one build never matches the
// next. Signing with a real identity, even a self-signed one, pins the requirement to the
// bundle identifier plus certificate, which survives rebuilds.

export const APPLE_ID_VARS = ["APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"];
export const API_KEY_VARS = ["APPLE_API_KEY", "APPLE_API_ISSUER", "APPLE_API_KEY_PATH"];

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

/**
 * How a macOS release build should be produced from the environment.
 * - `notarized`: a stable identity plus a complete Apple ID or API-key set.
 * - `signed-only`: a stable identity and no notarization variables at all.
 * - `partial-notarization`: some notarization variables set, but no complete set.
 * - `no-identity`: no stable signing identity.
 * @param {Record<string, string | undefined>} env
 * @returns {"notarized" | "signed-only" | "partial-notarization" | "no-identity"}
 */
export function macBuildMode(env) {
  if (!isStableIdentity(env.APPLE_SIGNING_IDENTITY)) return "no-identity";
  const complete = (vars) => vars.every((k) => isSet(env[k]));
  if (complete(APPLE_ID_VARS) || complete(API_KEY_VARS)) return "notarized";
  const any = [...APPLE_ID_VARS, ...API_KEY_VARS].some((k) => isSet(env[k]));
  return any ? "partial-notarization" : "signed-only";
}
