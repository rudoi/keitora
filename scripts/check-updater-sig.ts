#!/usr/bin/env tsx
// Fails when an updater signature was not made by the key whose pubkey is committed in
// src-tauri/tauri.conf.json. Tauri only warns about that mismatch at build time, and a release
// signed by the wrong key would be rejected by every installed copy.
//
//   yarn --silent tsx scripts/check-updater-sig.ts <path to .sig>

import { readFileSync } from "node:fs";
import { committedUpdaterPubkey, failCli as fail } from "./cli.ts";
import { formatKeyId, minisignKeyId, updaterSigProblem } from "./release.lib.ts";

const args = process.argv.slice(2);
if (args.length !== 1 || args[0].startsWith("-")) fail("usage: check-updater-sig.ts <path to .sig>", 2);
const sigPath = args[0];

try {
  const pubkey = committedUpdaterPubkey();
  const sig = readFileSync(sigPath, "utf8");
  const problem = updaterSigProblem(pubkey, sig);
  if (problem) fail(`${sigPath}: ${problem}`, 1);
  console.log(`${sigPath}: signed by key ${formatKeyId(minisignKeyId(sig, "signature"))}, matching tauri.conf.json`);
} catch (e) {
  fail(`${sigPath}: ${e instanceof Error ? e.message : String(e)}`, 1);
}
