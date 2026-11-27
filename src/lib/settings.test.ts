import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => core);

import { type SettingsSnapshot, saveProjectSettings, saveUserSettings } from "./settings";

// The JSON `get_settings` returns, typed against the exported interfaces: a missing or
// renamed field is a compile error.
const SNAPSHOT: SettingsSnapshot = {
  project: {
    name: "proj",
    root: "D:/repos/proj",
    db_path: "D:/repos/proj/tasks.db",
    worktrees_dir: null,
    archive_days: 30,
    log_retention_days: 90,
    providers: {
      claude: { path: null, autonomy: false, dispatch_steps: ["/clear"] },
      codex: { path: "/home/u/bin/codex", autonomy: true, dispatch_steps: [] },
      antigravity: { path: null, autonomy: false, dispatch_steps: ["/a"] },
    },
    diff_base: null,
    worktrees: ["alpha"],
    has_committed_file: true,
  },
  user: {
    active: "proj",
    projects: [{ name: "proj", root: "D:/repos/proj", active: true }],
    providers: {
      claude: { path: null, autonomy: true },
      codex: { path: null, autonomy: false },
      antigravity: { path: "/home/u/bin/agy", autonomy: false },
    },
    default_cli: "claude",
    available: { claude: true, codex: false, antigravity: true },
    auto_check_updates: true,
    agent_update_check_hours: 6,
  },
};

describe("settings wire shapes", () => {
  beforeEach(() => {
    core.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("the snapshot is keyed by provider", () => {
    expect(Object.keys(SNAPSHOT.project.providers)).toEqual(["claude", "codex", "antigravity"]);
    expect(Object.keys(SNAPSHOT.user.providers)).toEqual(["claude", "codex", "antigravity"]);
    expect(Object.keys(SNAPSHOT.user.available)).toEqual(["claude", "codex", "antigravity"]);
  });

  it("saveProjectSettings sends a providers object keyed by every provider", async () => {
    const providers = {
      claude: { path: null, dispatchSteps: ["/clear"] },
      codex: { path: "/home/u/bin/codex", dispatchSteps: [] },
      antigravity: { path: null, dispatchSteps: ["/a"] },
    };
    await saveProjectSettings({
      archiveDays: 30,
      logRetentionDays: 90,
      worktreesDir: null,
      providers,
      diffBase: null,
    });
    expect(core.invoke).toHaveBeenCalledWith("save_project_settings", {
      archiveDays: 30,
      logRetentionDays: 90,
      worktreesDir: null,
      providers,
      diffBase: null,
    });
  });

  it("saveUserSettings sends a path per provider", async () => {
    const providers = { claude: null, codex: "/home/u/bin/codex", antigravity: null };
    await saveUserSettings({ providers, defaultCli: "antigravity" });
    expect(core.invoke).toHaveBeenCalledWith("save_user_settings", {
      providers,
      defaultCli: "antigravity",
    });
  });
});
