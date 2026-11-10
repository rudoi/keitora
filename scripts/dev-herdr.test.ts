// @ts-nocheck
import { describe, expect, it } from "vitest";

import { isAbsolute, join, resolve } from "node:path";

import {
  checkSocketPath,
  demoProjectCount,
  demoProjects,
  devHomeFor,
  discoverHerdrBin,
  herdrDevEnv,
  herdrDevPaths,
  isolationVerdict,
  isWithin,
  mayStopOnTeardown,
  parseReleaseVersion,
  seedDemoRegistryToml,
  seedProjectToml,
  seedRegistryToml,
  sortReleasesNewestFirst,
} from "./dev-herdr.mjs";

describe("devHomeFor", () => {
  it("defaults under .keitora-dev, keyed by the checkout", () => {
    expect(devHomeFor({}, "/repos/wt/epsilon")).toMatch(/[\\/]\.keitora-dev[\\/]epsilon-[0-9a-f]{8}$/);
  });

  it("gives each git worktree its own home", () => {
    // Two worktrees sharing a dev home would share a herdr server, so the second
    // app would attach to the first's sessions.
    expect(devHomeFor({}, "/repos/wt/epsilon")).not.toBe(devHomeFor({}, "/repos/wt/gamma"));
  });

  it("distinguishes same-named checkouts in different locations", () => {
    expect(devHomeFor({}, "/a/proj")).not.toBe(devHomeFor({}, "/b/proj"));
  });

  it("is stable for the same checkout", () => {
    expect(devHomeFor({}, "/repos/wt/epsilon")).toBe(devHomeFor({}, "/repos/wt/epsilon"));
  });

  it("honours KEITORA_DEV_HOME for a second sandbox", () => {
    expect(devHomeFor({ KEITORA_DEV_HOME: "/tmp/sandbox" }, "/repos/wt/epsilon")).toBe(
      resolve("/tmp/sandbox"),
    );
  });

  it("absolutises a relative override", () => {
    // `tauri dev` runs the app with cwd=src-tauri, so a relative home would be created
    // in one directory and resolved by the app in another.
    expect(isAbsolute(devHomeFor({ KEITORA_DEV_HOME: "devhome" }, "/repos/wt/epsilon"))).toBe(true);
  });

  it("does not point at the production ~/.keitora", () => {
    expect(devHomeFor({}, "/repos/wt/epsilon")).not.toMatch(/[\\/]\.keitora$/);
  });
});

describe("herdrDevPaths", () => {
  it("roots every isolated path under the given herdr dev home", () => {
    const p = herdrDevPaths("/dev/home-herdr");
    expect(p.keitoraHome).toBe("/dev/home-herdr");
    // state + registry + throwaway repo are all under the dev home (so purging is safe).
    expect(p.herdrStateDir.replace(/\\/g, "/")).toBe("/dev/home-herdr/herdr");
    expect(p.configHome.replace(/\\/g, "/")).toBe("/dev/home-herdr/config");
    // The repo directory is named like its worktree: hooks report by basename.
    expect(p.repoPath.replace(/\\/g, "/")).toBe("/dev/home-herdr/herdr-test-repo");
    expect(p.tasksDir.replace(/\\/g, "/")).toBe("/dev/home-herdr/tasks");
  });

  it("names the throwaway repo directory after its registered worktree", () => {
    const p = herdrDevPaths("C:\\scratch\\dev");
    expect(seedRegistryToml(p.repoPath)).toContain('name = "herdr-test-repo"');
    expect(p.repoPath.replace(/\\/g, "/").split("/").pop()).toBe("herdr-test-repo");
  });
});

