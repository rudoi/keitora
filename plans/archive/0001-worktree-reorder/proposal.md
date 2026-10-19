# 0001 — Reorder worktrees in the rail by drag

A worktree card in the rail's Projects panel moves up or down within its project by mouse drag; the
new order persists in the project's per-machine worktree list and is the one order the rail, the
Board columns and Ctrl+1–9 use.

## Goal

1. The user presses a worktree card, moves the mouse, and releases it in a gap between that
   project's worktree cards; the card moves to that gap.
2. The order persists across restarts and applies to every registered project, active or not.
3. One order per project: the rail's worktree cards, the Board view's worktree columns, and the
   Ctrl+1–9 worktree jumps all follow it.

## Scope

### In

1. Mouse drag of any worktree card, available or unavailable, within its own project group, in any
   open group.
2. Persistence of the order in the registry's `ProjectRef.worktrees` list for the project. A project
   whose list comes from its committed `keitora.toml` gets that list, reordered, in the registry, as
   worktree creation and the worktree picker already do.
3. Order preservation when the worktree picker saves: worktrees already in the list keep their
   order, and newly tracked worktrees follow them in discovery order.

### Out of Scope

1. Keyboard reordering of worktrees: the drag is mouse-only, and a worktree card has no grip control.
2. Moving a worktree from one project to another.
3. A registry write by another process (the `keitora` CLI) between a reorder's load and save:
   `UserConfig::write_lock` serializes writers within the app process only, as for every
   existing registry writer.
4. Worktrees added to a project's committed `keitora.toml` after its list is in the registry: they
   appear once the worktree picker adds them.

## Architecture

1. The stored worktree list's entry order is the display order. `Config::resolved_worktrees_with`
   reads entries in list order. The active project's rail, the Board columns and Ctrl+1–9 read
   `activeWorktrees` in `App.tsx`, the warm project copy or the live `listWorktrees` result;
   `sameWorktrees` compares by index, so a new order refreshes the warm copy. Non-active groups
   read `project_overview`, which resolves through the same function.
2. A reorder writes the same entries in a new order through `UserConfig::set_project_worktrees_by_root`,
   which writes entries in the order given and keeps each entry's `cli`, `codex_thread_id`,
   `claude_session_id` and `diff_base`.
3. The `worktrees://changed` event reloads both the active project's worktree list and the projects
   overview.
4. The rail's existing project-group drag code (dnd-kit `DndContext`, drop line, optimistic order
   override) handles worktree drags in the same `DndContext`.

## Technical Design

### Task store (`crates/keitora-tasks`)

1. `reorder_entries(entries, order)` is a pure function in `config.rs`. It returns the entries
   named in `order`, in that order, followed by every other entry in its original relative order.
   It skips names in `order` with no entry and repeated names, and returns every input entry
   exactly once; entries sharing a name stay together in their original order. `lib.rs`
   re-exports it with the other `config` items.
2. `discovery::set_project_worktrees` keeps the current order for a registered project: under
   `UserConfig::write_lock` it reads the project's current names from its registry list, or from
   `effective_config(pref).worktree_entries()` when the registry has none, and writes
   `reorder_entries(entries, current names)`.
   Worktrees already listed keep their order, new worktrees follow in the given order, and removed
   worktrees leave the list. An unregistered project's committed-file write keeps the given order.
   Its callers are the worktree picker's save (`set_worktrees`), worktree
   creation and path repair; creation and repair pass the current order with new entries at the end.
3. `discovery::reorder_project_worktrees(project, order)` writes the order for the project
   registered as `project`: under `UserConfig::write_lock` it loads the registry, reads the
   project's entries through `effective_config(pref).worktree_entries()`, applies `reorder_entries`,
   and saves with `set_project_worktrees_by_root`. It returns the error
   `No project named '{name}' is registered.`, the existing text from `set_active_project`, for a
   key with no registered project. `lib.rs` re-exports it.

### Host (`src-tauri/src/host.rs`)

1. `reorder_worktrees(project, order)` is a new Tauri command; `project` is the registry key. It
   calls `reorder_project_worktrees`, returns its error text, and emits `worktrees://changed`.
2. The command is registered in the invoke handler beside `reorder_projects`.

### Frontend

1. `reorderWorktrees(project, order)` in `src/lib/ipc.ts` invokes the command. `App.tsx` saves
   through `latestWinsBy(reorderWorktrees)`: saves for one project run one at a time and the
   latest waiting one wins; projects save independently.
2. `Lane` takes a `groupKey` prop and registers its card with `useDraggable`. The draggable's
   data holds the kind, group key and worktree name. The card spreads the pointer listeners only, without the keyboard attributes
   (`role`, `tabIndex`). The whole card, including the diff button, starts the drag after a 4 px
   pointer move; a click selects the worktree or opens the diff, and a right-click opens the
   context menu.
3. `RailProjects` branches on the dragged item's kind in `onDragStart`, `track`, `onDragEnd`,
   `onDragCancel`, the announcements and the `DragOverlay` content.
