// Shared IO helpers for the small CI scripts (bundle-overlay.ts, check-updater-sig.ts). The pure
// logic they call lives in ./release.lib.ts.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { readUpdaterPubkey } from "./release.lib.ts";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Print an error (as a GitHub Actions annotation when on Actions) and exit with `code`. */
export function failCli(message: string, code: number): never {
  const prefix = process.env.GITHUB_ACTIONS === "true" ? "::error::" : "error: ";
  console.error(`${prefix}${message}`);
  process.exit(code);
}

/** `plugins.updater.pubkey` from the committed src-tauri/tauri.conf.json. */
export function committedUpdaterPubkey(): string {
  return readUpdaterPubkey(readFileSync(join(ROOT, "src-tauri", "tauri.conf.json"), "utf8"));
}
