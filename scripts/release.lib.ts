// Pure, side-effect-free logic for the release tool. Everything here is unit-tested
// (scripts/release.lib.test.ts). IO and process orchestration live in scripts/release.ts.

export type BumpLevel = "major" | "minor" | "patch";

export interface Version {
  major: number;
  minor: number;
  patch: number;
}

export interface ReleaseOptions {
  bump?: BumpLevel;
  setVersion?: string;
  dryRun: boolean;
  skipCiCheck: boolean;
  help: boolean;
}

/** Repo-relative paths the release commit touches, all carrying the app version. */
export const BUMPED_FILES: readonly string[] = [
  "package.json",
  "src-tauri/tauri.conf.json",
  "src-tauri/Cargo.toml",
  "Cargo.lock",
];

/** The app crate's name in `Cargo.lock` (other workspace members are versioned independently). */
export const APP_CRATE = "keitora";

/** The branch releases are cut from. */
export const RELEASE_BRANCH = "main";

/** The CI workflow whose result for `HEAD` gates a release. */
export const CI_WORKFLOW = "ci.yml";

// ── versions ────────────────────────────────────────────────────────────────

const VERSION_RE = /^(\d+)\.(\d+)\.(\d+)$/;

/** Parse a strict `X.Y.Z` (no `v` prefix, no pre-release suffix), throwing on anything else. */
export function parseVersion(v: string): Version {
  const m = VERSION_RE.exec(v.trim());
  if (!m) throw new Error(`Invalid version "${v}": expected X.Y.Z`);
  return { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]) };
}

export function formatVersion(v: Version): string {
  return `${v.major}.${v.minor}.${v.patch}`;
}

/** Increment by the given level (minor resets patch; major resets both). */
export function bumpVersion(current: string, level: BumpLevel): string {
  const v = parseVersion(current);
  switch (level) {
    case "major":
      return formatVersion({ major: v.major + 1, minor: 0, patch: 0 });
    case "minor":
      return formatVersion({ major: v.major, minor: v.minor + 1, patch: 0 });
    case "patch":
      return formatVersion({ major: v.major, minor: v.minor, patch: v.patch + 1 });
  }
}

/** Validate and normalise an explicit `--set` version. */
export function parseSetVersion(v: string): string {
  return formatVersion(parseVersion(v));
}

/** Negative if a<b, 0 if equal, positive if a>b. */
export function compareVersions(a: string, b: string): number {
  const pa = parseVersion(a);
  const pb = parseVersion(b);
  if (pa.major !== pb.major) return pa.major - pb.major;
  if (pa.minor !== pb.minor) return pa.minor - pb.minor;
  return pa.patch - pb.patch;
}

/**
 * The version to release: the explicit `--set`, or `current` bumped by the flag (`--patch` when
 * none is given, which only a dry run allows). Throws unless the result is greater than `current`.
 */
export function nextVersion(current: string, opts: Pick<ReleaseOptions, "bump" | "setVersion">): string {
  const next = opts.setVersion ?? bumpVersion(current, opts.bump ?? "patch");
  if (compareVersions(next, current) <= 0) {
    throw new Error(`Requested version ${next} is not greater than current ${current}`);
  }
  return next;
}

export function tagFor(version: string): string {
  return `v${version}`;
}

export function commitMessage(version: string): string {
  return `chore(release): v${version}`;
}

// ── arguments ───────────────────────────────────────────────────────────────

/**
 * Parse argv (already sliced past node and the script). Throws on unknown or conflicting flags,
 * and when neither a bump flag nor `--set` is given outside `--dry-run` and `--help`. Every throw
 * is a usage error (exit 2).
 */