4. A worktree drag measures the card rects inside that group's `.rail-group-lanes`, computes its
   insertion slot with `insertionIndex` over them, and draws the existing `.rail-drop-line` with
   `lineOffset` in the gap. Slots that leave the card in place show no line (`isNoopSlot`). While
   the group's lanes are absent from the DOM, the drag keeps its last slot and line.
5. The dragged worktree card dims in place; a lifted chip with the worktree's name follows the
   pointer in the `DragOverlay`.
6. On release, `moveToSlot` computes the new order; `RailProjects` shows it at once as a
   per-group override and calls `reorderWorktrees`. `settleOverride` clears the override when the
   reloaded list matches it or its worktree set changes; a failed save clears the override and
   shows the error in a toast.
7. `applyLaneOrders(groups, overrides)` is a pure function in `src/lib/railOrder.ts` that returns
   the groups with each overridden group's lanes in override order, using `applyOrder`. A group
   with no override is returned as the same object.
8. `settleLaneOverrides(overrides, groups)` is a pure function in `src/lib/railOrder.ts` that applies
   `settleOverride` to each group's override against that group's reported lane names and removes
   overrides for groups no longer present. `RailProjects` runs it with `laneNamesByGroup(groups)`
   whenever the groups change and an override is pending.
9. The worktree drag id is `laneDragId(groupKey, name)` in `src/lib/railOrder.ts`: a NUL
   character, `lane`, the group key and the worktree name, separated by NUL characters. The draggable data is `{ kind: "lane", group, worktree }`; a project
   group's draggable data is `{ kind: "group" }`.
10. The lane being dragged gets the class `is-dragging` on its `.lane-row`, which sets its opacity
    to 0.4. The lifted chip is `LaneOverlay` in `Lane.tsx`: a `.rail-lane-overlay` element
    with the worktree name, raised with the same `--bg-3` surface and shadow as
    `.rail-group-overlay`.
11. A worktree drag's pointer outside its group's lanes maps to the first or last slot through
    `insertionIndex`.

### User-facing text (approved)

| Location | Purpose | Text |
|---|---|---|
| Screen-reader announcement, drag start | Worktree picked up | `Picked up {worktree}, position {n} of {total}.` |
| Screen-reader announcement, drag move | Target position | `{worktree} will move to position {n} of {total}.` |
| Screen-reader announcement, drop | Final position | `{worktree} dropped at position {n} of {total}.` |
| Screen-reader announcement, cancel | Escape during drag | `Reorder cancelled. {worktree} is back where it was.` |
| Toast, save failure | Unknown project | `No project named '{name}' is registered.` (existing text) |

## Tests

1. `reorder_entries`: full permutation; partial order; unknown names; repeated names; empty order;
   entries sharing a name; every entry field preserved.
2. `reorder_project_worktrees` (against a temporary config home): writes the new order to the
   registry; keeps `cli`, `diff_base` and session ids; returns an error for an unknown key; copies a
   committed list, reordered, into the registry and leaves the committed file as it was.
3. `set_project_worktrees` order preservation: existing worktrees keep their order; new worktrees
   follow in incoming order; removed worktrees leave the list; creation appends the new worktree
   last.
4. `applyLaneOrders`: overridden group reordered; other groups returned as the same object; lanes
   missing from the override follow in their order.
5. `settleLaneOverrides`: override cleared when the reported order matches; cleared when the
   worktree set changes; kept while the reported order differs; removed for a missing group.
6. `laneDragId`: stable for the same pair; distinct for the same worktree name in two projects;
   distinct when the split between group and worktree differs; starts with a NUL character.
7. `latestWinsBy`: keys run independently; within a key the latest waiting call wins.
8. `laneNamesByGroup`: each group key maps to its worktree names in lane order.
9. `cargo test --workspace` and `yarn test` pass.

## Validation

1. The installed branch build runs against the user's registry; a mouse drag in a non-active open
   group writes the new order to the registry.
2. After restart, the group shows the dragged order.

## Dependencies

1. `@dnd-kit/core`, present in `package.json`.

## Sources

A. `crates/keitora-tasks/src/config.rs:2528-2551`
B. `src/App.tsx:119` (`useLiveResource(listWorktrees, "worktrees://changed")`)
C. `src/components/board/useBoardLanes.ts:32-33`
D. `src/lib/keybindings.ts:319`, `src/lib/railGroupView.ts:66-73`
E. `src-tauri/src/host.rs:212-238`
F. `crates/keitora-tasks/src/config.rs:621-651`, `836-852`
G. `src/lib/projectsOverview.ts:69`
H. `src/components/rail/RailProjects.tsx`
I. `src/lib/railDrop.ts`
J. `src/lib/railOrder.ts`
K. `crates/keitora-tasks/src/config.rs:779-782` (an in-process `Mutex`)
L. `src/App.tsx:138-142` (`activeWorktrees`), `src/lib/warmProjects.ts:36-51` (`sameWorktrees`)
M. `crates/keitora-tasks/src/discovery.rs:98-108` (`set_project_worktrees`), `228-243` (creation appends)
N. `src-tauri/src/host.rs:277` (`No project named '{name}' is registered.`)
