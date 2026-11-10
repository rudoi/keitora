// Launch a dev Keitora on the HERDR backend, fully isolated from your real work.
//
// Why a dedicated launcher (not plain `tauri dev`): the herdr backend spawns a
// real agent into every worktree of the ACTIVE project on boot. `tauri dev` runs
// the app with cwd inside THIS repo, so Keitora auto-registers the real project and
// its worktrees and starts `claude --resume` in each — hijacking your live sessions.
// Three independent leaks bit us during development (registry auto-discovery from
// cwd, the shared `~`-anchored task DB, and herdr persisting+restoring its layout),
// so this launcher closes all three:
//   1. cwd = a THROWAWAY git repo (not this repo)  -> no real project auto-registers
//   2. a FRESH KEITORA_CONFIG_HOME pre-seeded with ONLY the throwaway project
//   3. the herdr state dir is PURGED each run       -> no accumulated layout restores
// It therefore bypasses `tauri dev` (which cannot set the app's cwd) and runs the
// built `keitora-app` directly against a `yarn dev` Vite server.
//
// This is for SAFELY testing the herdr backend. Dogfooding herdr against your real
// projects (which by design spawns into real worktrees) is a deliberate, separate
// act and intentionally NOT what this script does.
//
// No shebang: this module is imported by dev-herdr.test.ts, and Vitest's transform
// relocates a shebang off byte 0, where it is no longer valid syntax.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join, posix, resolve, win32 } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const isWindows = process.platform === "win32";
const EXE = isWindows ? ".exe" : "";

/**
 * Where THIS checkout's dev instance keeps its state.
 *
 * Keyed by repo root rather than machine-global, because Keitora is developed across
 * several git worktrees: a shared dev home would hand them the same herdr server and
 * state, so the second worktree's app would quietly attach to the first one's sessions.
 * The hash keeps two worktrees with the same basename apart.
 *
 * Always absolute: the app may run with a different cwd than this script, so a relative
 * override would be created here and then looked up somewhere else entirely.
 */
export function devHomeFor(env = process.env, root = repoRoot) {
  const override = env.KEITORA_DEV_HOME;
  if (override) return resolve(override);
  const slug = `${basename(root)}-${createHash("sha1").update(root).digest("hex").slice(0, 8)}`;
  return join(homedir(), ".keitora-dev", slug);
}

/**
 * Leading `major.minor.patch` of a release dir name such as `0.9.10-aarch64-apple-darwin`
 * (an optional `v` is allowed), or null when it does not start with one. Mirrors the
 * Rust side's `parse_release_version`.
 */