export function parseArgs(argv: string[]): ReleaseOptions {
  const opts: ReleaseOptions = { dryRun: false, skipCiCheck: false, help: false };
  const bumpFlags: BumpLevel[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case "--major":
      case "--minor":
      case "--patch":
        bumpFlags.push(arg.slice(2) as BumpLevel);
        break;
      case "--set": {
        const next = argv[i + 1];
        if (!next || next.startsWith("-")) {
          throw new Error("--set requires a version argument (X.Y.Z)");
        }
        if (opts.setVersion !== undefined) throw new Error("--set given more than once");
        opts.setVersion = parseSetVersion(next);
        i++;
        break;
      }
      case "--dry-run":
        opts.dryRun = true;
        break;
      case "--skip-ci-check":
        opts.skipCiCheck = true;
        break;
      case "--help":
      case "-h":
        opts.help = true;
        break;
      default:
        throw new Error(`Unknown argument "${arg}"`);
    }
  }
  if (bumpFlags.length > 1) {
    throw new Error(`Conflicting bump flags: ${bumpFlags.map((b) => `--${b}`).join(" ")}`);
  }
  if (bumpFlags.length === 1 && opts.setVersion) {
    throw new Error("Cannot combine --set with --major/--minor/--patch");
  }
  if (bumpFlags.length === 1) opts.bump = bumpFlags[0];
  if (!opts.help && !opts.dryRun && !opts.bump && !opts.setVersion) {
    throw new Error("pass --patch, --minor, --major or --set X.Y.Z");
  }
  return opts;
}

export function helpText(): string {
  return `yarn release [options]

Bumps the app version, commits "chore(release): vX.Y.Z", tags it and pushes main and the tag in
one atomic push. The tag push runs release.yml, which builds and publishes the installers. This
script never builds or publishes anything itself.

Run it on an up-to-date, clean main whose latest ${CI_WORKFLOW} run passed.

  --patch          Bump patch (x.y.Z).
  --minor          Bump minor (x.Y.0).
  --major          Bump major (X.0.0).
  --set X.Y.Z      Set an explicit version; it must be greater than the current one.
  --dry-run        Print every step and change nothing; usable from any branch
                   (previews --patch when no bump flag is given).
  --skip-ci-check  Release without a passing ${CI_WORKFLOW} run for HEAD (for example before
                   CI has any history, or while Actions is down).
  --help, -h       Show this help.

Bumped files: ${BUMPED_FILES.join(", ")}`;
}

// ── version files ───────────────────────────────────────────────────────────

/** End index (of the closing quote) of the JSON string whose opening quote is at `start`. */
function jsonStringEnd(text: string, start: number): number {
  let j = start + 1;
  while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
  if (j >= text.length) throw new Error("unterminated JSON string");
  return j;
}

/**
 * The span (inside the quotes) of the string value of top-level `key` in a JSON object's text,
 * or null when the key is absent. Nested keys of the same name are ignored.
 */
function topLevelStringSpan(
  text: string,
  key: string,
  file: string,
): { start: number; end: number } | null {
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '"') {
      const end = jsonStringEnd(text, i);
      let k = end + 1;
      while (k < text.length && /\s/.test(text[k])) k++;
      const isKey = text[k] === ":";
      if (depth === 1 && isKey && JSON.parse(text.slice(i, end + 1)) === key) {
        k++;
        while (k < text.length && /\s/.test(text[k])) k++;
        if (text[k] !== '"') throw new Error(`${file}: top-level "${key}" is not a string`);
        return { start: k + 1, end: jsonStringEnd(text, k) };
      }
      i = end;
    } else if (c === "{" || c === "[") {
      depth++;
    } else if (c === "}" || c === "]") {
      depth--;
    }
  }
  return null;
}

function getJsonVersion(text: string, file: string): string {
  const parsed = JSON.parse(text) as unknown;
  const version =
    parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as { version?: unknown }).version
      : undefined;
  if (typeof version !== "string") throw new Error(`${file} has no top-level version`);
  return version;
}

function setJsonVersion(text: string, next: string, file: string): string {
  getJsonVersion(text, file);
  const span = topLevelStringSpan(text, "version", file);
  if (!span) throw new Error(`${file} has no top-level version`);
  const out = text.slice(0, span.start) + next + text.slice(span.end);
  if (getJsonVersion(out, file) !== next) throw new Error(`${file}: version rewrite failed`);
  return out;
}

