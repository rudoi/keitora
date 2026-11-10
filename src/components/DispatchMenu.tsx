// Per-card "Dispatch to…" menu — the keyboard-first parity path for drag-to-
// dispatch (proposal §4.3). A Radix DropdownMenu opened by a caret on the card
// header (hover/focus-visible) or by `D` when the card is selected (the parent
// drives `open`). Order: Dispatch (auto-pick) first, so `D` then Enter dispatches;
// then the pool worktrees (→ send now, accent; → pending, dim) — worktrees outside
// the dispatch pool never appear; then Release, and Move to backlog / Reopen.

import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { ChevronDown } from "lucide-react";
import type { CardState } from "../lib/cardState";
import type { SessionState } from "../lib/sessions";
import { STATE_ORDER, sessionDotCls } from "../lib/taskStyles";
import type { TaskWithDeps } from "../lib/tasks";
import { judgeLane, type LaneContext } from "../lib/useDispatchTargets";

export function DispatchMenu({
  card,
  state,
  worktreeNames,
  pool,
  laneCtx,
  open,
  onOpenChange,
  onDispatch,
  onAutoDispatch,
  onRelease,
  onBacklog,
  onReopen,
}: {
  card: TaskWithDeps;
  state: CardState;
  worktreeNames: string[];
  pool: Set<string>;
  laneCtx: LaneContext;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDispatch: (worktree: string) => void;
  onAutoDispatch: () => void;
  onRelease: () => void;
  onBacklog: () => void;
  onReopen: () => void;
}) {
  const backlog = state.kind === "backlog";
  const blocked = card.blocked_by.length > 0;
  const dispatchable = !backlog && !blocked && (state.kind === "open" || state.kind === "pending");

  // Candidate worktrees = pool lanes, sorted idle → busy → waiting → offline. The
  // card's own lane stays in (a drop there retries a pending card).
  const targets = dispatchable
    ? worktreeNames
        .filter((wt) => pool.has(wt))
        .map((wt) => ({
          wt,
          state: (laneCtx.sessions.get(wt)?.state ?? "offline") as SessionState,
          judged: judgeLane(card, { key: wt, isWorktree: true, inPool: true }, laneCtx),
        }))
        .filter((t) => t.judged.decision !== "invalid")
        .sort((a, b) => STATE_ORDER[a.state] - STATE_ORDER[b.state])
    : [];

  const itemCls =
    "flex cursor-pointer items-center gap-2 rounded-chip px-2 py-1.5 text-[11.5px] outline-none data-[highlighted]:bg-bg-3 data-[disabled]:cursor-not-allowed data-[disabled]:opacity-45";

  return (
    <DropdownMenu.Root open={open} onOpenChange={onOpenChange}>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          aria-label="Dispatch to…"
          onClick={(e) => e.stopPropagation()}
          onPointerDown={(e) => e.stopPropagation()}
          className="ml-1 flex-none rounded text-tx-3 opacity-0 outline-none transition-opacity hover:text-tx-1 focus-visible:opacity-100 group-hover:opacity-100 data-[state=open]:opacity-100"
        >
          <ChevronDown size={13} />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="z-50 min-w-[200px] rounded-card border border-line bg-bg-2 p-1 shadow-[0_8px_24px_rgba(0,0,0,.45)]"
        >
          <DropdownMenu.Label className="px-2 py-1 text-[10px] uppercase tracking-wide text-tx-3">
            Dispatch to…
          </DropdownMenu.Label>

          {blocked && (
            <div className="px-2 py-1.5 text-[11px] text-urgent">Blocked — resolve deps first</div>
          )}
          {dispatchable && pool.size === 0 && (
            <div className="px-2 py-1.5 text-[11px] text-tx-3">
              No worktree is in the dispatch pool
            </div>
          )}

          {dispatchable && pool.size > 0 && (
            <DropdownMenu.Item
              onSelect={() => onAutoDispatch()}
              className={`${itemCls} font-semibold text-accent-text`}
            >
              <span className="h-1.5 w-1.5 flex-none rounded-full bg-accent" />
              <span>Dispatch</span>
              <span className="ml-auto text-[10px] font-normal text-tx-3">auto-pick</span>
            </DropdownMenu.Item>
          )}

          {targets.map((tgt, i) => {
            const prev = targets[i - 1]?.state;
            // Separator between the action tiers (idle | busy/waiting | offline).
            const tier = (s: SessionState) => (s === "idle" ? 0 : s === "offline" ? 2 : 1);
            const sep = i === 0 || (prev !== undefined && tier(prev) !== tier(tgt.state));
            const now = tgt.judged.decision === "dispatch";
            const here = card.assigned_to === tgt.wt;
            const verb = here ? "→ retry" : now ? "→ send now" : `→ pending (${tgt.state})`;
            return (
              <div key={tgt.wt}>
                {sep && <DropdownMenu.Separator className="my-1 h-px bg-line" />}
                <DropdownMenu.Item
                  onSelect={() => onDispatch(tgt.wt)}
                  className={`${itemCls} ${now ? "text-accent-text" : "text-tx-2"}`}
                >
                  <span
                    className={`h-1.5 w-1.5 flex-none rounded-full ${sessionDotCls(tgt.state)}`}
                  />
                  <span className="font-mono">@{tgt.wt}</span>
                  <span className="ml-auto text-[10px] text-tx-3">{verb}</span>
                </DropdownMenu.Item>
              </div>
            );
          })}

          {(card.assigned_to || card.status !== "active") && (
            <DropdownMenu.Separator className="my-1 h-px bg-line" />
          )}
          {card.assigned_to && (
            <DropdownMenu.Item onSelect={() => onRelease()} className={`${itemCls} text-tx-2`}>
              <span className="h-1.5 w-1.5 flex-none rounded-full bg-tx-3" />
              <span>Release to Open</span>
            </DropdownMenu.Item>
          )}
          {backlog ? (
            <DropdownMenu.Item onSelect={() => onReopen()} className={`${itemCls} text-tx-2`}>
              <span className="h-1.5 w-1.5 flex-none rounded-full bg-tx-3" />
              <span>Reopen from backlog</span>
              <span className="ml-auto text-[10px] text-tx-3">b</span>
            </DropdownMenu.Item>
          ) : (
            card.status !== "active" && (
              <DropdownMenu.Item onSelect={() => onBacklog()} className={`${itemCls} text-tx-2`}>
                <span className="h-1.5 w-1.5 flex-none rounded-full bg-tx-3" />
                <span>Move to backlog</span>
                <span className="ml-auto text-[10px] text-tx-3">b</span>
              </DropdownMenu.Item>
            )
          )}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
