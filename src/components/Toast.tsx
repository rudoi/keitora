// Radix Toast — one visible at a time. Used for failures (urgent) in the
// dispatcher. A ToastProvider wraps the whole app (mounted in App.tsx).
// useToast() returns a `showToast(msg, opts?)` handle for anywhere in the tree;
// `opts.action` adds one button (and keeps the toast up longer so it can be clicked).

import * as RadixToast from "@radix-ui/react-toast";
import { X } from "lucide-react";
import { createContext, useCallback, useContext, useRef, useState } from "react";
import { LAYER_TOAST } from "../lib/layers";

// ---- Context ----------------------------------------------------------------

export interface ToastAction {
  label: string;
  onClick: () => void;
}

export interface ToastOptions {
  action?: ToastAction;
}

interface ToastCtx {
  showToast: (message: string, opts?: ToastOptions) => void;
}

const Ctx = createContext<ToastCtx>({ showToast: () => {} });

export function useToast(): ToastCtx {
  return useContext(Ctx);
}

// ---- Provider ---------------------------------------------------------------

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [message, setMessage] = useState<string | null>(null);
  const [action, setAction] = useState<ToastAction | null>(null);
  const [open, setOpen] = useState(false);
  // A key that forces Radix to re-mount the toast on rapid successive calls so
  // each new call resets the auto-dismiss timer even if one is already visible.
  const keyRef = useRef(0);
  const [toastKey, setToastKey] = useState(0);

  const showToast = useCallback((msg: string, opts?: ToastOptions) => {
    keyRef.current += 1;
    setToastKey(keyRef.current);
    setMessage(msg);
    setAction(opts?.action ?? null);
    setOpen(true);
  }, []);

  return (
    <Ctx.Provider value={{ showToast }}>
      <RadixToast.Provider swipeDirection="right" duration={5000}>
        {children}

        {message !== null && (
          <RadixToast.Root
            key={toastKey}
            open={open}
            onOpenChange={setOpen}
            duration={action ? 15000 : undefined}
            className="flex items-start gap-3 rounded-card border border-urgent/30 bg-bg-2 px-3.5 py-3 shadow-[0_4px_20px_rgba(0,0,0,.4)] data-[state=closed]:animate-none data-[state=open]:animate-none"
          >
            <RadixToast.Description className="flex-1 text-[12px] leading-snug text-urgent">
              {message}
            </RadixToast.Description>
            {action && (
              <RadixToast.Action asChild altText={action.label}>
                <button
                  type="button"
                  className="pane-btn pane-btn-secondary flex-none"
                  onClick={action.onClick}
                >
                  {action.label}
                </button>
              </RadixToast.Action>
            )}
            <RadixToast.Close asChild>
              <button className="flex-none text-tx-3 hover:text-tx-1" aria-label="Dismiss">
                <X size={13} />
              </button>
            </RadixToast.Close>
          </RadixToast.Root>
        )}

        <RadixToast.Viewport
          className={`fixed bottom-4 right-4 ${LAYER_TOAST} w-[320px] max-w-[90vw]`}
        />
      </RadixToast.Provider>
    </Ctx.Provider>
  );
}
