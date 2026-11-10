#!/usr/bin/env tsx
// Platform-aware release tool. Builds the current platform's Tauri bundle and
// publishes it to a GitHub release keyed by `v{version}`, optionally revving the
// semantic version across the three app version files first.
//
// Pure logic (tested) is
// in ./release.lib.ts; this file is the IO / process orchestration.
//
//   yarn release                 # fill the v{current} release with this platform's artifact
//   yarn release --patch         # bump patch, commit+tag+push, create+fill release
//   yarn release --minor|--major
//   yarn release --set 1.4.0     # set an explicit version
//   yarn release --overwrite     # replace an existing same-platform asset (no-bump)
//   yarn release --dry-run       # print every action, mutate nothing

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import {
  assetMatches,
  bumpVersion,
  compareVersions,
  decideUpload,
  needsShell,
  parseArgs,
  platformArtifactPattern,
  type ReleaseOptions,
  tauriArchToken,
} from "./release.lib.ts";
import { API_KEY_VARS, APPLE_ID_VARS, macBuildMode } from "./signing.mjs";

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

const VERSION_FILES = {
  packageJson: join(REPO_ROOT, "package.json"),
  tauriConf: join(REPO_ROOT, "src-tauri", "tauri.conf.json"),
  cargoToml: join(REPO_ROOT, "src-tauri", "Cargo.toml"),
};

// Cargo.lock that records the app crate's version and must be bumped in lockstep.
const LOCKFILES = [join(REPO_ROOT, "Cargo.lock")];
// Repo-relative paths for the commit pathspec.
const VERSION_FILE_PATHS = ["package.json", "src-tauri/tauri.conf.json", "src-tauri/Cargo.toml"];
const LOCKFILE_PATHS = ["Cargo.lock"];

// ── process helpers ──────────────────────────────────────────────────────────

function logCmd(prefix: string, cmd: string, args: string[]): void {
  console.log(`${prefix} ${cmd} ${args.join(" ")}`);
}

/** Run a state-mutating command. Under dry-run, log and skip. Throws on failure. */
function runMutating(cmd: string, args: string[], dryRun: boolean): void {
  if (dryRun) {
    logCmd("[dry-run]", cmd, args);
    return;
  }
  logCmd("+", cmd, args);
  // shell only where required (yarn on Windows). Using a shell would word-split
  // args containing spaces (commit messages, paths) — see needsShell().
  const r = spawnSync(cmd, args, { stdio: "inherit", shell: needsShell(cmd, process.platform), cwd: REPO_ROOT });
  if (r.status !== 0) {
    throw new Error(`Command failed (exit ${r.status ?? "?"}): ${cmd} ${args.join(" ")}`);
  }
}

/** Run a read-only command and capture stdout. */
function capture(cmd: string, args: string[]): { status: number; stdout: string } {
  const r = spawnSync(cmd, args, {
    encoding: "utf8",
    shell: needsShell(cmd, process.platform),
    cwd: REPO_ROOT,
  });
  return { status: r.status ?? 1, stdout: r.stdout ?? "" };
}

function confirm(question: string): Promise<boolean> {
  if (!process.stdin.isTTY) return Promise.resolve(false);
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) =>
    rl.question(`${question} [y/N] `, (a) => {
      rl.close();
      resolve(/^y(es)?$/i.test(a.trim()));
    }),
  );
}

// ── version IO ───────────────────────────────────────────────────────────────

function readCurrentVersion(): string {
  const pkg = JSON.parse(readFileSync(VERSION_FILES.packageJson, "utf8"));
  if (typeof pkg.version !== "string") throw new Error("package.json has no version field");
  return pkg.version;
}

function replaceFirst(path: string, pattern: RegExp, replacement: string, dryRun: boolean): void {
  const before = readFileSync(path, "utf8");
  // Distinguish "pattern not present" (a real error) from "value already at target"
  // (a harmless no-op) so we never emit a misleading error or partially rewrite.
  if (!pattern.test(before)) throw new Error(`Could not find version to update in ${path}`);
  const after = before.replace(pattern, replacement);
  if (dryRun) {
    console.log(`[dry-run] write ${path}`);
    return;
  }
  if (after === before) {
    console.log(`= ${path} already at target`);
    return;
  }
  writeFileSync(path, after);
  console.log(`+ wrote ${path}`);
}