describe("seedProjectToml", () => {
  it("pins the task DB and the hook CLI to the given (dev home) paths", () => {
    const toml = seedProjectToml(
      "herdr-test-repo",
      "C:\\scratch\\dev\\tasks\\herdr-test-repo\\tasks.db",
      "D:\\repos\\proj\\target\\debug\\keitora.exe",
    );
    expect(toml).toContain('name = "herdr-test-repo"');
    expect(toml).toContain("db_path = 'C:/scratch/dev/tasks/herdr-test-repo/tasks.db'");
    expect(toml).toContain("[cli]\npath = 'D:/repos/proj/target/debug/keitora.exe'");
    // Never the default `~/.keitora/...` DB.
    expect(toml).not.toContain("~");
  });
});

describe("seedRegistryToml", () => {
  it("registers ONLY the throwaway project (no real project can slip in)", () => {
    const toml = seedRegistryToml("C:\\scratch\\test-repo");
    expect(toml).toContain('active = "herdr-test-repo"');
    expect(toml).toContain("[projects.herdr-test-repo]");
    // Exactly one project TABLE header (worktree sub-tables don't count).
    expect(toml.match(/^\[projects\.[^.\]]+\]$/gm)?.length).toBe(1);
    // Project path is the literal throwaway path; worktree path is forward-slashed.
    expect(toml).toContain("path = 'C:\\scratch\\test-repo'");
    expect(toml).toContain('path = "C:/scratch/test-repo"');
  });
});

describe("demoProjectCount", () => {
  it("defaults to the single-project run", () => {
    expect(demoProjectCount({})).toBe(1);
    expect(demoProjectCount({ KEITORA_DEV_PROJECTS: "" })).toBe(1);
    expect(demoProjectCount({ KEITORA_DEV_PROJECTS: "nope" })).toBe(1);
    expect(demoProjectCount({ KEITORA_DEV_PROJECTS: "0" })).toBe(1);
  });

  it("reads KEITORA_DEV_PROJECTS and clamps to the available demo names", () => {
    expect(demoProjectCount({ KEITORA_DEV_PROJECTS: "6" })).toBe(6);
    expect(demoProjectCount({ KEITORA_DEV_PROJECTS: "99" })).toBe(10);
  });
});

describe("demoProjects / seedDemoRegistryToml", () => {
  const projects = demoProjects("C:\\scratch\\dev", 4);

  it("roots every project and worktree under <devHome>/demo", () => {
    for (const p of projects) {
      expect(p.root.startsWith(join("C:\\scratch\\dev", "demo"))).toBe(true);
      for (const w of p.worktrees) expect(w.path.startsWith(join("C:\\scratch\\dev", "demo"))).toBe(true);
    }
  });

  it("makes the repo root the main worktree and varies group sizes", () => {
    for (const p of projects) expect(p.worktrees[0]).toEqual({ name: "main", path: p.root });
    expect(new Set(projects.map((p) => p.worktrees.length)).size).toBeGreaterThan(1);
  });

  it("registers ONLY the demo projects, the first one active", () => {
    const toml = seedDemoRegistryToml(projects);
    expect(toml).toContain(`active = "${projects[0].name}"`);
    expect(toml.match(/^\[projects\.[^.\]]+\]$/gm)?.length).toBe(4);
    // Worktree paths are forward-slashed; project paths stay literal.
    expect(toml).not.toMatch(/^path = ".*\\/m);
    expect(toml).toContain(`path = '${projects[0].root}'`);
  });
});

const W = (s: string) => s.replaceAll("/", "\\");

describe("isWithin", () => {
  it("matches the dir itself and paths inside it, case-insensitively on Windows", () => {
    expect(isWithin(W("C:/dev/herdr"), W("C:/dev/herdr"), "win32")).toBe(true);
    expect(isWithin(W("c:/DEV/herdr/config/herdr.sock"), W("C:/dev/herdr"), "win32")).toBe(true);
    expect(isWithin("/dev/herdr/state/s.sock", "/dev/herdr", "linux")).toBe(true);
  });

  it("rejects siblings, parents and lookalike prefixes", () => {
    expect(isWithin(W("C:/dev/herdr-real/s.sock"), W("C:/dev/herdr"), "win32")).toBe(false);
    expect(isWithin(W("C:/dev"), W("C:/dev/herdr"), "win32")).toBe(false);
    expect(isWithin("/home/u/.config/herdr/s.sock", "/dev/herdr", "linux")).toBe(false);
  });
});

