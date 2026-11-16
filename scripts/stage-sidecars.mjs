#!/usr/bin/env node
// Cross-platform sidecar staging for the Tauri release bundle.
//
// Builds the `keitora-cli` release binary and stages it as a Tauri
// sidecar (target-triple–suffixed) under `src-tauri/binaries/`, so the packaged app
// ships it next to the app binary. Replaces the POSIX `bundle-cli.sh` so
// `yarn tauri:bundle` works on Windows/Linux/macOS without Git Bash — the missing
// staging on Windows is why the first Windows installer shipped no sidecar.
//
// Invoked by the `bundle-cli` yarn script (itself run by `tauri:bundle`). Deliberately
// NOT part of `tauri dev` / plain `cargo build`: the externalBin overlay
// (`src-tauri/tauri.bundle.conf.json`) is applied only for the release bundle, so
// everyday builds don't require the staged sidecar. Any failure exits non-zero so the
// `tauri:bundle` `&&` chain aborts — a release bundle can never silently proceed
// without sidecars.
//
// On macOS it also warns (without failing) when no stable signing identity is set: an
// ad-hoc bundle loses its privacy grants, such as Screen Recording, on every rebuild.
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { unsignedBundleWarning } from "./signing.mjs";

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const isWindows = process.platform === "win32";
const EXE = isWindows ? ".exe" : "";
// Node's execFile does no PATHEXT resolution on Windows, so name the real executables.
const CARGO = isWindows ? "cargo.exe" : "cargo";
const RUSTC = isWindows ? "rustc.exe" : "rustc";

const signingWarning = unsignedBundleWarning(process.platform, process.env);
if (signingWarning) console.warn(signingWarning);

// Host target triple — Tauri's externalBin requires the sidecar filename to carry it
// (it strips the suffix at bundle time, placing plain `keitora-cli`
// next to the app binary).
const rustcVerbose = execFileSync(RUSTC, ["-vV"], { encoding: "utf8" });
const triple = rustcVerbose.match(/^host:\s*(.+)$/m)?.[1]?.trim();
if (!triple) {
  console.error("stage-sidecars: could not determine host target triple from `rustc -vV`");
  process.exit(1);
}

const binariesDir = join(repoRoot, "src-tauri", "binaries");
mkdirSync(binariesDir, { recursive: true });

// The CLI crate builds `keitora` (NOT `keitora-cli`); the sidecar basename is
// `keitora-cli` because Tauri forbids a sidecar named the same as the `keitora` app
// package. The first-run installer still links it onto PATH as `keitora`
// (see `cli_install::sidecar_candidate`).
// Only the CLI is staged; herdr is the PTY backend and is not bundled.
const sidecars = [{ crate: "keitora-cli", builtName: "keitora", sidecar: "keitora-cli" }];

for (const { crate, builtName, sidecar } of sidecars) {
  execFileSync(CARGO, ["build", "-p", crate, "--release"], { cwd: repoRoot, stdio: "inherit" });
  const from = join(repoRoot, "target", "release", `${builtName}${EXE}`);
  const to = join(binariesDir, `${sidecar}-${triple}${EXE}`);
  copyFileSync(from, to);
  console.log(`stage-sidecars: staged ${to}`);
}
