// Stacking layers for overlays, as Tailwind classes over the --z-* scale in App.css.
// Each class is spelled out in full so Tailwind's source scan generates it.

/** Dialogs, drawers and the shortcut cheatsheet. */
export const LAYER_MODAL = "z-[var(--z-modal)]";
/** Toasts: above dialogs, so errors raised by dialog actions stay visible. */
export const LAYER_TOAST = "z-[var(--z-toast)]";
/** The quit confirmation, always topmost. */
export const LAYER_QUIT = "z-[var(--z-quit)]";

/** The dimmed backdrop behind a dialog. */
export const MODAL_OVERLAY_CLS = "fixed inset-0 bg-black/55 z-[var(--z-modal)]";
/** The dimmed backdrop behind the quit confirmation. */
export const QUIT_OVERLAY_CLS = "fixed inset-0 bg-black/55 z-[var(--z-quit)]";