/** The top-level `version` of package.json. */
export function getPackageJsonVersion(text: string): string {
  return getJsonVersion(text, "package.json");
}

/** Replace the top-level `version` of package.json, preserving all other text byte for byte. */
export function setPackageJsonVersion(text: string, next: string): string {
  return setJsonVersion(text, next, "package.json");
}

/** The top-level `version` of tauri.conf.json (the app version Tauri bundles). */
export function getTauriConfVersion(text: string): string {
  return getJsonVersion(text, "tauri.conf.json");
}

/** Replace the top-level `version` of tauri.conf.json, preserving all other text byte for byte. */
export function setTauriConfVersion(text: string, next: string): string {
  return setJsonVersion(text, next, "tauri.conf.json");
}

/** Split keeping each line's terminator (LF or CRLF) attached. */
function linesOf(text: string): string[] {
  return text.split(/(?<=\n)/);
}

/** The index of the `version = "..."` line inside the `[package]` table. */
function packageVersionLine(lines: string[]): number {
  let inPackage = false;
  let seenPackage = false;
  for (let i = 0; i < lines.length; i++) {
    const body = lines[i].replace(/\r?\n$/, "");
    const header = /^\s*\[([^\]]*)\]\s*(?:#.*)?$/.exec(body);
    if (header) {
      inPackage = header[1].trim() === "package";
      seenPackage ||= inPackage;
      continue;
    }
    if (inPackage && /^\s*version\s*=\s*"[^"]*"/.test(body)) return i;
  }
  throw new Error(seenPackage ? "[package] has no version" : "Cargo.toml has no [package] table");
}

/** Read `version = "..."` from the `[package]` table only. */
export function getCargoPackageVersion(text: string): string {
  const lines = linesOf(text);
  return /^\s*version\s*=\s*"([^"]*)"/.exec(lines[packageVersionLine(lines)])?.[1] ?? "";
}

/** Replace `version = "..."` inside the `[package]` table only. */
export function setCargoPackageVersion(text: string, next: string): string {
  const lines = linesOf(text);
  const i = packageVersionLine(lines);
  lines[i] = lines[i].replace(
    /^(\s*version\s*=\s*")[^"]*(")/,
    (_m, a: string, b: string) => `${a}${next}${b}`,
  );
  return lines.join("");
}

function lockEntryRe(crate: string): RegExp {
  const name = crate.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`(^name = "${name}"\\r?\\nversion = ")([^"]*)(")`, "gm");
}

/** The version of the lock entry named exactly `crate`; throws when absent or ambiguous. */
export function getCargoLockVersion(text: string, crate: string): string {
  const matches = [...text.matchAll(lockEntryRe(crate))];
  if (matches.length === 0) throw new Error(`Cargo.lock has no entry for ${crate}`);
  if (matches.length > 1) throw new Error(`Cargo.lock has ${matches.length} entries for ${crate}`);
  return matches[0][2];
}

/** Replace the version of the lock entry named exactly `crate` (LF or CRLF). */
export function setCargoLockVersion(text: string, crate: string, next: string): string {
  getCargoLockVersion(text, crate);
  return text.replace(lockEntryRe(crate), (_m, a: string, _v: string, b: string) => `${a}${next}${b}`);
}

/** The text of every bumped file, keyed by its repo-relative path (see BUMPED_FILES). */
export type VersionFileTexts = Record<string, string>;

/** The version each bumped file records, in BUMPED_FILES order. */
export function readVersions(files: VersionFileTexts): Record<string, string> {
  const text = (rel: string): string => {
    const t = files[rel];
    if (t === undefined) throw new Error(`missing ${rel}`);
    return t;
  };
  return {
    "package.json": getPackageJsonVersion(text("package.json")),
    "src-tauri/tauri.conf.json": getTauriConfVersion(text("src-tauri/tauri.conf.json")),
    "src-tauri/Cargo.toml": getCargoPackageVersion(text("src-tauri/Cargo.toml")),
    "Cargo.lock": getCargoLockVersion(text("Cargo.lock"), APP_CRATE),
  };
}

