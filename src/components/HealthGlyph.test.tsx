import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("../lib/transport", () => ({
  getTransport: () => ({ invoke: vi.fn(), listen: vi.fn(), channel: vi.fn() }),
}));

import type { HealthFinding } from "../lib/ipc";
import { HealthSectionView } from "./HealthSection";
import { HealthGlyph } from "./rail/HealthGlyph";

const T0 = Date.UTC(2026, 0, 1, 10, 0, 0);

function finding(over: Partial<HealthFinding> = {}): HealthFinding {
  return {
    project: "proj",
    worktree: "bravo",
    checkId: "hooks_vs_herdr",
    severity: "warn",
    message: "first message",
    detail: {},
    firstSeen: T0,
    lastSeen: T0 + 60_000,
    occurrences: 2,
    clearedAt: null,
    remedy: "",
    remedyAt: null,
    ...over,
  };
}

const glyph = (findings: HealthFinding[], scope: "lane" | "project" = "lane") =>
  renderToStaticMarkup(<HealthGlyph findings={findings} scope={scope} />);

describe("HealthGlyph", () => {
  it("renders nothing for info or no findings", () => {
    expect(glyph([])).toBe("");
    expect(glyph([finding({ severity: "info" })])).toBe("");
  });

  it("is an amber button for warn with the issue count", () => {
    const html = glyph([finding()]);
    expect(html).toContain('class="health-glyph warn"');
    expect(html).toContain("<button");
    expect(html).toContain('aria-label="Health: 1 issue"');
    expect(html).toContain('title="first message"');
  });

  it("is red for error, tooltip ordered with +n more", () => {
    const html = glyph([
      finding(),
      finding({ checkId: "hooks_missing", severity: "error", message: "the error", lastSeen: 1 }),
    ]);
    expect(html).toContain('class="health-glyph error"');
    expect(html).toContain('aria-label="Health: 2 issues"');
    expect(html).toContain('title="the error\n(+1 more)"');
  });

  it("is not a button in a project header", () => {
    const html = glyph([finding()], "project");
    expect(html).not.toContain("<button");
    expect(html).toContain('role="img"');
  });
});

describe("HealthSectionView", () => {
  const view = (findings: HealthFinding[]) =>
    renderToStaticMarkup(
      <HealthSectionView findings={findings} now={T0 + 120_000} onCopy={() => {}} />,
    );

  it("says no issues when nothing is active", () => {
    const html = view([]);
    expect(html).toContain("no issues");
    expect(html).toContain("copy as JSON");
    expect(html).not.toContain("recently cleared");
  });

  it("groups by project then worktree and shows the row facts", () => {
    const html = view([
      finding({ remedy: "reinstalled hooks", remedyAt: T0 }),
      finding({ worktree: "", checkId: "pipeline_stalled", message: "proj: request 4 pending" }),
      finding({ project: "", worktree: "", checkId: "herdr_link", message: "herdr unavailable" }),
    ]);
    expect(html).not.toContain("no issues");
    const app = html.indexOf('data-project=""');
    const proj = html.indexOf('data-project="proj"');
    expect(app).toBeGreaterThanOrEqual(0);
    expect(proj).toBeGreaterThan(app);
    // App-level findings carry no worktree heading.
    expect(html.slice(app, proj)).not.toContain(">project<");
    const projectLevel = html.indexOf('data-worktree=""', proj);
    const bravo = html.indexOf('data-worktree="bravo"', proj);
    expect(bravo).toBeGreaterThan(projectLevel);
    expect(html).toContain("@bravo");
    expect(html).toContain("hooks_vs_herdr");
    expect(html).toContain("last seen 60s ago");
    expect(html).toContain("2x");
    expect(html).toContain("fixed by: reinstalled hooks");
  });

  it("puts cleared findings in a collapsed recently cleared group", () => {
    const html = view([finding({ clearedAt: T0 + 90_000 })]);
    expect(html).toContain("no issues");
    expect(html).toContain("<details");
    expect(html).not.toContain("<details open");
    expect(html).toContain("recently cleared (1)");
    expect(html).toContain("hooks_vs_herdr");
  });
});