describe("isolationVerdict", () => {
  const home = W("C:/keitora-dev/x-herdr/herdr");
  const inside = W("C:/keitora-dev/x-herdr/herdr/config/herdr.sock");

  it("refuses when herdr reaches an already-running server (it may be the real one)", () => {
    const v = isolationVerdict({ running: true, socket: inside }, home, "win32");
    expect(v.ok).toBe(false);
    expect(v.reason).toMatch(/already reachable/);
  });

  it("refuses when herdr points outside the isolated home, running or not", () => {
    for (const running of [true, false]) {
      const v = isolationVerdict({ running, socket: W("//./pipe/herdr") }, home, "win32");
      expect(v.ok).toBe(false);
      expect(v.reason).toMatch(/ignores the isolated environment/);
    }
  });

  it("refuses when the status query failed", () => {
    expect(isolationVerdict(null, home, "win32").ok).toBe(false);
  });

  it("allows a stopped server whose socket is under the isolated home, or none reported", () => {
    expect(isolationVerdict({ running: false, socket: inside }, home, "win32").ok).toBe(true);
    expect(isolationVerdict({ running: false }, home, "win32").ok).toBe(true);
  });
});

describe("mayStopOnTeardown", () => {
  const home = "/k/dev-herdr/herdr";

  it("stops only a server whose socket is under the isolated home", () => {
    expect(mayStopOnTeardown({ running: true, socket: `${home}/config/herdr.sock` }, home, "linux")).toBe(true);
  });

  it("never stops on a missing status, a missing socket, or a socket elsewhere", () => {
    expect(mayStopOnTeardown(null, home, "linux")).toBe(false);
    expect(mayStopOnTeardown({ running: true }, home, "linux")).toBe(false);
    expect(
      mayStopOnTeardown({ running: true, socket: "/home/u/.config/herdr/herdr.sock" }, home, "linux"),
    ).toBe(false);
  });
});

describe("herdrDevEnv", () => {
  const opts = {
    keitoraHome: "/dev/home-herdr",
    configHome: "/dev/home-herdr/config",
    herdrBin: "/opt/herdr/herdr",
  };

  it("pins herdr and isolates home + registry", () => {
    const env = herdrDevEnv({}, opts);
    expect(env.KEITORA_HERDR_BIN).toBe("/opt/herdr/herdr");
    expect(env.KEITORA_HOME).toBe("/dev/home-herdr");
    // The registry is the throwaway one, NOT the shared real registry.
    expect(env.KEITORA_CONFIG_HOME).toBe("/dev/home-herdr/config");
  });

  it("sets no daemon-binary or backend-selector vars (herdr is the only backend)", () => {
    const env = herdrDevEnv({}, opts);
    expect(env.KEITORA_PTYD_BIN).toBeUndefined();
    expect(env.KEITORA_PTY_BACKEND).toBeUndefined();
  });

  it("opts out of herdr integration install (it writes the real ~/.claude/~/.codex)", () => {
    expect(herdrDevEnv({}, opts).KEITORA_HERDR_NO_INTEGRATION).toBe("1");
  });

  it("overrides an inherited real KEITORA_CONFIG_HOME (never reuses the shared registry)", () => {
    const env = herdrDevEnv({ KEITORA_CONFIG_HOME: "/real/config" }, opts);
    expect(env.KEITORA_CONFIG_HOME).toBe("/dev/home-herdr/config");
  });

  it("sets XDG_* under <keitoraHome>/herdr so herdr CLI calls (e.g. teardown stop) hit the ISOLATED server, not the real one", () => {
    // herdr locates its socket via XDG_CONFIG_HOME; must match the Rust env.rs layout.
    const env = herdrDevEnv({ XDG_CONFIG_HOME: "/real/.config" }, opts);
    const norm = (p: string) => p.replace(/\\/g, "/");
    expect(norm(env.XDG_CONFIG_HOME)).toBe("/dev/home-herdr/herdr/config");
    expect(norm(env.XDG_STATE_HOME)).toBe("/dev/home-herdr/herdr/state");
    expect(norm(env.XDG_RUNTIME_DIR)).toBe("/dev/home-herdr/herdr/runtime");
    expect(norm(env.HERDR_CONFIG_PATH)).toBe("/dev/home-herdr/herdr/config/herdr/config.toml");
  });

  it("passes the rest of the environment through untouched", () => {
    const env = herdrDevEnv({ PATH: "/usr/bin", FOO: "bar" }, opts);
    expect(env.PATH).toBe("/usr/bin");
    expect(env.FOO).toBe("bar");
  });

  it("does not mutate the base environment", () => {
    const base = { KEITORA_CONFIG_HOME: "/real/config" };
    herdrDevEnv(base, opts);
    expect(base.KEITORA_CONFIG_HOME).toBe("/real/config");
  });
});

