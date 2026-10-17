# Rail bottom dock — unify daemon status + footer with a clean top edge

## Goal

The left rail's scrolling projects/worktrees list currently runs right up to the herdr
daemon panel with no boundary treatment — no divider, no fade. Worktree cards appear to
collide with the pinned bottom area as they scroll. Fix this by grouping the daemon
status and the Settings/shortcuts footer into a single **pinned bottom dock** that reads
as a distinct, anchored region with a clear top edge separating it from the scroll list.

## Scope

**In:**

- A new dock container wrapping the daemon status panel + the Settings/shortcuts footer.
- A single top edge (border) and subtle recessed background on the dock so the list has a
  clean break where it ends.
- Full-bleed to the rail's left/right edges (counteracting the rail's 10px padding), so the
  dock reads as a dock and not an inset card.
- Keep all existing behavior: daemon polling, Connected / Offline / NeverStarted states,
  the Restart button, Settings and shortcuts buttons.

**Out:**

- No scroll-driven fade/shadow (that was the alternative "Scroll fade" path; not chosen).
- No change to daemon stats content, polling cadence, or the metric slots.
- No change to the scroll list internals (project groups, lanes, sticky headers).
- No new dependencies.

## Current structure

`src/components/Rail.tsx` (`<nav className="rail">`, lines ~646–728), styled in
`src/App.css`:

```
.rail  (flex column, padding:10px, bg:--bg-1, border-right)
  .rail-label-row          "Projects" + add button
  .rail-lanes              scroll body (flex:1 1 auto; overflow-y:auto; min-height:0)
  <DaemonStatusBadge/>     .rail-daemon-badge-wrap (flex:0 0 auto; padding:0 6px; mb:2px)
  .rail-footer             border-top; Settings · shortcuts (flex:0 0 auto)
```

Key facts (verified):

- `.rail-lanes` is the only scrolling region; the daemon panel and footer are siblings
  below it in flex flow, so content never overlaps them — it simply ends at the scroll
  body's bottom edge with no visual marker.
- `.rail-footer` already has `border-top: 1px solid var(--line)` (App.css ~200–208). The
  daemon panel above it has **no** top border — this is the missing break.
- `.rail-daemon-card` (Connected state) is a bordered card: `background: var(--bg-3)`,
  `border: 1px solid var(--line)`, `border-radius: 8px` (App.css ~1140–1156).
- Theme tokens (App.css ~3–10 dark, ~35–42 light): rail is `--bg-1`; `--bg-0` is the
  recessed/darker surface in both themes; `--bg-3` is the raised card surface.
- `DaemonStatusBadge` returns `null` for NeverStarted, `DaemonCard` for Connected, and
  `OfflineCard` for Disconnected (Rail.tsx ~123–148).

## Architecture / technical design

**New wrapper, `.rail-dock`.** Introduce a single `<div className="rail-dock">` in
`Rail.tsx` that wraps `<DaemonStatusBadge/>` and the existing `.rail-footer`:

```tsx
<div className="rail-dock">
  <DaemonStatusBadge daemonInfo={daemonInfo} />
  <div className="rail-footer"> … Settings · shortcuts … </div>
</div>
```

**Dock CSS** (`.rail-dock`, new rule in App.css near the rail layout block ~195–227):

- `flex: 0 0 auto` — stays pinned; the scroll body keeps taking the remaining space.
- Full-bleed: `margin: 8px -10px 0` + inner `padding: 8px 10px 0` to cancel the rail's
  10px side padding, so the top edge spans the whole rail width.
- Top edge: `border-top: 1px solid var(--line)` — the clean break the list needs.
- Recessed surface: `background: var(--bg-0)` — subtly darker than the rail (`--bg-1`) in
  both themes, so the dock reads as an anchored strip rather than part of the list.

**Footer adjustment.** With the dock owning the top border, remove `border-top` and
`margin-top` from `.rail-footer` (it becomes an inner row of the dock, divider handled by
the dock edge).

**Daemon panel inside the dock — recommended treatment (flatten).** With a tinted dock
behind it, the bordered `--bg-3` card now reads as a card-inside-a-strip, which looks
busy. Recommendation: **flatten** the Connected card inside the dock — drop its own
border/background/radius so the daemon info sits directly on the dock surface, matching
the flat Settings/shortcuts row. The metric slots, tabular-nums, uptime and session
alignment all stay. (See Open Questions for the alternative.)

- This is a small override on `.rail-daemon-card` (or a `.rail-dock .rail-daemon-card`
  scope): `background: transparent; border: none; border-radius: 0; padding: 6px 2px`.
- `.rail-daemon-badge-wrap` padding is reconciled with the dock's own padding so the
  daemon row and the Settings/shortcuts row share the same left inset.

**Offline / NeverStarted.** The Offline card and the null (NeverStarted) case live in the
same dock. When NeverStarted renders nothing, the dock still shows the footer under its
top edge — still a clean break. Verify the Offline Restart button spacing inside the dock.

## Files touched

- `src/components/Rail.tsx` — wrap the daemon badge + footer in `.rail-dock` (small JSX
  change; ~3 lines).
- `src/App.css` — add `.rail-dock`; adjust `.rail-footer`; flatten `.rail-daemon-card`
  within the dock; reconcile `.rail-daemon-badge-wrap` padding; light-theme spot-check.

Estimated ~2 files, CSS-dominant.

## Dependencies

None. No new packages, no backend/Rust changes, no proto changes.

## Risks

- **Light theme.** `--bg-0` is only slightly darker than `--bg-1` in light mode; the dock
  separation could read faint. Mitigation: the `border-top` carries the break regardless;
  verify visually and bump to a stronger token or a hairline shadow only if needed.
- **Three daemon states.** The dock must look right in Connected, Offline, and
  NeverStarted. Smoke-test all three (NeverStarted = footer-only dock).
- **Full-bleed math.** The `-10px` margins must exactly cancel the rail padding or the top
  edge won't reach the rail borders. Low risk, easy to eyeball.

## Open questions

1. **Daemon card: flatten vs keep.** Proposal recommends flattening the Connected card
   into the dock surface for a clean single-strip look. Alternative: keep the bordered
   `--bg-3` card floating on the dock (more "widget"-like). Decide in Refine with a
   screenshot.
2. **Dock background strength.** `--bg-0` (subtle) vs a slightly stronger tint or a top
   inset shadow for more depth. Default to subtle; revisit after seeing it.

## Test plan

- `yarn build` / `tsc --noEmit` clean.
- Run the app and screenshot the rail in all three daemon states (Connected, Offline,
  NeverStarted) with a long worktree list scrolled to the bottom — confirm the clean break
  and that nothing overlaps.
- Dark + light theme visual check.