export function parseReleaseVersion(name) {
  const m = /^v?(\d+)\.(\d+)\.(\d+)/.exec(name);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

/** Newest first by parsed version; unparsable names last, ties by name descending. */
export function sortReleasesNewestFirst(names) {
  const cmp = (a, b) => {
    const va = parseReleaseVersion(a);
    const vb = parseReleaseVersion(b);
    if (va && vb) {
      for (let i = 0; i < 3; i++) if (va[i] !== vb[i]) return vb[i] - va[i];
    } else if (va || vb) {
      return va ? -1 : 1;
    }
    return a < b ? 1 : a > b ? -1 : 0;
  };
  return [...names].sort(cmp);
}

/** A regular file; on unix also with an execute bit. */
function isExecutableFile(p, platform = process.platform) {
  try {
    const st = statSync(p);
    return st.isFile() && (platform === "win32" || (st.mode & 0o111) !== 0);
  } catch {
    return false;
  }
}

/**
 * Resolve the herdr executable, first hit wins. The same order as the app's own search
 * (`discover_exe` in backend/herdr/env.rs), minus the config file and the login shell:
 *   1. `KEITORA_HERDR_BIN`, taken as given;
 *   2. `herdr` on `PATH`;
 *   3. on unix, `/opt/homebrew/bin`, `/usr/local/bin` and `~/.local/bin`, which a GUI
 *      launch's minimal `PATH` misses;
 *   4. the newest standalone release under
 *      `~/.herdr/packages/standalone/releases/<ver>-<triple>/herdr[.exe]` (herdr
 *      self-updates into versioned dirs; newest by parsed version, not by string).
 * Candidates 2-4 count only if they are executable files. Returns null if none is found.
 * Pure modulo the filesystem: `opts` injects the platform, the executable test and the
 * directory listing.
 */
export function discoverHerdrBin(env = process.env, home = homedir(), opts = {}) {
  const platform = opts.platform ?? process.platform;
  const isExecutable = opts.isExecutable ?? ((p) => isExecutableFile(p, platform));
  const listDir =
    opts.listDir ??
    ((d) => {
      try {
        return readdirSync(d);
      } catch {
        return [];
      }
    });
  if (env.KEITORA_HERDR_BIN) return env.KEITORA_HERDR_BIN;
  const win = platform === "win32";
  // Join with the target platform's separator, not the host's, so `opts.platform` holds.
  const join = win ? win32.join : posix.join;
  const exeName = win ? "herdr.exe" : "herdr";
  const sep = win ? ";" : ":";
  for (const dir of (env.PATH ?? env.Path ?? "").split(sep).filter(Boolean)) {
    const p = join(dir, exeName);
    if (isExecutable(p)) return p;
  }
  if (!win) {
    for (const dir of ["/opt/homebrew/bin", "/usr/local/bin", join(home, ".local", "bin")]) {
      const p = join(dir, exeName);
      if (isExecutable(p)) return p;
    }
  }
  const releases = join(home, ".herdr", "packages", "standalone", "releases");
  for (const name of sortReleasesNewestFirst(listDir(releases))) {
    const p = join(releases, name, exeName);
    if (isExecutable(p)) return p;
  }
  return null;
}

/**
 * The isolated paths for a herdr dev run, all under a dedicated herdr dev home
 * (the `-herdr` suffix on the checkout's dev home keeps purging state here from
 * touching anything else). Pure: builds paths, creates nothing.
 */
export function herdrDevPaths(devHome) {
  return {
    devHome,
    // KEITORA_HOME for the run: the herdr server's state lives at <here>/herdr.
    keitoraHome: devHome,
    herdrStateDir: join(devHome, "herdr"),
    // A fresh registry home, pre-seeded with only the throwaway project.
    configHome: join(devHome, "config"),
    // The throwaway git repo the app registers + spawns into. Its directory name is
    // the worktree name: session hooks report state under the git toplevel's
    // basename, so the two must match or the session never leaves `offline`.
    repoPath: join(devHome, "herdr-test-repo"),
    // Task DBs for the run. Without an explicit `db_path` a project's DB defaults to
    // the REAL `~/.keitora/<name>/shared/tasks.db`; this keeps it in the dev home.
    tasksDir: join(devHome, "tasks"),
  };
}

/**
 * The `keitora.toml` written into a throwaway project's root each run. It pins the
 * task DB to `dbPath` (inside the dev home, not the real `~/.keitora`) and points the
 * session hooks at this checkout's freshly built CLI (`cliPath`), so the app installs
 * them instead of finding no `keitora` binary. Worktrees come from the seeded
 * registry. Pure (returns the TOML text); both paths are forward-slashed TOML
 * literal strings.
 */
export function seedProjectToml(name, dbPath, cliPath) {
  const fwd = (p) => p.replace(/\\/g, "/");
  return [
    "[project]",
    `name = "${name}"`,
    "",
    "[tasks]",
    `db_path = '${fwd(dbPath)}'`,
    "",
    "[cli]",
    `path = '${fwd(cliPath)}'`,
    "",
  ].join("\n");
}

/**
 * The Claude Code config the dev agents run under, or null to leave them on your
 * real one.
 *
 * Claude Code remembers "I trust this folder" only in its config file
 * (`.claude.json`, `projects[<repo root>].hasTrustDialogAccepted`); there is no
 * flag or env var to skip the prompt. Pre-trusting the throwaway repos therefore
 * needs a config of our own: `CLAUDE_CONFIG_DIR` under the dev home, which this run
 * may write freely. Your real `~/.claude.json` is never touched.
 *
 * A separate config dir also has its own login, so this is only used when the
 * environment already carries a credential Claude Code reads without one
 * (`CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`, or `ANTHROPIC_API_KEY`).
 * Without one, null: the agents use your real config, and the first run in a new
 * dev repo asks to trust it once. Pure.
 */
export function claudeDevConfig(env, devHome) {
  const hasCredential = Boolean(env.CLAUDE_CODE_OAUTH_TOKEN || env.ANTHROPIC_API_KEY);
  if (!hasCredential) return null;
  const configDir = join(devHome, "claude");
  return {
    configDir,
    configFile: join(configDir, ".claude.json"),
    env: { CLAUDE_CONFIG_DIR: configDir },
  };
}

/**
 * `existing` (a parsed `.claude.json`, or null) with every path in `repoPaths`
 * marked trusted, and the first-run onboarding marked done so a fresh config dir
 * doesn't stop at the theme picker either. Each path is keyed both native and
 * forward-slashed, since the Windows key form is undocumented. Everything else in
 * `existing` is kept. Pure (returns a new object).
 */
export function trustedClaudeConfig(existing, repoPaths) {
  const cfg = { ...(existing ?? {}) };
  cfg.hasCompletedOnboarding = true;
  const projects = { ...(cfg.projects ?? {}) };
  for (const repo of repoPaths) {
    for (const key of new Set([repo, repo.replace(/\\/g, "/")])) {
      projects[key] = { ...(projects[key] ?? {}), hasTrustDialogAccepted: true };
    }
  }
  cfg.projects = projects;
  return cfg;
}

/**
 * The seed `keitora.toml` registering ONLY the throwaway project, so the app opens
 * on it and never discovers a real project. Pure (returns the TOML text).
 * `repoPath` is embedded as a literal single-quoted TOML string (backslashes safe).
 */
export function seedRegistryToml(repoPath) {
  const fwd = repoPath.replace(/\\/g, "/");
  return [
    'active = "herdr-test-repo"',
    "",
    "[projects.herdr-test-repo]",
    `path = '${repoPath}'`,
    "",
    "[[projects.herdr-test-repo.worktrees]]",
    'name = "herdr-test-repo"',
    `path = "${fwd}"`,
    "",
  ].join("\n");
}

/** Throwaway project names for a multi-project dev run, in registry order. */
const DEMO_PROJECT_NAMES = [
  "atlas",
  "beacon",
  "cinder",
  "drift",
  "ember",
  "fathom",
  "gale",
  "harbor",
  "ion",
  "juniper",
];
const DEMO_WORKTREE_NAMES = ["main", "alpha", "bravo", "charlie"];

/**
 * How many throwaway projects to seed, from `KEITORA_DEV_PROJECTS` (default 1: the
 * single-project run). Clamped to the available demo names. Pure.
 */
export function demoProjectCount(env = process.env) {
  const n = Number.parseInt(env.KEITORA_DEV_PROJECTS ?? "", 10);
  if (!Number.isFinite(n) || n < 1) return 1;
  return Math.min(n, DEMO_PROJECT_NAMES.length);
}

/**
 * The throwaway projects for a multi-project run, all under `<devHome>/demo`. Each is a
 * git repo whose root is the `main` worktree, plus 0-3 extra git worktrees (varied so
 * the rail shows groups of different sizes). Directory basenames equal the registry
 * keys, so the resolved project name matches the key. Pure.
 */
export function demoProjects(devHome, count) {
  return DEMO_PROJECT_NAMES.slice(0, count).map((name, i) => {
    const root = join(devHome, "demo", name);
    const extra = DEMO_WORKTREE_NAMES.slice(1, 1 + ((i * 2 + 1) % 4));
    return {
      name,
      root,
      worktrees: [
        { name: "main", path: root },
        ...extra.map((wt) => ({ name: wt, path: join(devHome, "demo", `${name}-${wt}`) })),
      ],
    };
  });
}

/**
 * The seed registry for a multi-project run: ONLY the given throwaway projects, the
 * first one active. Same quoting rules as {@link seedRegistryToml}. Pure.
 */
export function seedDemoRegistryToml(projects) {
  const lines = [`active = "${projects[0].name}"`, ""];
  for (const p of projects) {
    lines.push(`[projects.${p.name}]`, `path = '${p.root}'`, "");
    for (const w of p.worktrees) {
      lines.push(
        `[[projects.${p.name}.worktrees]]`,
        `name = "${w.name}"`,
        `path = "${w.path.replace(/\\/g, "/")}"`,
        "",
      );
    }
  }
  return lines.join("\n");
}

/**
 * The environment the isolated herdr dev app runs under. Pure and exported so the
 * isolation contract is asserted in tests, not just observed by running the app.
 * KEITORA_CONFIG_HOME is set here precisely so the registry
 * is the throwaway one, not your shared real registry.
 */
export function herdrDevEnv(base, { keitoraHome, configHome, herdrBin }) {
  // herdr locates its socket via XDG_*, NOT via KEITORA_HOME/KEITORA_CONFIG_HOME.
  // The Rust backend (backend/herdr/env.rs) runs the isolated server under
  // <KEITORA_HOME>/herdr/{config,state,…}; we MUST mirror that here so any herdr
  // CLI call from this script (notably the `server stop` on teardown) targets the
  // isolated server and never the user's real one.
  const herdrHome = join(keitoraHome, "herdr");
  return {
    ...base,
    KEITORA_HOME: keitoraHome,
    KEITORA_CONFIG_HOME: configHome,
    KEITORA_HERDR_BIN: herdrBin,
    // `herdr integration install` writes the REAL ~/.claude / ~/.codex (global, not
    // sandboxable by XDG_*): never let a dev run install it.
    KEITORA_HERDR_NO_INTEGRATION: "1",
    XDG_CONFIG_HOME: join(herdrHome, "config"),
    XDG_STATE_HOME: join(herdrHome, "state"),
    XDG_DATA_HOME: join(herdrHome, "data"),
    XDG_CACHE_HOME: join(herdrHome, "cache"),
    XDG_RUNTIME_DIR: join(herdrHome, "runtime"),
    HERDR_CONFIG_PATH: join(herdrHome, "config", "herdr", "config.toml"),
  };
}

/**
 * Check that herdr's longest socket under this KEITORA_HOME,
 * `<keitoraHome>/herdr/config/herdr/herdr.sock.agent`, fits a unix socket path
 * (103 bytes on macOS, 107 elsewhere); a longer one makes every herdr call fail
 * inside herdr. Returns null when fine (always on Windows) or a readable error.
 * Pure.
 */
export function checkSocketPath(keitoraHome, platform = process.platform) {
  if (platform === "win32") return null;
  const limit = platform === "darwin" ? 103 : 107;
  const sock = join(keitoraHome, "herdr", "config", "herdr", "herdr.sock.agent");
  const len = Buffer.byteLength(sock);
  if (len <= limit) return null;
  return (
    `herdr socket path ${sock} is ${len} bytes, over the ${limit}-byte unix socket limit; ` +
    "set KEITORA_DEV_HOME to a shorter path"
  );
}

/**
 * Whether `child` is `dir` or lies inside it, comparing case-insensitively on Windows.
 * Pure.
 */
export function isWithin(child, dir, platform = process.platform) {
  const path = platform === "win32" ? win32 : posix;
  const norm = (x) => {
    const r = path.resolve(x);
    return platform === "win32" ? r.toLowerCase() : r;
  };
  const rel = path.relative(norm(dir), norm(child));
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

/**
 * Decide, from `herdr status server --json` run with the isolated env, whether this
 * run really has its own herdr. herdr does not honour the XDG_* isolation on every
 * platform (on Windows it reaches the user's real server regardless), and a dev app
 * that adopts the real server spawns into it, and the teardown stop would then kill
 * every real session. So refuse unless the server herdr points at is provably ours:
 * not already running, and its socket (when reported) under the isolated herdr home.
 * `status` is the parsed JSON, or null when the query failed. Pure.
 */
export function isolationVerdict(status, herdrHome, platform = process.platform) {
  if (status === null) {
    return { ok: false, reason: "could not query herdr status with the isolated environment" };
  }
  const socket = typeof status.socket === "string" ? status.socket : null;
  if (socket && !isWithin(socket, herdrHome, platform)) {
    return {
      ok: false,
      reason: `herdr ignores the isolated environment here: it points at ${socket}, outside ${herdrHome}`,
    };
  }
  if (status.running === true) {
    return {
      ok: false,
      reason: `a herdr server is already reachable with the isolated environment${socket ? ` (${socket})` : ""}; it may be your real one`,
    };
  }
  return { ok: true, reason: null };
}

/**
 * Whether the teardown may run `herdr server stop`: only when herdr reports a socket
 * under the isolated herdr home, i.e. the server it would stop is this run's own.
 * Anything else (no status, no socket, a socket elsewhere) leaves the server alone.
 * Pure.
 */
export function mayStopOnTeardown(status, herdrHome, platform = process.platform) {
  const socket = status && typeof status.socket === "string" ? status.socket : null;
  return socket !== null && isWithin(socket, herdrHome, platform);
}

/** `herdr status server --json` under `env`, parsed; null on any failure. Read-only. */
function herdrStatus(herdrBin, env) {
  const r = spawnSync(herdrBin, ["status", "server", "--json"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
    env,
    timeout: 10_000,
  });
  if (r.status !== 0 || !r.stdout) return null;
  try {
    return JSON.parse(r.stdout);
  } catch {
    return null;
  }
}

/** Cargo's target directory (honours CARGO_TARGET_DIR / build.target-dir). */
function targetDir() {
  try {
    const out = execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    const dir = JSON.parse(out).target_directory;
    if (dir) return dir;
  } catch {
    /* fall through */
  }
  return join(repoRoot, "target");
}

/** Create (once) a throwaway git repo at `repoPath` for the app to spawn into. */
function ensureTestRepo(repoPath) {
  if (existsSync(join(repoPath, ".git"))) return;
  mkdirSync(repoPath, { recursive: true });
  const git = (args) =>
    spawnSync("git", args, { cwd: repoPath, stdio: "ignore", env: process.env });
  git(["init", "-q"]);
  git(["config", "user.email", "dev@keitora.local"]);
  git(["config", "user.name", "keitora herdr dev"]);
  writeFileSync(
    join(repoPath, "README.md"),
    "# herdr dev test repo\n\nThrowaway project for testing the herdr PTY backend.\n",
  );
  git(["add", "-A"]);
  git(["commit", "-q", "-m", "init herdr dev test repo"]);
}

/** Create (once) a throwaway demo project: its repo plus its extra git worktrees. */
function ensureDemoProject(project) {
  ensureTestRepo(project.root);
  for (const w of project.worktrees) {
    if (w.path === project.root || existsSync(w.path)) continue;
    spawnSync("git", ["worktree", "add", "-q", "-b", w.name, w.path], {
      cwd: project.root,
      stdio: "ignore",
      env: process.env,
    });
  }
}

/** Free a listening TCP port (Windows), best-effort. */
function killPort(port) {
  if (!isWindows) return;
  const script =
    `Get-NetTCPConnection -LocalPort ${port} -State Listen -ErrorAction SilentlyContinue |` +
    " ForEach-Object { Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue }";
  spawnSync("powershell", ["-NoProfile", "-NonInteractive", "-Command", script], {
    stdio: "ignore",
  });
}

function waitForVite(url, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const r = spawnSync("curl", ["-s", "-o", isWindows ? "NUL" : "/dev/null", url], {
      stdio: "ignore",
    });
    if (r.status === 0) return true;
    spawnSync(isWindows ? "cmd" : "sh", isWindows ? ["/c", "timeout", "/t", "1"] : ["-c", "sleep 1"], {
      stdio: "ignore",
    });
  }
  return false;
}

function main() {
  const devHome = `${devHomeFor()}-herdr`;
  const p = herdrDevPaths(devHome);

  const socketError = checkSocketPath(p.keitoraHome);
  if (socketError) {
    console.error(`[dev:herdr] ${socketError}`);
    process.exit(1);
  }

  const herdrBin = discoverHerdrBin();
  if (!herdrBin) {
    console.error("[dev:herdr] no herdr found. Install herdr or set KEITORA_HERDR_BIN.");
    process.exit(1);
  }

  // Before touching anything, prove herdr really is isolated under this env. If it
  // reaches an existing server (the user's real one), the app would adopt it and the
  // teardown stop would kill every real session.
  const isolatedEnv = herdrDevEnv(process.env, {
    keitoraHome: p.keitoraHome,
    configHome: p.configHome,
    herdrBin,
  });
  const herdrHome = join(p.keitoraHome, "herdr");
  const verdict = isolationVerdict(herdrStatus(herdrBin, isolatedEnv), herdrHome);
  if (!verdict.ok) {
    console.error(`[dev:herdr] refusing to launch: ${verdict.reason}.`);
    console.error("[dev:herdr] nothing was started or stopped.");
    process.exit(1);
  }

  // (3) Purge accumulated herdr state so the server restores NO prior layout.
  rmSync(p.herdrStateDir, { recursive: true, force: true });
  // (2) Fresh registry, pre-seeded with ONLY the throwaway project.
  rmSync(p.configHome, { recursive: true, force: true });
  mkdirSync(join(p.configHome, "keitora"), { recursive: true });
  // (1) Throwaway repo(s) for the app's cwd + the registered project(s).
  // KEITORA_DEV_PROJECTS=N seeds N throwaway projects instead of one, for UI that
  // needs several (e.g. the rail's project groups).
  const projectCount = demoProjectCount();
  const appBin = join(targetDir(), "debug", `keitora-app${EXE}`);
  // The session hooks run this CLI, built below next to the app.
  const cliBin = join(targetDir(), "debug", `keitora${EXE}`);
  // Each project's task DB lives in the dev home; see `seedProjectToml`.
  const writeProjectToml = (name, root) =>
    writeFileSync(
      join(root, "keitora.toml"),
      seedProjectToml(name, join(p.tasksDir, name, "tasks.db"), cliBin),
    );
  let appCwd = p.repoPath;
  // Every directory an agent will start in, for the Claude trust seed below.
  let agentDirs = [p.repoPath];
  if (projectCount > 1) {
    const projects = demoProjects(devHome, projectCount);
    for (const project of projects) {
      ensureDemoProject(project);
      writeProjectToml(project.name, project.root);
    }
    appCwd = projects[0].root;
    agentDirs = projects.flatMap((project) => project.worktrees.map((w) => w.path));
    writeFileSync(join(p.configHome, "keitora", "keitora.toml"), seedDemoRegistryToml(projects));
  } else {
    ensureTestRepo(p.repoPath);
    writeProjectToml("herdr-test-repo", p.repoPath);
    writeFileSync(join(p.configHome, "keitora", "keitora.toml"), seedRegistryToml(p.repoPath));
  }

  // Pre-trust the throwaway repos in a dev-home Claude config, when one can be used.
  const claude = claudeDevConfig(process.env, devHome);
  if (claude) {
    mkdirSync(claude.configDir, { recursive: true });
    let existing = null;
    try {
      existing = JSON.parse(readFileSync(claude.configFile, "utf8"));
    } catch {
      /* first run, or unreadable: start fresh */
    }
    writeFileSync(
      claude.configFile,
      `${JSON.stringify(trustedClaudeConfig(existing, agentDirs), null, 2)}\n`,
    );
  }

  // Build the app and the CLI its session hooks call (tauri dev would build the app,
  // but we bypass it for cwd control).
  try {
    execFileSync("cargo", ["build", "-p", "keitora", "-p", "keitora-cli"], {
      stdio: "inherit",
      cwd: repoRoot,
    });
  } catch {
    console.error("[dev:herdr] cargo build failed — not launching.");
    process.exit(1);
  }
  if (!existsSync(appBin)) {
    console.error(`[dev:herdr] app binary missing at ${appBin} — not launching.`);
    process.exit(1);
  }

  // Vite dev server (the debug app loads http://localhost:1420).
  const vite = spawn(process.execPath, [join(repoRoot, "node_modules", "vite", "bin", "vite.js")], {
    cwd: repoRoot,
    stdio: "inherit",
    env: process.env,
  });
  if (!waitForVite("http://localhost:1420", 90_000)) {
    console.error("[dev:herdr] Vite did not come up on :1420 — aborting.");
    vite.kill();
    process.exit(1);
  }

  console.log(`[dev:herdr] KEITORA_HOME        ${p.keitoraHome}`);
  console.log(`[dev:herdr] KEITORA_CONFIG_HOME ${p.configHome} (fresh; throwaway project only)`);
  console.log(`[dev:herdr] KEITORA_HERDR_BIN   ${herdrBin}`);
  console.log(`[dev:herdr] app cwd             ${appCwd} (throwaway repo)`);
  console.log(`[dev:herdr] task DBs            ${p.tasksDir}`);
  if (projectCount > 1) console.log(`[dev:herdr] seeded ${projectCount} throwaway projects`);
  if (claude) {
    console.log(`[dev:herdr] CLAUDE_CONFIG_DIR   ${claude.configDir} (throwaway repos pre-trusted)`);
  } else {
    console.log(
      "[dev:herdr] Claude uses your real config: a new dev repo asks once to be trusted. " +
        "Set CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`) to use an isolated, pre-trusted one.",
    );
  }
  console.log("[dev:herdr] ISOLATED: no real project registered, herdr state purged.");

  // The agents inherit the app's environment (through the herdr server it starts).
  const app = spawn(appBin, [], {
    cwd: appCwd,
    stdio: "inherit",
    env: { ...isolatedEnv, ...(claude?.env ?? {}) },
  });

  const shutdown = () => {
    try {
      app.kill();
    } catch {
      /* ignore */
    }
    try {
      vite.kill();
    } catch {
      /* ignore */
    }
    killPort(1420);
    // Stop the isolated herdr server so no agents linger after the window closes, but
    // only once herdr confirms the server it would stop is this run's own.
    if (mayStopOnTeardown(herdrStatus(herdrBin, isolatedEnv), herdrHome)) {
      spawnSync(herdrBin, ["server", "stop"], { stdio: "ignore", env: isolatedEnv });
    } else {
      console.error("[dev:herdr] not stopping herdr: could not confirm it is the isolated server.");
    }
  };
  app.on("exit", (code, signal) => {
    shutdown();
    process.exit(code ?? (signal ? 1 : 0));
  });
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