describe("checkSocketPath", () => {
  const tail = "/herdr/config/herdr/herdr.sock.agent";
  // A KEITORA_HOME that makes the socket path exactly `len` bytes.
  const homeFor = (len: number) => `/${"h".repeat(len - tail.length - 1)}`;

  it("accepts a path exactly at the macOS limit and one under", () => {
    expect(checkSocketPath(homeFor(103), "darwin")).toBeNull();
    expect(checkSocketPath(homeFor(102), "darwin")).toBeNull();
  });

  it("rejects one byte over the macOS limit with a readable message", () => {
    const msg = checkSocketPath(homeFor(104), "darwin");
    expect(msg).toContain("104 bytes");
    expect(msg).toContain("103");
    expect(msg).toContain("herdr.sock.agent");
    expect(msg).toContain("KEITORA_DEV_HOME");
  });

  it("allows up to 107 bytes elsewhere on unix", () => {
    expect(checkSocketPath(homeFor(107), "linux")).toBeNull();
    expect(checkSocketPath(homeFor(108), "linux")).not.toBeNull();
  });

  it("never applies on Windows", () => {
    expect(checkSocketPath(homeFor(500), "win32")).toBeNull();
  });
});

describe("parseReleaseVersion", () => {
  it("reads the leading triple of a release dir name", () => {
    expect(parseReleaseVersion("0.9.10-aarch64-apple-darwin")).toEqual([0, 9, 10]);
    expect(parseReleaseVersion("v1.2.3")).toEqual([1, 2, 3]);
    expect(parseReleaseVersion("1.2")).toBeNull();
    expect(parseReleaseVersion("latest")).toBeNull();
  });
});

describe("sortReleasesNewestFirst", () => {
  it("orders by version, not by string, with unparsable names last", () => {
    expect(sortReleasesNewestFirst(["0.9.3-x", "0.9.10-x", "junk", "0.10.0-x", "0.9.9-x"])).toEqual([
      "0.10.0-x",
      "0.9.10-x",
      "0.9.9-x",
      "0.9.3-x",
      "junk",
    ]);
  });
});