/** A description of the disagreement when the files record different versions, else null. */
export function versionDisagreement(versions: Record<string, string>): string | null {
  const distinct = new Set(Object.values(versions));
  if (distinct.size <= 1) return null;
  const list = Object.entries(versions)
    .map(([file, v]) => `${file} ${v}`)
    .join(", ");
  return `the version files disagree: ${list}`;
}

/** The new text of every bumped file with its version set to `next`. */
export function bumpFiles(files: VersionFileTexts, next: string): VersionFileTexts {
  return {
    "package.json": setPackageJsonVersion(files["package.json"], next),
    "src-tauri/tauri.conf.json": setTauriConfVersion(files["src-tauri/tauri.conf.json"], next),
    "src-tauri/Cargo.toml": setCargoPackageVersion(files["src-tauri/Cargo.toml"], next),
    "Cargo.lock": setCargoLockVersion(files["Cargo.lock"], APP_CRATE, next),
  };
}

// ── CI status ───────────────────────────────────────────────────────────────

export type CiStatus =
  | { kind: "passed" }
  | { kind: "skipped" }
  | { kind: "running"; status: string }
  | { kind: "failed"; conclusion: string }
  | { kind: "none" }
  | { kind: "unavailable"; reason: string };

/** The outcome of running `gh run list`; `spawnError` is set when gh could not be started. */
export interface GhResult {
  spawnError?: string;
  status: number;
  stdout: string;
  stderr: string;
}

/** The `gh run list` arguments that read the CI runs for `sha`. */
export function ciRunListArgs(sha: string, repo: string | null): string[] {
  const args = ["run", "list", "--workflow", CI_WORKFLOW, "--commit", sha];
  args.push("--json", "status,conclusion,headSha", "--limit", "20");
  if (repo) args.push("--repo", repo);
  return args;
}

/** `owner/name` from a GitHub remote URL (https or ssh), or null for anything else. */
export function githubRepoFromUrl(url: string): string | null {
  const m = /github\.com[:/]([^/\s]+)\/([^/\s]+?)(?:\.git)?\/?$/.exec(url.trim());
  return m ? `${m[1]}/${m[2]}` : null;
}

function firstLine(s: string): string {
  return s.trim().split(/\r?\n/)[0] ?? "";
}

/**
 * Interpret `gh run list --json status,conclusion,headSha` for `sha`. gh lists newest first, so
 * the first run whose `headSha` is `sha` decides: completed + success passes, anything not yet
 * completed is still running, any other conclusion failed, and no run at all is refused too.
 */
export function interpretCiRuns(gh: GhResult, sha: string): CiStatus {
  if (gh.spawnError !== undefined) {
    return { kind: "unavailable", reason: `gh could not be run (${gh.spawnError})` };
  }
  if (gh.status !== 0) {
    // The workflow does not exist (yet) where gh looked: no run can have passed.
    if (/could not find any workflows|workflow \S+ not found/i.test(gh.stderr)) return { kind: "none" };
    const why = firstLine(gh.stderr) || `exit ${gh.status}`;
    return { kind: "unavailable", reason: `gh run list failed: ${why}` };
  }
  let runs: unknown;
  try {
    runs = JSON.parse(gh.stdout || "[]");
  } catch {
    return { kind: "unavailable", reason: "gh run list printed something other than JSON" };
  }
  if (!Array.isArray(runs)) {
    return { kind: "unavailable", reason: "gh run list printed something other than a list" };
  }
  const run = (runs as { status?: unknown; conclusion?: unknown; headSha?: unknown }[]).find(
    (r) => r !== null && typeof r === "object" && r.headSha === sha,
  );
  if (!run) return { kind: "none" };
  const status = typeof run.status === "string" ? run.status : "";
  const conclusion = typeof run.conclusion === "string" ? run.conclusion : "";
  if (status !== "completed") return { kind: "running", status: status || "unknown" };
  if (conclusion === "success") return { kind: "passed" };
  return { kind: "failed", conclusion: conclusion || "unknown" };
}

