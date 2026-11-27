// Pure driving view logic: what a lane, the rail menu and the pane banner show about a
// worktree that drives others or is driven by one. No React or Tauri imports, so the rail
// data model and its tests use it directly.

import { plural } from "./format";
import type { DriveView, WorktreeActivity } from "./ipc";
import type { SessionState, WorktreeSession } from "./sessions";

/** How a driver lane shows one of the worktrees it drives. */
export type DrivenVisual = "idle" | "busy" | "deciding" | "needs-you" | "offline";

/** What a driver lane shows about one driven worktree (tooltip rows). */
export interface DrivenSummary {
  name: string;
  visual: DrivenVisual;
  pending: number;
}

export interface LaneDrive {
  /** This lane is driven by that worktree. */
  drivenBy: string | null;
  /** Worktrees this lane drives (empty unless it is a driver). */
  drives: DrivenSummary[];
  /** Brokered prompts pending for this lane. */
  pendingPrompts: number;
  paused: boolean;
  /** The session is waiting on a pending driver decision. */
  brokered: boolean;
  /** Why a prompt was escalated; only while waiting and not brokered. */
  escalatedReason: string | null;
}

/**
 * A lane's drive facts, or null when it neither drives nor is driven. The session row is
 * event-fresh for the active project; the activity covers every project, so it fills in
 * whatever the session lacks. `drives` entries are placeholders until the rail fills them
 * from sibling lanes.
 */
export function buildLaneDrive(
  s: WorktreeSession | undefined,
  a: WorktreeActivity | undefined,
): LaneDrive | null {
  const drivenBy = s?.driven_by ?? a?.drivenBy ?? null;
  const drives: DrivenSummary[] = (a?.drives ?? []).map((name) => ({
    name,
    visual: "idle",
    pending: 0,
  }));
  if (drivenBy === null && drives.length === 0) return null;
  const brokered = s?.brokered === true && drivenBy !== null;
  return {
    drivenBy,
    drives,
    pendingPrompts: a?.pendingPrompts ?? 0,
    paused: a?.drivePaused ?? false,
    brokered,
    escalatedReason: s?.state === "waiting" && !brokered ? (s.escalated_reason ?? null) : null,
  };
}

/** A driven session waiting while its driver decides: the human is not needed. */
export function isQuietWaiting(s: WorktreeSession | undefined): boolean {
  return s?.state === "waiting" && !!s.driven_by && s.brokered === true;
}

export type DriveKind = "deciding" | "escalated" | "driven";

/** How a driven lane's badge reads; null when the lane is not driven. */
export function driveKind(d: LaneDrive | null, state: SessionState | undefined): DriveKind | null {
  if (d === null || d.drivenBy === null) return null;
  if (state === "waiting") return d.brokered ? "deciding" : "escalated";
  return "driven";
}

/** The driven lane's head-row badge: `⇠ alpha · 1 · paused`. */
export function driveBadgeText(d: LaneDrive): string {
  let text = `⇠ ${d.drivenBy ?? ""}`;
  if (d.pendingPrompts > 0) text += ` · ${d.pendingPrompts}`;
  if (d.paused) text += " · paused";
  return text;
}

/** The driven lane's badge tooltip. An escalation reason wins over paused, which wins
 *  over the plain driven/deciding wording. */
export function driveBadgeTitle(d: LaneDrive, kind: DriveKind): string {
  const driver = d.drivenBy ?? "";
  if (kind === "escalated") {
    return d.escalatedReason
      ? `${driver} escalated: ${d.escalatedReason}`
      : `${driver} escalated a prompt to you.`;
  }
  if (d.paused) return `Driven by ${driver} (paused): prompts come to you.`;
  if (kind === "deciding") {
    const n = Math.max(1, d.pendingPrompts);
    return `${driver} is deciding ${n} ${plural(n, "prompt")}.`;
  }
  return `Driven by ${driver}. Anything you type here mixes with its prompts.`;
}

/** The driver lane's head-row chip: `drives 2`. */
export function driveChipText(d: LaneDrive): string {
  return `drives ${d.drives.length}`;
}

function drivenLabel(s: DrivenSummary): string {
  switch (s.visual) {
    case "deciding": {
      const k = Math.max(1, s.pending);
      return `deciding ${k} ${plural(k, "prompt")}`;
    }
    case "needs-you":
      return "needs you";
    default:
      return s.visual;
  }
}

/** The driver chip's tooltip: a header line, then one line per driven worktree. */
export function driveChipTitle(d: LaneDrive): string {
  const n = d.drives.length;
  return [
    `Drives ${n} ${plural(n, "worktree")}:`,
    ...d.drives.map((s) => `${s.name}: ${drivenLabel(s)}`),
  ].join("\n");
}

/** A lane's drive tag: its class, tooltip and text. */
export interface DriveTag {
  className: string;
  title: string;
  text: string;
}

/** A lane's drive tag: `⇠ driver` on a driven lane (styled by whether the driver is
 *  deciding a prompt or escalated one to you), `drives N` on a driver lane, else `null`.
 *  A lane is never both. */
