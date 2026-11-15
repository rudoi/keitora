#!/usr/bin/env tsx
// Writes the tauri `--config` overlay that turns on signed updater artifacts (see bundleOverlay in
// ./release.lib.ts) to a file. A file rather than inline JSON sidesteps yarn and Windows shell
// quoting; the build passes it as a second `--config`, which merges after the bundle overlay.
//
//   UPDATER_KEY=true|false GITHUB_REF=refs/tags/vX.Y.Z \
//     yarn --silent tsx scripts/bundle-overlay.ts --target windows|macos --out <path>
//
// UPDATER_KEY is a bare boolean saying whether the signing key secret is set; the key itself never
// reaches this script. The pubkey comes from src-tauri/tauri.conf.json.

import { writeFileSync } from "node:fs";
import { committedUpdaterPubkey, failCli as fail } from "./cli.ts";
import { type BundleTarget, bundleOverlay } from "./release.lib.ts";

const USAGE = "usage: bundle-overlay.ts --target windows|macos --out <path>";

function parse(argv: string[]): { target: BundleTarget; out: string } {
  let target: BundleTarget | undefined;
  let out: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const value = argv[i + 1];
    switch (argv[i]) {
      case "--target":
        if (value !== "windows" && value !== "macos") fail(`--target must be windows or macos\n${USAGE}`, 2);
        target = value;
        i++;
        break;
      case "--out":
        if (!value || value.startsWith("-")) fail(`--out requires a path\n${USAGE}`, 2);
        out = value;
        i++;
        break;
      default:
        fail(`unknown argument "${argv[i]}"\n${USAGE}`, 2);
    }
  }
  if (!target || !out) fail(USAGE, 2);
  return { target, out };
}

const { target, out } = parse(process.argv.slice(2));
try {
  const pubkey = committedUpdaterPubkey();
  const overlay = bundleOverlay({
    target,
    updaterKey: process.env.UPDATER_KEY,
    ref: process.env.GITHUB_REF,
    pubkey,
  });
  writeFileSync(out, `${overlay}\n`);
  console.log(`bundle overlay (${target}): ${overlay}`);
} catch (e) {
  fail(e instanceof Error ? e.message : String(e), 1);
}