function shortSha(sha: string): string {
  return sha.slice(0, 12);
}

/** Why the CI status blocks a release, or null when it does not. */
export function ciProblem(ci: CiStatus, sha: string): string | null {
  const at = shortSha(sha);
  switch (ci.kind) {
    case "passed":
    case "skipped":
      return null;
    case "running":
      return `CI still running for ${at} (${ci.status}); wait for ${CI_WORKFLOW} to finish`;
    case "failed":
      return `CI did not pass for ${at} (${ci.conclusion})`;
    case "none":
      return `no CI run for ${at}; push it and wait for ${CI_WORKFLOW}, or pass --skip-ci-check`;
    case "unavailable":
      return `could not read the CI status for ${at}: ${ci.reason}. Install and authenticate gh (gh auth login), or pass --skip-ci-check`;
  }
}

export const CI_SKIP_WARNING = `warning: --skip-ci-check given; releasing without a passing ${CI_WORKFLOW} run for HEAD`;

// ── preconditions ───────────────────────────────────────────────────────────

export interface RepoState {
  branch: string;
  dirty: boolean;
  behindOrigin: number;
  tagExists: boolean;
  /** The commit being released. */
  headSha: string;
  ci: CiStatus;
  /** From versionDisagreement: null when all four files agree. */
  versionMismatch: string | null;
}

/** Reasons a release must not start; empty when the repository is ready. */
export function preconditionProblems(s: RepoState, tag: string): string[] {
  const problems: string[] = [];
  if (s.branch !== RELEASE_BRANCH) problems.push(`not on ${RELEASE_BRANCH} (on ${s.branch})`);
  if (s.dirty) problems.push("the working tree has uncommitted changes");
  if (s.behindOrigin > 0) {
    problems.push(`${s.branch} is ${s.behindOrigin} commit(s) behind origin/${RELEASE_BRANCH}`);
  }
  if (s.tagExists) problems.push(`tag ${tag} already exists`);
  if (s.versionMismatch) problems.push(s.versionMismatch);
  const ci = ciProblem(s.ci, s.headSha);
  if (ci) problems.push(ci);
  return problems;
}

// ── commit, tag, push ───────────────────────────────────────────────────────

export function commitArgs(version: string): string[] {
  return ["commit", "-m", commitMessage(version), "--", ...BUMPED_FILES];
}

/**
 * One atomic push of the branch and the tag: either both refs land or neither does. Full refspecs
 * keep a branch and a tag of the same name from being confused.
 */
export function pushArgs(tag: string): string[] {
  return ["push", "--atomic", "origin", `refs/heads/${RELEASE_BRANCH}`, `refs/tags/${tag}`];
}

/**
 * How far a release got locally before a step failed:
 * - `written`: the bumped files are on disk, but the version commit was not made.
 * - `committed`: the version commit exists, but the tag was not created.
 * - `tagged`: the commit and the tag exist; the push is the step that failed.
 */
export type ReleaseProgress = "written" | "committed" | "tagged";

/**
 * The git commands that undo a local release that got as far as `progress`, in order: delete the
 * tag, uncommit the bump keeping the index, and restore the bumped files (the tree was clean before
 * the bump, so they carry only the bump). Each stage undoes only what it did. Never
 * `reset --hard`, which could take unrelated work with it.
 */
export function undoSteps(tag: string, progress: ReleaseProgress): string[][] {
  const restore = ["checkout", "HEAD", "--", ...BUMPED_FILES];
  const uncommit = ["reset", "--soft", "HEAD~1"];
  switch (progress) {
    case "written":
      return [restore];
    case "committed":
      return [uncommit, restore];
    case "tagged":
      return [["tag", "-d", tag], uncommit, restore];
  }
}