export function driveTag(d: LaneDrive | null, state: SessionState | undefined): DriveTag | null {
  if (d === null) return null;
  const kind = driveKind(d, state);
  if (kind !== null) {
    const className = ["lane-drive-badge", kind !== "driven" ? kind : "", d.paused ? "paused" : ""]
      .filter(Boolean)
      .join(" ");
    return { className, title: driveBadgeTitle(d, kind), text: driveBadgeText(d) };
  }
  if (d.drives.length > 0) {
    return { className: "lane-drive-chip", title: driveChipTitle(d), text: driveChipText(d) };
  }
  return null;
}

const DRIVE_MENU_ITEMS = ["take-over", "pause-driver", "resume-driver", "release"] as const;

export type DriveMenuItem = (typeof DRIVE_MENU_ITEMS)[number];

/** The lane menu's drive entries: none unless the lane is driven. */
export function driveMenuItems(d: LaneDrive | null): DriveMenuItem[] {
  if (d === null || d.drivenBy === null) return [];
  return ["take-over", d.paused ? "resume-driver" : "pause-driver", "release"];
}

export function isDriveMenuItem(i: string): i is DriveMenuItem {
  return (DRIVE_MENU_ITEMS as readonly string[]).includes(i);
}

/** Why a drive menu entry is disabled, or null when it is enabled. */
export function driveMenuDisabledReason(i: DriveMenuItem, d: LaneDrive): string | null {
  return i === "take-over" && d.pendingPrompts === 0 ? "No pending prompts" : null;
}

/** Lanes that are driven by another worktree. */
export function drivenCount(lanes: readonly { drive: LaneDrive | null }[]): number {
  return lanes.filter((l) => l.drive?.drivenBy != null).length;
}

/** The pane banner's content, or null when the worktree is not driven. */
export interface PaneDrive {
  driver: string;
  paused: boolean;
  pendingCount: number;
  /** The oldest pending prompt; only known for the active project (from `drives_list`). */
  firstPending: { toolName: string; summary: string } | null;
  escalatedReason: string | null;
  forcedFrom: string | null;
}

export function paneDrive(
  a: WorktreeActivity | undefined,
  s: WorktreeSession | undefined,
  v: DriveView | undefined,
): PaneDrive | null {
  const driver = s?.driven_by ?? a?.drivenBy ?? null;
  if (driver === null) return null;
  const first = v?.pending[0];
  const brokered = s?.brokered === true;
  return {
    driver,
    paused: a?.drivePaused ?? v?.paused ?? false,
    pendingCount: v?.pending.length ?? a?.pendingPrompts ?? 0,
    firstPending: first ? { toolName: first.tool_name, summary: first.summary } : null,
    escalatedReason: s?.state === "waiting" && !brokered ? (s.escalated_reason ?? null) : null,
    forcedFrom: v?.forced_from ?? null,
  };
}

/** A driven pane banner's button. */
export type DriveBannerAction = "take-over" | "resume" | "release";

export interface DriveBannerButton {
  action: DriveBannerAction;
  label: string;
  disabled: boolean;
  /** Tooltip; set only to explain a disabled button. */
  title: string | null;
}

/** Everything the driven-pane banner renders, decided without React. */
export interface DriveBannerView {
  /** Row 1: who drives the pane and what that means for typing here. */
  text: string;
  /** The driver escalated a prompt to the human (waiting colours). */
  escalated: boolean;
  /** The driver is deciding a pending prompt (waiting colours, row 2 shown). */
  deciding: boolean;
  buttons: DriveBannerButton[];
  /** Row 2: `{lead} <code>{summary}</code>{more}`; null when nothing is pending. */
  pendingLine: { lead: string; summary: string; more: string } | null;
  /** `Took over from …` when this drive replaced another driver's. */
  note: string | null;
}

/** How long a pending prompt's summary may run in the banner before it is cut. */
export const BANNER_SUMMARY_MAX = 80;

/** `s` cut to `max` characters (not UTF-16 units) with `…` when it was longer. */
function cutChars(s: string, max: number): string {
  const chars = Array.from(s);
  return chars.length > max ? `${chars.slice(0, max).join("")}…` : s;
}

/** The driven-pane banner's content for `d`. */
export function driveBannerView(d: PaneDrive): DriveBannerView {
  const { driver } = d;
  let text: string;
  if (d.escalatedReason) text = `${driver} escalated: ${d.escalatedReason}`;
  else if (d.paused) {
    text = `Driven by ${driver} (paused) — prompts come to you, and ${driver} can't send until resumed.`;
  } else text = `Driven by ${driver} — what you type here mixes with ${driver}'s prompts.`;

  const none = d.pendingCount === 0;
  const buttons: DriveBannerButton[] = [
    {
      action: "take-over",
      label: "Take over",
      disabled: none,
      title: none ? "No pending prompts" : null,
    },
  ];
  if (d.paused) buttons.push({ action: "resume", label: "Resume", disabled: false, title: null });
  buttons.push({ action: "release", label: "Release", disabled: false, title: null });

  const first = d.firstPending;
  const pendingLine = first
    ? {
        lead: `${driver} is deciding: ${first.toolName}`,
        summary: cutChars(first.summary, BANNER_SUMMARY_MAX),
        more: d.pendingCount > 1 ? ` (+${d.pendingCount - 1} more)` : "",
      }
    : null;

  return {
    text,
    escalated: !!d.escalatedReason,
    deciding: pendingLine !== null,
    buttons,
    pendingLine,
    note: d.forcedFrom ? `Took over from ${d.forcedFrom}.` : null,
  };
}
