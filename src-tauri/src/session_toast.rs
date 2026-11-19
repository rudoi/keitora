//! Clickable OS notifications for a worktree session that needs input.
//!
//! `tauri-plugin-notification` drops click events on desktop, so a toast it shows
//! does nothing when clicked. On Windows we show the toast through
//! `tauri-winrt-notification` directly and handle its `Activated` event in-process:
//! bring the Keitora window to the front and emit [`TOAST_ACTIVATED_EVENT`] with the
//! originating project + worktree, which the frontend turns into a project switch +
//! lane focus. Other platforms keep the plugin path (no click target there yet).
//!
//! Both notification sources go through [`show_session_toast`]: the background-project
//! watcher (`bg_notifier`) and the active project's `useAttention` hook (via the
//! `notify_session` command).

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Emitted when the user clicks a session toast; the payload is a [`ToastTarget`].
pub const TOAST_ACTIVATED_EVENT: &str = "notification://activated";

/// Label of the app's single window (the default Tauri gives an unlabelled config window).
const MAIN_WINDOW: &str = "main";

/// Where a clicked toast should take the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToastTarget {
    /// The project's resolved `[project].name` — what the frontend keys projects by.
    pub project: String,
    pub worktree: String,
}

/// Show a notification for `target`'s session; clicking it focuses Keitora on that
/// worktree where the platform reports clicks (Windows).
pub fn show_session_toast<R: Runtime>(
    app: &AppHandle<R>,
    title: String,
    body: String,
    target: ToastTarget,
) {
    #[cfg(windows)]
    {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Err(e) = show_windows_toast(&app, &title, &body, target) {
                log::warn!("session toast failed: {e}");
            }
        });
    }
    #[cfg(not(windows))]
    {
        use tauri_plugin_notification::NotificationExt;
        let _ = target;
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

#[cfg(windows)]
fn show_windows_toast<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    body: &str,
    target: ToastTarget,
) -> Result<(), String> {
    use tauri_winrt_notification::Toast;

    let exe = tauri::utils::platform::current_exe().map_err(|e| e.to_string())?;
    let app_id = toast_app_id(exe.parent(), &app.config().identifier);
    let on_click = app.clone();
    Toast::new(app_id)
        .title(title)
        .text1(body)
        .on_activated(move |_action| {
            focus_target(&on_click, &target);
            Ok(())
        })
        .show()
        .map_err(|e| e.to_string())
}

/// Bring the main window to the front and tell the frontend which lane to focus.
#[cfg_attr(not(windows), allow(dead_code))]
fn focus_target<R: Runtime>(app: &AppHandle<R>, target: &ToastTarget) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    let _ = app.emit(TOAST_ACTIVATED_EVENT, target);
}

/// The AppUserModelID to post the toast under. Only an installed app has its
/// identifier registered with the shell (via its Start-menu shortcut); a binary run
/// straight out of `target/debug` or `target/release` borrows PowerShell's, as
/// `tauri-plugin-notification` does — an unregistered ID shows no toast at all.
#[cfg_attr(not(windows), allow(dead_code))]
fn toast_app_id<'a>(exe_dir: Option<&std::path::Path>, identifier: &'a str) -> &'a str {
    const POWERSHELL_APP_ID: &str =
        "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";
    let in_cargo_target = exe_dir.is_some_and(|dir| {
        let profile = dir.file_name().and_then(|n| n.to_str());
        let parent = dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        parent == Some("target") && matches!(profile, Some("debug" | "release"))
    });
    if in_cargo_target {
        POWERSHELL_APP_ID
    } else {
        identifier
    }
}

/// Show a session toast for one of the active project's worktrees (the frontend's
/// `useAttention` path), so its clicks focus the lane like the background ones do.
#[tauri::command]
pub fn notify_session(
    app: AppHandle,
    project: String,
    worktree: String,
    title: String,
    body: String,
) {
    show_session_toast(&app, title, body, ToastTarget { project, worktree });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cargo_target_builds_borrow_the_powershell_app_id() {
        for dir in ["D:/repos/proj/target/debug", "D:/repos/proj/target/release"] {
            let id = toast_app_id(Some(Path::new(dir)), "com.example.app");
            assert!(id.ends_with("powershell.exe"), "{dir} → {id}");
        }
    }

    #[test]
    fn installed_builds_use_the_bundle_identifier() {
        for dir in [
            "C:/Users/x/AppData/Local/Keitora",
            "D:/repos/proj/target/debug/bundle",
            "D:/scratch/debug",
        ] {
            assert_eq!(
                toast_app_id(Some(Path::new(dir)), "com.example.app"),
                "com.example.app",
                "{dir}"
            );
        }
        assert_eq!(toast_app_id(None, "com.example.app"), "com.example.app");
    }

    #[test]
    fn toast_target_serializes_as_the_frontend_payload() {
        let target = ToastTarget {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(
            serde_json::to_value(&target).unwrap(),
            serde_json::json!({ "project": "proj", "worktree": "alpha" })
        );
    }
}