/** Git argument lists as copy-pasteable `git ...` command lines. */
export function gitCommandLines(steps: string[][]): string[] {
  return steps.map((args) => `git ${args.join(" ")}`);
}

/** What to print when a release step failed at `progress`, before undoing locally. */
export function releaseFailureLines(tag: string, progress: ReleaseProgress): string[] {
  switch (progress) {
    case "written":
      return [
        "The version commit failed (a pre-commit hook may have rejected it), so nothing was committed or pushed.",
        "Undoing locally: restoring the bumped files.",
      ];
    case "committed":
      return [
        `Creating tag ${tag} failed, so nothing was tagged or pushed.`,
        "Undoing locally: removing the version commit and restoring the bumped files.",
      ];
    case "tagged":
      return [
        `The atomic push failed and origin does not have tag ${tag} at the release commit, so nothing was pushed.`,
        `Undoing locally: deleting tag ${tag}, removing the version commit and restoring the bumped files.`,
      ];
  }
}

/**
 * Where a failed push actually left the remote. A non-zero exit from `git push` does not prove
 * nothing landed: the connection can drop after the server applied the refs.
 * - `landed`: origin has the tag at the release commit (the push is atomic, so main landed too).
 * - `rejected`: origin has no such tag, or has it at another commit; nothing of ours landed.
 * - `unknown`: the remote could not be checked, or the local commit could not be resolved.
 */
export type PushOutcome = "landed" | "rejected" | "unknown";

export interface RemoteTagCheck {
  /** Exit status of `git ls-remote origin refs/tags/<tag>`. */
  lsRemoteStatus: number;
  /** The commit origin's tag points at (see `remoteTagSha`), or null when origin has no such tag. */
  remoteTagSha: string | null;
  /** `git rev-parse <tag>^{commit}` locally; empty when that failed. */
  localCommitSha: string;
}

export function pushOutcome(c: RemoteTagCheck): PushOutcome {
  if (c.lsRemoteStatus !== 0) return "unknown";
  if (c.remoteTagSha === null) return "rejected";
  const local = c.localCommitSha.trim().toLowerCase();
  if (local.length === 0) return "unknown";
  return c.remoteTagSha.trim().toLowerCase() === local ? "landed" : "rejected";
}

/**
 * The commit `refs/tags/<tag>` points at in `git ls-remote` output, or null when it is absent.
 * A peeled `refs/tags/<tag>^{}` line (an annotated tag's commit) wins over the tag object's line.
 */
export function remoteTagSha(lsRemoteStdout: string, tag: string): string | null {
  const ref = `refs/tags/${tag}`;
  let direct: string | null = null;
  for (const line of lsRemoteStdout.split(/\r?\n/)) {
    const [sha, name] = line.trim().split(/\s+/);
    if (!sha || !name) continue;
    if (name === `${ref}^{}`) return sha;
    if (name === ref) direct = sha;
  }
  return direct;
}

/** The ls-remote check to run after a failed push. */
export function lsRemoteTagArgs(tag: string): string[] {
  return ["ls-remote", "origin", `refs/tags/${tag}`];
}

/** What to print when the push failed but origin has the tag at the release commit. */
export function pushLandedLines(tag: string): string[] {
  return [
    `warning: git push exited non-zero, but origin has ${tag} at the release commit: the push reached GitHub.`,
    "Nothing was undone. release.yml is running for the tag.",
    `Confirm with: git fetch origin && git ls-remote origin refs/tags/${tag}`,
  ];
}

/** What to print when the push failed and origin could not be checked: undo by hand if absent. */
export function pushUnknownLines(tag: string): string[] {
  return [
    `The push failed and origin could not be checked, so it is unknown whether ${tag} reached GitHub.`,
    "Nothing was undone. Check with:",
    `  git ls-remote origin refs/tags/${tag}`,
    `If it prints the commit that \`git rev-parse ${tag}^{commit}\` prints, the release went out: nothing more to do.`,
    "If it prints nothing, undo with:",
    ...gitCommandLines(undoSteps(tag, "tagged")).map((l) => `  ${l}`),
  ];
}