/**
 * Sync the app crate's version in the Cargo.lock (a version-only bump of a
 * workspace member touches exactly its own `version =` line; nothing depends on
 * the app crate). Returns the repo-relative paths that changed so they can be
 * committed alongside the manifests.
 */
function syncLockfiles(next: string, dryRun: boolean): string[] {
  const changed: string[] = [];
  for (let i = 0; i < LOCKFILES.length; i++) {
    const lock = LOCKFILES[i];
    if (!existsSync(lock)) continue;
    const before = readFileSync(lock, "utf8");
    const after = before.replace(/(name = "keitora"\r?\nversion = ")[^"]+(")/, `$1${next}$2`);
    if (after === before) continue;
    changed.push(LOCKFILE_PATHS[i]);
    if (dryRun) {
      console.log(`[dry-run] write ${lock}`);
    } else {
      writeFileSync(lock, after);
      console.log(`+ wrote ${lock}`);
    }
  }
  return changed;
}

/** Propagate the new version to all three app version files (formatting preserved). */
function writeVersion(next: string, dryRun: boolean): void {
  // package.json / tauri.conf.json: first `"version": "..."` (the app version, line ~4).
  replaceFirst(VERSION_FILES.packageJson, /("version":\s*")[^"]+(")/, `$1${next}$2`, dryRun);
  replaceFirst(VERSION_FILES.tauriConf, /("version":\s*")[^"]+(")/, `$1${next}$2`, dryRun);
  // Cargo.toml: first line-anchored `version = "..."` — the [package] version, which
  // precedes every dependency section in src-tauri/Cargo.toml.
  replaceFirst(VERSION_FILES.cargoToml, /^version = "[^"]+"/m, `version = "${next}"`, dryRun);
}

// ── git helpers ──────────────────────────────────────────────────────────────

function gitHeadSha(): string {
  const r = capture("git", ["rev-parse", "HEAD"]);
  if (r.status !== 0) throw new Error("Could not resolve HEAD commit");
  return r.stdout.trim();
}

/** Whether a commit is reachable from any remote-tracking branch (i.e. pushed). */
function commitIsOnRemote(sha: string): boolean {
  const r = capture("git", ["branch", "-r", "--contains", sha]);
  return r.status === 0 && r.stdout.trim().length > 0;
}

// ── build + artifact discovery ───────────────────────────────────────────────

function bundleDir(platform: string): string {
  const sub = platform === "darwin" ? "dmg" : "nsis";
  return join(REPO_ROOT, "target", "release", "bundle", sub);
}

function cleanBundleDir(platform: string, dryRun: boolean): void {
  const dir = bundleDir(platform);
  if (dryRun) {
    console.log(`[dry-run] rm -rf ${dir}`);
    return;
  }
  rmSync(dir, { recursive: true, force: true });
  console.log(`+ cleaned ${dir}`);
}

/**
 * Decide how the macOS dmg is built, before anything is bumped, tagged or pushed: a
 * signing problem must stop the release while nothing has changed. Returns the extra
 * `build-dmg.sh` arguments. In a dry run, problems are reported instead of thrown.
 */
function macBuildFlags(dryRun: boolean): string[] {
  const mode = macBuildMode(process.env);
  const problem =
    mode === "no-identity"
      ? "APPLE_SIGNING_IDENTITY must name a code-signing identity (not '-'). " +
        'See CONTRIBUTING.md, "Signed macOS builds".'
      : mode === "partial-notarization"
        ? `Notarization variables are only partly set. Set all of ${APPLE_ID_VARS.join(", ")}, ` +
          `or all of ${API_KEY_VARS.join(", ")}, or none of them for a sign-only build.`
        : null;
  if (problem) {
    if (!dryRun) throw new Error(problem);
    console.warn(`[dry-run] a real release would stop here: ${problem}`);
  }
  if (mode === "signed-only") {
    console.warn(
      "\n*** WARNING: no notarization credentials. The DMG will be signed but NOT notarized.\n" +
        "*** Downloaders must allow it once under System Settings > Privacy & Security > Open Anyway.\n",
    );
    return ["--sign-only"];
  }
  return [];
}

function build(platform: string, macFlags: string[], dryRun: boolean): void {
  if (platform === "win32") {
    runMutating("yarn", ["tauri:bundle"], dryRun);
  } else {
    // macOS: always signed with a stable identity (so privacy grants survive upgrades),
    // notarized when credentials are present. Overlay-aware (see scripts/build-dmg.sh).
    runMutating("bash", ["scripts/build-dmg.sh", ...macFlags], dryRun);
  }
}

/** Find the single artifact matching this platform+version+arch. */
function discoverArtifact(
  platform: string,
  version: string,
  archToken: string,
  dryRun: boolean,
): string {
  const dir = bundleDir(platform);
  const pattern = platformArtifactPattern(platform, version, archToken);
  const ext = platform === "darwin" ? ".dmg" : "-setup.exe";
  const synthetic = join(dir, `Keitora_${version}_${archToken}${ext}`);

  if (!existsSync(dir)) {
    if (dryRun) {
      console.log(`[dry-run] would discover artifact matching ${pattern} in ${dir}`);
      return synthetic;
    }
    throw new Error(`Bundle directory not found: ${dir}`);
  }
  const matches = readdirSync(dir).filter((f) => assetMatches(f, pattern));
  if (matches.length === 1) return join(dir, matches[0]);
  if (matches.length === 0) {
    if (dryRun) {
      console.log(`[dry-run] no artifact yet; would expect ${pattern} in ${dir}`);
      return synthetic;
    }
    throw new Error(`No artifact matching ${pattern} in ${dir}`);
  }
  if (dryRun) {
    // A real run cleans the dir first; under dry-run stale files may linger. Warn, don't crash.
    console.log(`[dry-run] multiple artifacts match ${pattern} in ${dir}: ${matches.join(", ")} (a real run cleans first)`);
    return synthetic;
  }
  throw new Error(`Multiple artifacts match ${pattern} in ${dir}: ${matches.join(", ")}`);
}

// ── gh release ───────────────────────────────────────────────────────────────

interface ReleaseInfo {
  exists: boolean;
  assets: string[];
}

function viewRelease(tag: string): ReleaseInfo {
  const r = capture("gh", ["release", "view", tag, "--json", "tagName,assets"]);
  if (r.status !== 0) return { exists: false, assets: [] };
  try {
    const json = JSON.parse(r.stdout);
    const assets = Array.isArray(json.assets) ? json.assets.map((a: { name: string }) => a.name) : [];
    return { exists: true, assets };
  } catch {
    return { exists: false, assets: [] };
  }
}

function releaseUrl(tag: string): string | null {
  const r = capture("gh", ["release", "view", tag, "--json", "url", "-q", ".url"]);
  return r.status === 0 ? r.stdout.trim() : null;
}

// ── main ─────────────────────────────────────────────────────────────────────

function printHelp(): void {
  console.log(`yarn release [options]

Builds this platform's Tauri bundle and uploads it to the GitHub release v{version}.

  (no flags)     Fill the current release (v{current}) with this platform's artifact.
                 Creates the release at the current version if it doesn't exist.
  --patch        Bump patch (x.y.Z), commit + tag + push, create+fill the new release.
  --minor        Bump minor (x.Y.0).
  --major        Bump major (X.0.0).
  --set X.Y.Z    Set an explicit version.
  --overwrite    Replace an existing same-platform asset (no-bump mode only).
  --dry-run      Print every git/gh/build action without executing any of them.
  --help, -h     Show this help.

Version bumps are propagated to package.json, src-tauri/tauri.conf.json, and
src-tauri/Cargo.toml. Run on each platform to fill that platform's artifact into
the same release (e.g. Windows NSIS, then macOS dmg).

macOS needs APPLE_SIGNING_IDENTITY. With a complete set of notarization variables
the dmg is notarized; with none it is signed only (downloaders must allow it once
under System Settings > Privacy & Security > Open Anyway). See CONTRIBUTING.md, "Signed macOS builds".`);
}

async function main(): Promise<void> {
  let opts: ReleaseOptions;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (e) {
    console.error(`error: ${(e as Error).message}\n`);
    printHelp();
    process.exit(2);
  }
  if (opts.help) {
    printHelp();
    return;
  }

  const platform = process.platform;
  if (platform !== "darwin" && platform !== "win32") {
    throw new Error(`Unsupported platform "${platform}" (this tool targets macOS and Windows)`);
  }
  const archToken = tauriArchToken(process.arch);
  const current = readCurrentVersion();
  const macFlags = platform === "darwin" ? macBuildFlags(opts.dryRun) : [];

  // ── 1. optional version rev ──
  let targetVersion = current;
  const revving = Boolean(opts.bump || opts.setVersion);
  if (revving) {
    const next = opts.setVersion ?? bumpVersion(current, opts.bump as "major" | "minor" | "patch");
    if (compareVersions(next, current) <= 0) {
      const msg = `Requested version ${next} is not greater than current ${current}.`;
      if (opts.dryRun) {
        console.warn(`[dry-run] ${msg} (would require confirmation)`);
      } else if (!(await confirm(`${msg} Proceed anyway?`))) {
        throw new Error("Aborted: version not increased (pass a higher version or confirm interactively).");
      }
    }
    console.log(`==> Revving version ${current} -> ${next}`);
    writeVersion(next, opts.dryRun);
    const changedLocks = syncLockfiles(next, opts.dryRun);
    runMutating(
      "git",
      ["commit", "-m", `chore(release): v${next}`, "--", ...VERSION_FILE_PATHS, ...changedLocks],
      opts.dryRun,
    );
    runMutating("git", ["tag", `v${next}`], opts.dryRun);
    runMutating("git", ["push", "origin", "HEAD"], opts.dryRun);
    runMutating("git", ["push", "origin", `v${next}`], opts.dryRun);
    targetVersion = next;
  }

  const tag = `v${targetVersion}`;

  // ── 2. build + discover ──
  console.log(`==> Building ${platform} (${archToken}) bundle for ${tag}`);
  cleanBundleDir(platform, opts.dryRun);
  build(platform, macFlags, opts.dryRun);
  const artifact = discoverArtifact(platform, targetVersion, archToken, opts.dryRun);
  console.log(`==> Artifact: ${artifact}`);

  // ── 3. reconcile release ──
  const info = viewRelease(tag);
  if (!info.exists) {
    // GitHub creates the tag from a resolved commit SHA (target_commitish); the
    // literal "HEAD" is not a valid remote target, and the commit must exist on
    // the remote. In rev mode we just pushed HEAD + the tag; in no-bump mode we
    // must ensure HEAD is pushed before creating the release from it.
    const sha = gitHeadSha();
    if (!revving && !opts.dryRun && !commitIsOnRemote(sha)) {
      throw new Error(
        `Release ${tag} does not exist and HEAD (${sha.slice(0, 9)}) is not on origin. ` +
          "Push the commit first, or run with a version bump (--patch/--minor/--major).",
      );
    }
    console.log(`==> Release ${tag} does not exist; creating from ${sha.slice(0, 9)}`);
    runMutating(
      "gh",
      ["release", "create", tag, "--title", tag, "--generate-notes", "--target", sha],
      opts.dryRun,
    );
  } else {
    console.log(`==> Release ${tag} exists with ${info.assets.length} asset(s)`);
  }

  // ── 4. upload ──
  const pattern = platformArtifactPattern(platform, targetVersion, archToken);
  const assetPresent = info.exists && info.assets.some((a) => assetMatches(a, pattern));
  const decision = decideUpload(assetPresent, opts.overwrite);
  if (decision === "skip") {
    const existing = info.assets.find((a) => assetMatches(a, pattern));
    console.log(`==> ${tag} already has ${existing}; use --overwrite to replace. Nothing to do.`);
  } else {
    const uploadArgs = ["release", "upload", tag, artifact];
    if (decision === "clobber") uploadArgs.push("--clobber");
    runMutating("gh", uploadArgs, opts.dryRun);
    console.log(`==> Uploaded ${artifact} to ${tag}${decision === "clobber" ? " (replaced)" : ""}`);
  }

  const url = releaseUrl(tag);
  if (url) console.log(`\nRelease: ${url}`);
  else console.log(`\nRelease: ${tag}${opts.dryRun ? " (dry-run; not created)" : ""}`);
}

main().catch((e) => {
  console.error(`\nrelease failed: ${(e as Error).message}`);
  process.exit(1);
});
