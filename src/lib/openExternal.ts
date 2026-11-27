// Opening links and files in the user's default app goes through the backend rather than the
// opener plugin's `openUrl`/`openPath`: on Linux the backend strips the AppImage's environment
// first, so a browser or editor started from Keitora doesn't load the image's libraries.

import { invoke } from "@tauri-apps/api/core";

/** Open a web or mail link (http, https, mailto, tel) in the default app. */
export function openUrl(url: string): Promise<void> {
  return invoke("open_external_url", { url });
}

/** Open the herdr daemon's log in the default app. */
export function openDaemonLog(): Promise<void> {
  return invoke("open_daemon_log");
}