// ── output ──────────────────────────────────────────────────────────────────

/** The line that opens a release, saying under `--dry-run` that nothing will change. */
export function startLine(current: string, next: string, dryRun: boolean): string {
  return dryRun
    ? `==> Would release ${current} -> ${next} (dry run: nothing is changed)`
    : `==> Releasing ${current} -> ${next}`;
}

/** The lines that end a release; under `--dry-run` they say what a real run would do. */
export function doneLines(tag: string, dryRun: boolean): string[] {
  if (dryRun) {
    return [
      `Would tag ${tag} and push it with ${RELEASE_BRANCH}; release.yml would then build and publish the installers.`,
      "Nothing was changed. Run again without --dry-run to release.",
    ];
  }
  return [
    `Tagged ${tag}. release.yml builds and publishes the installers.`,
    "Watch it with: gh run list --workflow release.yml --limit 1",
  ];
}

// ── updater artifacts ───────────────────────────────────────────────────────

/** The `plugins.updater.pubkey` value that stands in until a real key is generated. */
export const UPDATER_PUBKEY_PLACEHOLDER = "REPLACE_WITH_UPDATER_PUBKEY";

/** Error codes that open the message of every updater-signing failure. */
export type UpdaterErrorCode = "UPDATER_KEY_MISSING" | "UPDATER_PUBKEY_PLACEHOLDER";

/** An updater-signing failure; the message starts with the code so CI logs show it. */
export class UpdaterSigningError extends Error {
  readonly code: UpdaterErrorCode;
  constructor(code: UpdaterErrorCode, detail: string) {
    super(`${code}: ${detail}`);
    this.name = "UpdaterSigningError";
    this.code = code;
  }
}

export type BundleTarget = "windows" | "macos" | "linux";

export const BUNDLE_TARGETS: readonly BundleTarget[] = ["windows", "macos", "linux"];

export interface BundleOverlayInput {
  target: BundleTarget;
  /** Whether the signing key secret is set: the bare "true"/"false" a workflow step computes. */
  updaterKey: string | undefined;
  /** `GITHUB_REF`; undefined outside Actions. */
  ref: string | undefined;
  /** `plugins.updater.pubkey` from tauri.conf.json. */
  pubkey: string;
}

/** Whether `ref` is a release tag (`refs/tags/v*`), the only ref that publishes. */
export function isReleaseTagRef(ref: string | undefined): boolean {
  return (ref ?? "").startsWith("refs/tags/v");
}

function isPlaceholderPubkey(pubkey: string): boolean {
  return pubkey.trim() === UPDATER_PUBKEY_PLACEHOLDER;
}

function placeholderPubkeyError(): UpdaterSigningError {
  return new UpdaterSigningError(
    "UPDATER_PUBKEY_PLACEHOLDER",
    "plugins.updater.pubkey in src-tauri/tauri.conf.json is still the placeholder; a release cannot ship updates nobody can verify",
  );
}

/**
 * The tauri `--config` overlay (as JSON text) that turns on signed updater artifacts. With the key
 * it sets `bundle.createUpdaterArtifacts`; on macOS it also adds the `app` target, because the
 * bundler only makes `Keitora.app.tar.gz` when `app` is a target (arrays replace on merge, so this
 * overrides the base targets on the macOS runner only). Linux needs no extra target: the AppImage
 * that tauri.linux.conf.json already builds is its own updater artifact. Without the key it is `{}`. A release tag
 * must ship updater artifacts, so a tag without the key, or with the placeholder pubkey, throws.
 */
