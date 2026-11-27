// A slim strip over the top of a driven pane: who drives it, what the driver is deciding,
// and the human's Take over / Resume / Release actions. An absolutely positioned overlay,
// so it never resizes the terminal (no refit, no pty resize) and never blocks typing.

import { type DriveBannerAction, driveBannerView, type PaneDrive } from "../lib/drive";

export function DriveBanner({
  drive,
  onTakeOver,
  onRelease,
  onResume,
}: {
  drive: PaneDrive;
  onTakeOver?: () => void;
  onRelease?: () => void;
  onResume?: () => void;
}) {
  const view = driveBannerView(drive);
  const handlers: Record<DriveBannerAction, (() => void) | undefined> = {
    "take-over": onTakeOver,
    resume: onResume,
    release: onRelease,
  };
  const cls = [
    "pane-drive-banner",
    view.deciding ? "deciding" : "",
    view.escalated ? "escalated" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={cls} role="status" aria-live="polite">
      <span className="pane-drive-banner-text">{view.text}</span>
      {view.buttons.map((b) => (
        <button
          key={b.action}
          type="button"
          className="pane-drive-btn"
          disabled={b.disabled}
          title={b.title ?? undefined}
          onClick={() => handlers[b.action]?.()}
        >
          {b.label}
        </button>
      ))}
      {view.pendingLine && (
        <div className="pane-drive-banner-pending">
          {view.pendingLine.lead} <code>{view.pendingLine.summary}</code>
          {view.pendingLine.more}
        </div>
      )}
      {view.note && <div className="pane-drive-banner-pending">{view.note}</div>}
    </div>
  );
}
