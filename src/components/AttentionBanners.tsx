// In-app attention banner — Radix Toast, one visible at a time. Used when Keitora is
// the focused app: macOS hides a foreground app's OS notification banners, so we
// surface attention for non-focused worktrees in-app instead. Distinct from the
// urgent dispatcher Toast (Toast.tsx, bottom-right, red) — this is accent-styled,
// top-right, and clickable to jump to the worktree.
//
// An AttentionBannersProvider wraps the app (mounted in App.tsx). useAttentionBanners()
// returns a `showAttention(...)` handle for anywhere in the tree.

import * as RadixToast from "@radix-ui/react-toast";
import { X } from "lucide-react";
import { createContext, useCallback, useContext, useRef, useState } from "react";
import { LAYER_TOAST } from "../lib/layers";

// ---- Context ----------------------------------------------------------------

interface AttentionArgs {
  title: string;
  body: string;
  // Invoked when the banner body is clicked (jump to the worktree).
  onClick: () => void;
}

interface Ctx {
  showAttention: (a: AttentionArgs) => void;
}

const Ctx = createContext<Ctx>({ showAttention: () => {} });

export function useAttentionBanners(): Ctx {
  return useContext(Ctx);
}

// ---- Provider ---------------------------------------------------------------

export function AttentionBannersProvider({ children }: { children: React.ReactNode }) {
  const [banner, setBanner] = useState<AttentionArgs | null>(null);
  const [open, setOpen] = useState(false);
  // Force a Radix re-mount on each call so the auto-dismiss timer resets even when
  // one is already visible (same pattern as Toast.tsx).
  const keyRef = useRef(0);
  const [toastKey, setToastKey] = useState(0);

  const showAttention = useCallback((a: AttentionArgs) => {
    keyRef.current += 1;
    setToastKey(keyRef.current);
    setBanner(a);
    setOpen(true);
  }, []);

  return (
    <Ctx.Provider value={{ showAttention }}>
      <RadixToast.Provider swipeDirection="right" duration={6000}>
        {children}

        {banner !== null && (
          <RadixToast.Root
            key={toastKey}
            open={open}
            onOpenChange={setOpen}
            className="flex items-start gap-2 rounded-card border border-accent/40 bg-bg-2 px-3.5 py-3 shadow-[0_4px_20px_rgba(0,0,0,.4)] data-[state=closed]:animate-none data-[state=open]:animate-none"
          >
            <button
              type="button"
              onClick={() => {
                banner.onClick();
                setOpen(false);
              }}
              className="flex-1 min-w-0 text-left"
            >
              <RadixToast.Title className="truncate text-[12px] font-semibold leading-snug text-tx-1">
                {banner.title}
              </RadixToast.Title>
              <RadixToast.Description className="truncate text-[12px] leading-snug text-tx-2">
                {banner.body}
              </RadixToast.Description>
            </button>
            <RadixToast.Close asChild>
              <button className="flex-none text-tx-3 hover:text-tx-1" aria-label="Dismiss">
                <X size={13} />
              </button>
            </RadixToast.Close>
          </RadixToast.Root>
        )}

        <RadixToast.Viewport
          className={`fixed top-4 right-4 ${LAYER_TOAST} w-[320px] max-w-[90vw]`}
        />
      </RadixToast.Provider>
    </Ctx.Provider>
  );
}