describe("discoverHerdrBin", () => {
  const home = "/home/u";
  const releases = "/home/u/.herdr/packages/standalone/releases";
  // Inject a fake filesystem: `files` are the executable files, `dirs` the listings.
  const fsOf = (files, dirs = {}, platform = "darwin") => ({
    platform,
    isExecutable: (p) => files.includes(p.replace(/\\/g, "/")),
    listDir: (d) => dirs[d.replace(/\\/g, "/")] ?? [],
  });

  it("prefers KEITORA_HERDR_BIN over everything, unchecked", () => {
    const opts = fsOf(["/usr/local/bin/herdr"]);
    expect(discoverHerdrBin({ KEITORA_HERDR_BIN: "/x/herdr", PATH: "/usr/local/bin" }, home, opts)).toBe(
      "/x/herdr",
    );
  });

  it("picks the newest standalone release by parsed version", () => {
    const names = ["0.9.3-a", "0.10.0-a", "0.9.10-a"];
    const files = names.map((n) => `${releases}/${n}/herdr`);
    const got = discoverHerdrBin({ PATH: "/usr/bin" }, home, fsOf(files, { [releases]: names }));
    expect(got).toBe(`${releases}/0.10.0-a/herdr`);
  });

  it("skips a release dir whose herdr is not executable", () => {
    const names = ["0.10.0-a", "0.9.0-a"];
    const files = [`${releases}/0.9.0-a/herdr`];
    const got = discoverHerdrBin({}, home, fsOf(files, { [releases]: names }));
    expect(got).toBe(`${releases}/0.9.0-a/herdr`);
  });

  it("takes the first herdr on PATH", () => {
    const got = discoverHerdrBin({ PATH: "/a:/b:/c" }, home, fsOf(["/b/herdr", "/c/herdr"]));
    expect(got).toBe("/b/herdr");
  });

  it("searches PATH, then the well-known dirs, then the releases", () => {
    const names = ["0.10.0-a"];
    const release = `${releases}/0.10.0-a/herdr`;
    const dirs = { [releases]: names };
    const env = { PATH: "/a:/b" };
    // Everything present: PATH wins.
    const all = ["/b/herdr", "/usr/local/bin/herdr", release];
    expect(discoverHerdrBin(env, home, fsOf(all, dirs))).toBe("/b/herdr");
    // Off PATH: a well-known dir beats the release.
    expect(discoverHerdrBin(env, home, fsOf(all.slice(1), dirs))).toBe("/usr/local/bin/herdr");
    // Only the release is left.
    expect(discoverHerdrBin(env, home, fsOf(all.slice(2), dirs))).toBe(release);
  });

  it("searches PATH before the releases on Windows", () => {
    const names = ["0.10.0-a"];
    const files = ["C:/tools/herdr.exe", `${releases}/0.10.0-a/herdr.exe`];
    const opts = fsOf(files, { [releases]: names }, "win32");
    const got = discoverHerdrBin({ Path: "C:\\nope;C:\\tools" }, home, opts);
    expect(got?.replace(/\\/g, "/")).toBe("C:/tools/herdr.exe");
    const only = fsOf(files.slice(1), { [releases]: names }, "win32");
    expect(discoverHerdrBin({ Path: "C:\\tools" }, home, only)?.replace(/\\/g, "/")).toBe(
      `${releases}/0.10.0-a/herdr.exe`,
    );
  });

  it("falls back to the well-known unix dirs in order", () => {
    expect(discoverHerdrBin({ PATH: "/a" }, home, fsOf(["/usr/local/bin/herdr", "/home/u/.local/bin/herdr"]))).toBe(
      "/usr/local/bin/herdr",
    );
    expect(discoverHerdrBin({}, home, fsOf(["/opt/homebrew/bin/herdr", "/usr/local/bin/herdr"]))).toBe(
      "/opt/homebrew/bin/herdr",
    );
    expect(discoverHerdrBin({}, home, fsOf(["/home/u/.local/bin/herdr"]))).toBe(
      "/home/u/.local/bin/herdr",
    );
  });

  it("does not look in the well-known unix dirs on Windows", () => {
    expect(discoverHerdrBin({}, home, fsOf(["/usr/local/bin/herdr.exe"], {}, "win32"))).toBeNull();
  });

  it("returns null when nothing is found", () => {
    expect(discoverHerdrBin({ PATH: "/a" }, home, fsOf([]))).toBeNull();
  });
});