export function bundleOverlay(input: BundleOverlayInput): string {
  const hasKey = input.updaterKey === "true";
  if (isReleaseTagRef(input.ref)) {
    if (isPlaceholderPubkey(input.pubkey)) throw placeholderPubkeyError();
    if (!hasKey) {
      throw new UpdaterSigningError(
        "UPDATER_KEY_MISSING",
        'secret TAURI_SIGNING_PRIVATE_KEY is not set in the release environment. See CONTRIBUTING.md, "Releases"',
      );
    }
  }
  if (!hasKey) return "{}";
  const bundle: Record<string, unknown> = { createUpdaterArtifacts: true };
  if (input.target === "macos") bundle.targets = ["app", "dmg"];
  return JSON.stringify({ bundle });
}

/** `plugins.updater.pubkey` from the text of tauri.conf.json. */
export function readUpdaterPubkey(tauriConfText: string): string {
  const conf = JSON.parse(tauriConfText) as { plugins?: { updater?: { pubkey?: unknown } } };
  const pubkey = conf?.plugins?.updater?.pubkey;
  if (typeof pubkey !== "string" || pubkey.trim() === "") {
    throw new Error("tauri.conf.json has no plugins.updater.pubkey");
  }
  return pubkey.trim();
}

const BASE64_RE = /^[A-Za-z0-9+/]+={0,2}$/;

function strictBase64(s: string, what: string): Buffer {
  const t = s.trim();
  if (t.length === 0 || t.length % 4 !== 0 || !BASE64_RE.test(t)) {
    throw new Error(`${what} is not valid base64`);
  }
  return Buffer.from(t, "base64");
}

/**
 * The 8-byte key id of a minisign public key or signature as Tauri stores it: base64 of the whole
 * minisign file text. Decoded, the first line that is not an `untrusted comment:` is itself base64
 * of a 2-byte algorithm tag, the key id, then the key (32 bytes) or the signature (64 bytes).
 */
export function minisignKeyId(b64: string, kind: "pubkey" | "signature"): Buffer {
  const text = strictBase64(b64, `the ${kind}`).toString("utf8");
  const line = text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => l !== "" && !l.startsWith("untrusted comment:"));
  if (!line) throw new Error(`the ${kind} has no minisign data line`);
  const bytes = strictBase64(line, `the ${kind}'s minisign data line`);
  const expected = kind === "pubkey" ? 2 + 8 + 32 : 2 + 8 + 64;
  if (bytes.length !== expected) {
    throw new Error(`the ${kind} decodes to ${bytes.length} bytes, expected ${expected}`);
  }
  const alg = bytes.subarray(0, 2).toString("latin1");
  const algOk = kind === "pubkey" ? alg === "Ed" : alg === "Ed" || alg === "ED";
  if (!algOk) throw new Error(`the ${kind} has an unknown minisign algorithm "${alg}"`);
  return bytes.subarray(2, 10);
}

/** Whether the signature was made by the key `pubkeyB64` names (both as Tauri stores them). */
export function updaterKeyIdMatches(pubkeyB64: string, sigB64: string): boolean {
  return minisignKeyId(pubkeyB64, "pubkey").equals(minisignKeyId(sigB64, "signature"));
}

/** A key id as minisign prints it: upper-case hex, most significant byte first. */
export function formatKeyId(id: Buffer): string {
  return Buffer.from(id).reverse().toString("hex").toUpperCase();
}

/**
 * Why an updater signature would fail verification in installed copies, or null when its key id
 * matches the configured pubkey. Tauri only warns on a mismatched key at build time, so CI turns
 * this into a failed job.
 */
export function updaterSigProblem(pubkeyB64: string, sigB64: string): string | null {
  if (isPlaceholderPubkey(pubkeyB64)) return placeholderPubkeyError().message;
  if (updaterKeyIdMatches(pubkeyB64, sigB64)) return null;
  const want = minisignKeyId(pubkeyB64, "pubkey");
  const got = minisignKeyId(sigB64, "signature");
  return `the signature was made by key ${formatKeyId(got)}, but tauri.conf.json trusts key ${formatKeyId(want)}: installed copies would reject this update. TAURI_SIGNING_PRIVATE_KEY does not belong to the committed pubkey`;
}
