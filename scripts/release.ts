#!/usr/bin/env tsx
// Cuts a release: bumps the app version, commits, tags and pushes. It never builds or publishes;
// the tag push runs .github/workflows/release.yml, which builds the Windows and macOS installers
// and creates the GitHub release.
//
// Pure logic (tested) is in ./release.lib.ts; this file is the IO and process orchestration.
//
//   yarn release --patch | --minor | --major
//   yarn release --set 0.4.0
//   yarn release --dry-run          # previews --patch from any branch; changes nothing
//
// Flow: check preconditions (on main, clean, not behind origin/main, tag absent, the four version
// files agree, the latest ci.yml run for HEAD passed) → bump the four files → commit
// "chore(release): vX.Y.Z" → tag vX.Y.Z → `git push --atomic origin refs/heads/main refs/tags/vX.Y.Z`.
//
// main is pushed directly: the main ruleset has an admin bypass, so the push prints
// "remote: Bypassed rule violations". Non-admins cannot release this way and need a PR instead.
// The push is atomic, so either both refs land or neither does. If any step after the files are
// written fails, the release is undone locally as far as it got (undoSteps) so a retry starts
// clean. A failed push is first checked against origin: a dropped connection can fail the push
// after the refs landed, and then nothing is undone (pushOutcome).

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  BUMPED_FILES,
  bumpFiles,
  CI_SKIP_WARNING,
  type CiStatus,
  ciRunListArgs,
  commitArgs,
  doneLines,
  type GhResult,
  githubRepoFromUrl,
  helpText,
  interpretCiRuns,
  nextVersion,
  parseArgs,
  preconditionProblems,
  gitCommandLines,
  lsRemoteTagArgs,
  type PushOutcome,
  pushArgs,
  pushLandedLines,
  pushOutcome,
  pushUnknownLines,
  type ReleaseOptions,
  type ReleaseProgress,
  releaseFailureLines,
  remoteTagSha,
  type RepoState,
  readVersions,
  startLine,
  tagFor,
  undoSteps,
  type VersionFileTexts,
  versionDisagreement,
} from "./release.lib.ts";

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const path = (rel: string): string => join(REPO_ROOT, rel);

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
  const r = spawnSync(cmd, args, {
    stdio: "inherit",
    cwd: REPO_ROOT,
  });
  if (r.status !== 0) {
    throw new Error(`Command failed (exit ${r.status ?? "?"}): ${cmd} ${args.join(" ")}`);
  }
}

/** Run a read-only command and capture its output. */
function capture(cmd: string, args: string[]): GhResult {
  const r = spawnSync(cmd, args, {
    encoding: "utf8",
    cwd: REPO_ROOT,
  });
  return {
    spawnError: r.error ? r.error.message : undefined,
    status: r.status ?? 1,
    stdout: r.stdout ?? "",
    stderr: r.stderr ?? "",
  };
}

function readVersionFiles(): VersionFileTexts {
  const files: VersionFileTexts = {};
  for (const rel of BUMPED_FILES) files[rel] = readFileSync(path(rel), "utf8");
  return files;
}

function readCiStatus(sha: string, skip: boolean): CiStatus {
  if (skip) return { kind: "skipped" };
  const remote = capture("git", ["remote", "get-url", "origin"]);
  const repo = remote.status === 0 ? githubRepoFromUrl(remote.stdout) : null;
  return interpretCiRuns(capture("gh", ciRunListArgs(sha, repo)), sha);
}

function gatherRepoState(tag: string, versionMismatch: string | null, skipCi: boolean): RepoState {
  const branch = capture("git", ["rev-parse", "--abbrev-ref", "HEAD"]).stdout.trim();
  const headSha = capture("git", ["rev-parse", "HEAD"]).stdout.trim();
  const dirty = capture("git", ["status", "--porcelain"]).stdout.trim().length > 0;
  // Read-only apart from updating the remote-tracking ref.
  capture("git", ["fetch", "origin", "main"]);
  const behind = capture("git", ["rev-list", "--count", "HEAD..origin/main"]);
  const behindOrigin = behind.status === 0 ? Number(behind.stdout.trim()) || 0 : 0;
  const localTag = capture("git", ["rev-parse", "-q", "--verify", `refs/tags/${tag}`]).status === 0;
  const remoteTag =
    capture("git", ["ls-remote", "--tags", "origin", `refs/tags/${tag}`]).stdout.trim().length > 0;
  return {
    branch,
    dirty,
    behindOrigin,
    tagExists: localTag || remoteTag,
    headSha,
    ci: readCiStatus(headSha, skipCi),
    versionMismatch,
  };
}

