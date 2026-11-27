import { describe, expect, it } from "vitest";
import { CLI_KINDS } from "./providers";
import type { SettingsSnapshot } from "./settings";
import {
  buildProviderSaves,
  defaultCliButtons,
  emptyForms,
  seedProjectForms,
  seedUserForms,
} from "./settingsForm";

function snapshot(): SettingsSnapshot {
  return {
    project: {
      name: "proj",
      root: "D:/repos/proj",
      db_path: "D:/repos/proj/tasks.db",
      worktrees_dir: null,
      archive_days: 30,
      log_retention_days: 90,
      providers: {
        claude: { path: "/p/claude", autonomy: false, dispatch_steps: ["/clear"] },
        codex: { path: null, autonomy: false, dispatch_steps: [] },
        antigravity: { path: "/p/agy", autonomy: false, dispatch_steps: ["/a"] },
      },
      diff_base: null,
      worktrees: [],
      has_committed_file: true,
    },
    user: {
      active: "proj",
      projects: [],
      providers: {
        claude: { path: "/u/claude", autonomy: true },
        codex: { path: null, autonomy: false },
        antigravity: { path: "/u/agy", autonomy: true },
      },
      default_cli: "claude",
      available: { claude: true, codex: true, antigravity: true },
      auto_check_updates: true,
      agent_update_check_hours: 6,
    },
  };
}

describe("settingsForm", () => {
  it("seedProjectForms changes only projPath and steps", () => {
    const forms = emptyForms();
    forms.antigravity = { ...forms.antigravity, userPath: "edited", autonomy: true };
    const out = seedProjectForms(forms, snapshot());
    expect(out.antigravity).toEqual({
      userPath: "edited",
      projPath: "/p/agy",
      steps: ["/a"],
      autonomy: true,
    });
    expect(out.codex.projPath).toBe("");
    expect(out.claude.steps).toEqual(["/clear"]);
  });

  it("seedUserForms changes only userPath and autonomy", () => {
    const forms = emptyForms();
    forms.claude = { ...forms.claude, projPath: "edited", steps: ["/x"] };
    const out = seedUserForms(forms, snapshot());
    expect(out.claude).toEqual({
      userPath: "/u/claude",
      projPath: "edited",
      steps: ["/x"],
      autonomy: true,
    });
    expect(out.antigravity.userPath).toBe("/u/agy");
    expect(out.codex.autonomy).toBe(false);
  });

  it("buildProviderSaves trims steps, drops blank ones and nulls blank paths for every provider", () => {
    const forms = emptyForms();
    forms.claude = {
      userPath: "  ",
      projPath: " /p/claude ",
      steps: [" /clear ", "  "],
      autonomy: false,
    };
    forms.antigravity = { userPath: " /u/agy ", projPath: "", steps: ["", "/a"], autonomy: false };
    const { project, user } = buildProviderSaves(forms);
    expect(project).toEqual({
      claude: { path: "/p/claude", dispatchSteps: ["/clear"] },
      codex: { path: null, dispatchSteps: [] },
      antigravity: { path: null, dispatchSteps: ["/a"] },
    });
    expect(user).toEqual({ claude: null, codex: null, antigravity: "/u/agy" });
  });

  it("defaultCliButtons lists every provider, disabling unavailable ones but keeping the selection pressed", () => {
    const buttons = defaultCliButtons({ claude: true, codex: false, antigravity: true }, "codex");
    expect(buttons.map((b) => b.cli)).toEqual([...CLI_KINDS]);
    expect(buttons.map((b) => b.disabled)).toEqual([false, true, false]);
    expect(buttons.map((b) => b.pressed)).toEqual([false, true, false]);
  });
});
