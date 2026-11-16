// One strip under the title chrome announcing an available update. Renders nothing
// when there is no update or the user dismissed this version.

import { openUrl } from "@tauri-apps/plugin-opener";
import { Loader2 } from "lucide-react";
import { bannerOf, errorText } from "../lib/updates";
import { useToast } from "./Toast";
import { useUpdates } from "./UpdatesProvider";

export function UpdateBanner() {
  const { state, install, dismiss } = useUpdates();
  const { showToast } = useToast();
  const banner = bannerOf(state);
  if (!banner) return null;

  const openNotes = () => {
    void openUrl(banner.releaseUrl).catch((err: unknown) => {
      showToast(`Could not open release notes: ${errorText(err)}`);
    });
  };

  return (
    <div
      className="update-banner"
      data-tone={banner.tone}
      role={banner.tone === "error" ? "alert" : "status"}
      aria-live="polite"
    >
      <span className="update-banner-text">{banner.text}</span>
      <button type="button" className="update-banner-btn" onClick={openNotes}>
        Release notes
      </button>
      <button
        type="button"
        className="update-banner-btn update-banner-btn-primary"
        disabled={banner.busy}
        onClick={install}
      >
        {banner.busy && <Loader2 size={11} className="animate-spin flex-none" />}
        {banner.buttonLabel}
      </button>
      {banner.canDismiss && (
        <button type="button" className="update-banner-btn" onClick={dismiss}>
          Dismiss
        </button>
      )}
    </div>
  );
}
