// TaskCard — one board card. Shows exactly one state (Open, Backlog, Pending with its
// reason, Sent, Active, Review, Blocked, Done), any failed send with its reason, and the
// actions that state allows: Dispatch (auto-pick) on an open card, Retry on a
// pending card whose send failed, Release on an assigned card, Reopen on a backlog
// card, Claim for the focused worktree, Mark done on a card in review (with its branch
// and PR, so the reviewer knows what to look at). The card menu adds Mark done on an
// active card and Reopen on a done one.

import { useDraggable } from "@dnd-kit/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { motion } from "motion/react";
import type { CardState } from "../../lib/cardState";
import { PRIORITY_BAR } from "../../lib/taskStyles";
import { claimTask, type TaskWithDeps } from "../../lib/tasks";
import { usePrefersReducedMotion } from "../../lib/usePrefersReducedMotion";
import { DispatchMenu } from "../DispatchMenu";
import type { CardEnv } from "./cardHelpers";

// Tags render on a single (non-wrapping) line; beyond this many we collapse the rest
// into a "+N more" chip so the card grows in width predictably instead of wrapping.
const MAX_VISIBLE_TAGS = 3;

/** Pure: the state chip's text and tooltip for a card. Exported for tests. */
export function stateChip(state: CardState): { text: string; title: string } | null {
  switch (state.kind) {
    case "pending": {
      const why = state.reason.replace(/^pending: /, "");
      return state.failure
        ? { text: "send failed", title: state.failure }
        : { text: "pending", title: why };
    }
    case "sent":
      return { text: "sent ✓", title: `typed into @${state.worktree}; waiting for its claim` };
    case "active":
      return {
        text: "active",
        title: state.worktree ? `claimed by @${state.worktree}` : "claimed",
      };
    case "review":
      return {
        text: "review",
        title: `${state.worktree ? `@${state.worktree}` : "The agent"} finished; mark it done once reviewed`,
      };
    case "backlog":
      return { text: "backlog", title: "hidden from dispatch; reopen it to work on it" };
    default:
      return null;
  }
}