/** Throws before anything is written if the bump would leave any file unchanged. */
function checkBump(before: VersionFileTexts, after: VersionFileTexts): void {
  for (const rel of BUMPED_FILES) {
    if (after[rel] === before[rel]) throw new Error(`the bump did not change ${rel}`);
  }
}

function writeFiles(after: VersionFileTexts, dryRun: boolean): void {
  for (const rel of BUMPED_FILES) {
    if (dryRun) {
      console.log(`[dry-run] write ${rel}`);
      continue;
    }
    writeFileSync(path(rel), after[rel]);
    console.log(`+ wrote ${rel}`);
  }
}

function main(): void {
  let opts: ReleaseOptions;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (e) {
    console.error(`error: ${(e as Error).message}\n`);
    console.log(helpText());
    process.exit(2);
    return;
  }
  if (opts.help) {
    console.log(helpText());
    return;
  }

  const files = readVersionFiles();
  const versions = readVersions(files);
  const current = versions["package.json"];
  const next = nextVersion(current, opts);
  const tag = tagFor(next);

  if (opts.skipCiCheck) console.warn(CI_SKIP_WARNING);
  const state = gatherRepoState(tag, versionDisagreement(versions), opts.skipCiCheck);
  const problems = preconditionProblems(state, tag);
  if (problems.length > 0) {
    if (!opts.dryRun) throw new Error(`Refusing to release:\n  - ${problems.join("\n  - ")}`);
    for (const p of problems) console.log(`[dry-run] would refuse: ${p}`);
  }

  console.log(startLine(current, next, opts.dryRun));
  const bumped = bumpFiles(files, next);
  checkBump(files, bumped);

  // How far the release got, so a failure undoes exactly that much. Dry runs never fail a step.
  let progress: ReleaseProgress | null = null;
  try {
    progress = "written";
    writeFiles(bumped, opts.dryRun);
    runMutating("git", commitArgs(next), opts.dryRun);
    progress = "committed";
    runMutating("git", ["tag", tag], opts.dryRun);
    progress = "tagged";
    runMutating("git", pushArgs(tag), opts.dryRun);
  } catch (e) {
    if (opts.dryRun || progress === null) throw e;
    console.error("");
    if (progress === "tagged") {
      const outcome = checkFailedPush(tag);
      if (outcome === "landed") {
        for (const line of pushLandedLines(tag)) console.warn(line);
        console.log("");
        for (const line of doneLines(tag, false)) console.log(line);
        return;
      }
      if (outcome === "unknown") {
        for (const line of pushUnknownLines(tag)) console.error(line);
        throw e;
      }
    }
    for (const line of releaseFailureLines(tag, progress)) console.error(line);
    undoLocally(tag, progress);
    throw e;
  }

  console.log("");
  for (const line of doneLines(tag, opts.dryRun)) console.log(line);
}

/** After a failed push, ask origin whether the tag landed anyway. */
function checkFailedPush(tag: string): PushOutcome {
  const ls = capture("git", lsRemoteTagArgs(tag));
  const local = capture("git", ["rev-parse", "--verify", "-q", `${tag}^{commit}`]);
  return pushOutcome({
    lsRemoteStatus: ls.spawnError ? 1 : ls.status,
    remoteTagSha: remoteTagSha(ls.stdout, tag),
    localCommitSha: local.status === 0 ? local.stdout.trim() : "",
  });
}

/** Run the undo steps; if one fails, print the ones left to run by hand. */
function undoLocally(tag: string, progress: ReleaseProgress): void {
  const steps = undoSteps(tag, progress);
  for (let i = 0; i < steps.length; i++) {
    try {
      runMutating("git", steps[i], false);
    } catch (e) {
      console.error("Undo stopped. Finish it by hand:");
      for (const line of gitCommandLines(steps.slice(i))) console.error(`  ${line}`);
      throw e;
    }
  }
}

try {
  main();
} catch (e) {
  console.error(`\nrelease failed: ${(e as Error).message}`);
  process.exit(1);
}