export function TaskCard({
  t,
  column,
  state,
  env,
  compact = false,
  fill = false,
}: {
  t: TaskWithDeps;
  column: string;
  state: CardState;
  env: CardEnv;
  /** Compact variant: tighter padding, smaller text, suited to ~w-[188px] cards in rows. */
  compact?: boolean;
  /** Stretch the card body to fill its parent's height (desktop worktree-row cards). */
  fill?: boolean;
}) {
  const task = t;
  const blocked_by = t.blocked_by;
  const terminal = task.status === "done" || task.status === "cancelled";
  const backlog = state.kind === "backlog";
  const inReview = state.kind === "review";
  // Terminal, backlog and review cards stay put: none of them can be dispatched.
  const fixed = terminal || backlog || inReview;
  const reducedMotion = usePrefersReducedMotion();
  const selected = t.id === env.selectedId;
  const flyIn = env.flyIn?.id === t.id ? env.flyIn.worktree : null;

  // An assigned, not-yet-claimed card on this worktree lane (pending or sent).
  const reserved = (state.kind === "pending" || state.kind === "sent") && state.worktree === column;
  const failed = state.kind === "pending" && state.failure !== null;

  // Whole-card drag on cards that can still be dispatched or released.
  const draggable = useDraggable({
    id: task.id,
    data: { task },
    disabled: fixed,
  });
  const { attributes, listeners, setNodeRef, isDragging } = draggable;
  // The lifted visual is the DragOverlay; the origin slot stays put (opacity-40),
  // so we deliberately do NOT apply `draggable.transform` here.

  // A sent card gets the brighter accent; pending cards stay dashed and dim.
  const emphasized = state.kind === "sent";

  const borderCls = failed
    ? "border-urgent/60"
    : emphasized
      ? "border-accent/60"
      : reserved
        ? "border-dashed border-accent/30"
        : selected
          ? "border-accent/55 shadow-[0_0_0_1px_color-mix(in_srgb,var(--color-accent)_25%,transparent)]"
          : "border-line";

  // Compact variant sizing tokens.
  const padCls = compact ? "px-2 py-2.5" : "px-2.5 py-2.5";
  const railInsetCls = "top-2.5 bottom-2.5";
  const idTextCls = compact ? "text-[10px]" : "text-[11px]";
  const titleTextCls = compact
    ? `mt-1 text-[11px] leading-snug line-clamp-2 ${reserved || backlog ? "text-tx-2" : "text-tx-1"}`
    : `mt-1.5 max-w-[200px] text-[12.5px] leading-snug ${reserved || backlog ? "text-tx-2" : "text-tx-1"}`;
  const badgeTextCls = compact ? "text-[9px]" : "text-[10px]";
  const tagTextCls = compact ? "text-[9px]" : "text-[10px]";
  const actionMtCls = compact ? "mt-1.5" : "mt-2.5";
  const chip = stateChip(state);
  const pendingNote =
    state.kind === "pending" ? (state.failure ?? state.reason.replace(/^pending: /, "")) : null;
  const canDispatch =
    state.kind === "open" && task.status === "open" && blocked_by.length === 0 && env.pool.size > 0;

  const stop = {
    onPointerDown: (e: React.PointerEvent) => e.stopPropagation(),
  };

  const cardBody = (
    <div
      ref={setNodeRef}
      onClick={() => env.onSelect(t.id)}
      {...(fixed ? {} : listeners)}
      {...attributes}
      data-card-state={state.kind}
      className={`group relative rounded-chip border bg-bg-2 ${padCls} ${fill ? "flex h-full flex-col" : ""} ${
        terminal ? "cursor-default" : "cursor-pointer"
      } ${borderCls} ${terminal || backlog ? "opacity-60" : ""} ${isDragging ? "opacity-40" : ""}`}
    >
      {/* Left rail: dashed accent for reserved, solid priority bar otherwise. */}
      {reserved ? (
        <span
          className={`absolute left-0 ${railInsetCls} w-0 border-l-2 border-dashed ${
            emphasized ? "border-accent/70" : "border-accent/50"
          }`}
        />
      ) : (
        <span
          className={`absolute left-0 ${railInsetCls} w-[3px] rounded ${PRIORITY_BAR[task.priority]}`}
        />
      )}

      <div className="flex items-center gap-2 pl-2">
        <span className={`font-mono ${idTextCls} text-tx-3`}>#{task.id}</span>
        {/* Reserved cards preserve priority as a small dot next to the id. */}
        {reserved && (
          <span
            className={`h-1.5 w-1.5 flex-none rounded-full ${PRIORITY_BAR[task.priority]}`}
            title={task.priority}
          />
        )}

        <DispatchMenu
          card={task}
          state={state}
          worktreeNames={env.worktreeNames}
          pool={env.pool}
          laneCtx={env.laneCtx}
          open={env.menuTaskId === t.id}
          onOpenChange={(open) => env.onMenuOpenChange(t.id, open)}
          onDispatch={(wt) => env.onDispatch(wt, t.id)}
          onAutoDispatch={() => env.onAutoDispatch(t.id)}
          onRelease={() => env.onRelease(t.id)}
          onBacklog={() => env.onBacklog(t.id)}
          onReopen={() => env.onReopen(t.id)}
          onMarkDone={() => env.onMarkDone(t.id)}
        />

        {/* Right-side badge slot: one state chip. */}
        {chip && (
          <span
            title={chip.title}
            className={`ml-auto whitespace-nowrap rounded-full border px-2 py-px ${badgeTextCls} ${
              failed
                ? "border-urgent/40 bg-urgent/10 text-urgent"
                : backlog
                  ? "border-line bg-bg-3 text-tx-3"
                  : "border-accent/40 bg-accent-dim text-accent-text"
            }`}
          >
            {chip.text}
          </span>
        )}
        {terminal && <span className="ml-auto text-idle">✓</span>}

        {/* Blocked indicator: top-right of the header, in line with the #id. */}
        {blocked_by.length > 0 && (
          <span
            className={`ml-auto whitespace-nowrap rounded-full bg-urgent/10 px-2 py-px ${badgeTextCls} text-urgent`}
          >
            blocked by {blocked_by.map((b) => `#${b}`).join(", ")}
          </span>
        )}
      </div>

      <div className={`pl-2 ${titleTextCls}`}>{task.title}</div>

      {/* Why a pending card waits, or why its send failed. */}
      {pendingNote && (
        <div
          title={pendingNote}
          className={`${compact ? "mt-1" : "mt-1.5"} truncate pl-2 text-[10px] ${
            failed ? "text-urgent" : "text-tx-3"
          }`}
        >
          {pendingNote}
        </div>
      )}

      {/* Inline dispatch fly-in line (until the worktree flips it to active). */}
      {flyIn && task.status === "open" && (
        <div className={`${compact ? "mt-1" : "mt-1.5"} pl-2 text-[10.5px] text-accent-text`}>
          sent → @{flyIn}
        </div>
      )}

      {task.tags.length > 0 && (
        <div
          className={`${fill ? "mt-auto pt-1.5" : compact ? "mt-1.5" : "mt-2"} flex flex-nowrap items-center gap-1.5 pl-2`}
        >
          {task.tags.slice(0, MAX_VISIBLE_TAGS).map((tag) => (
            <span
              key={tag}
              className={`whitespace-nowrap rounded-full border border-line bg-bg-3 px-2 py-px ${tagTextCls} text-tx-2`}
            >
              {tag}
            </span>
          ))}
          {task.tags.length > MAX_VISIBLE_TAGS && (
            <span
              className={`whitespace-nowrap rounded-full border border-line bg-bg-3 px-2 py-px ${tagTextCls} text-tx-3`}
            >
              +{task.tags.length - MAX_VISIBLE_TAGS} more
            </span>
          )}
        </div>
      )}
      {/* What a card in review is about: its branch and PR. */}
      {inReview && (task.branch || task.pr_url) && (
        <div
          className={`${compact ? "mt-1" : "mt-1.5"} flex min-w-0 items-center gap-2 pl-2 text-[10px] text-tx-3`}
        >
          {task.branch && (
            <span className="truncate font-mono" title={task.branch}>
              {task.branch}
            </span>
          )}
          {task.pr_url && (
            <button
              type="button"
              title={task.pr_url}
              onClick={(e) => {
                e.stopPropagation();
                // biome-ignore lint/style/noNonNullAssertion: guarded by the pr_url check above
                env.onAction(() => openUrl(task.pr_url!));
              }}
              {...stop}
              className="flex-none text-accent-text underline-offset-2 hover:underline"
            >
              PR ↗
            </button>
          )}
        </div>
      )}
      {/* A card in review offers Mark done, the human's step. Reserved cards hide Claim
          (a human must not self-claim an agent's reserved task); a failed one offers
          Retry, every one a subtle hover Release. */}
      {inReview ? (
        env.actor !== null && (
          <div className={`${actionMtCls} pl-2`}>
            <button
              title="Reviewed: move it to Done"
              onClick={(e) => {
                e.stopPropagation();
                env.onMarkDone(task.id);
              }}
              {...stop}
              className="rounded-chip border border-accent bg-accent px-2.5 py-1 text-[11px] font-semibold text-[var(--color-on-accent)]"
            >
              Mark done
            </button>
          </div>
        )
      ) : reserved ? (
        <div
          className={`${actionMtCls} flex gap-1.5 pl-2 ${
            failed
              ? ""
              : "opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100"
          }`}
        >
          {failed && (
            <button
              onClick={(e) => {
                e.stopPropagation();
                env.onDispatch(column, t.id);
              }}
              {...stop}
              className="rounded-chip border border-accent bg-accent px-2.5 py-1 text-[11px] font-semibold text-[var(--color-on-accent)]"
            >
              Retry
            </button>
          )}
          <button
            onClick={(e) => {
              e.stopPropagation();
              env.onRelease(t.id);
            }}
            {...stop}
            className="rounded-chip border border-line bg-bg-3 px-2.5 py-1 text-[11px] text-tx-1"
          >
            Release
          </button>
        </div>
      ) : backlog ? (
        <div className={`${actionMtCls} pl-2`}>
          <button
            onClick={(e) => {
              e.stopPropagation();
              env.onReopen(t.id);
            }}
            {...stop}
            className="rounded-chip border border-line bg-bg-3 px-2.5 py-1 text-[11px] text-tx-1"
          >
            Reopen
          </button>
        </div>
      ) : (
        task.status === "open" &&
        blocked_by.length === 0 && (
          <div className={`${actionMtCls} flex gap-1.5 pl-2`}>
            {canDispatch && (
              <button
                title="Send to a worktree from the dispatch pool (an idle one first)"
                onClick={(e) => {
                  e.stopPropagation();
                  env.onAutoDispatch(t.id);
                }}
                {...stop}
                className="rounded-chip border border-accent bg-accent px-2.5 py-1 text-[11px] font-semibold text-[var(--color-on-accent)]"
              >
                Dispatch
              </button>
            )}
            {env.actor !== null && (
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  // biome-ignore lint/style/noNonNullAssertion: guarded by the actor check above
                  env.onAction(() => claimTask(task.id, env.actor!));
                }}
                {...stop}
                className="rounded-chip border border-line bg-bg-3 px-2.5 py-1 text-[11px] text-tx-1"
              >
                Claim
              </button>
            )}
          </div>
        )
      )}
    </div>
  );

  // A just-dispatched card mounts with a one-shot opacity fade (transforms-safe);
  // reduced motion = appears in place.
  if (flyIn && !reducedMotion) {
    return (
      <motion.div
        className={fill ? "h-full" : undefined}
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.22 }}
      >
        {cardBody}
      </motion.div>
    );
  }
  return cardBody;
}
