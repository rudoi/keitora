//! Tauri commands for the updater, plus the macOS app-menu entry.

use tauri::{AppHandle, Manager, State};

use super::{install, StatusSnapshot, UpdateStatus, Updater};

/// Current status, phase and progress; seeds the UI on mount and after a webview reload.
#[tauri::command(async)]
pub fn update_status(updater: State<'_, Updater>) -> StatusSnapshot {
    updater.snapshot()
}

/// A manual check. Always emits `updater://status` with `manual: true`.
#[tauri::command(async)]
pub async fn update_check(app: AppHandle) -> Result<UpdateStatus, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<Updater>().check(true))
        .await
        .map_err(|e| format!("update check task failed: {e}"))
}

/// Download and verify the available update; progress arrives on `updater://progress`.
#[tauri::command(async)]
pub async fn update_download(app: AppHandle) -> Result<(), String> {
    install::download(app).await
}

/// Install the downloaded update and relaunch. Returns only on error.
#[tauri::command(async)]
pub async fn update_apply(app: AppHandle) -> Result<(), String> {
    install::apply(app).await
}

#[cfg(target_os = "macos")]
const CHECK_MENU_ID: &str = "check_for_updates";

/// Replace the default menu with one whose app submenu has "Check for Updates…" right
/// after "About".
#[cfg(target_os = "macos")]
pub fn install_app_menu(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, MenuItemKind};

    let menu = Menu::default(app)?;
    if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.first() {
        let item = MenuItem::with_id(app, CHECK_MENU_ID, "Check for Updates…", true, None::<&str>)?;
        app_menu.insert(&item, 1)?;
    }
    app.set_menu(menu)?;
    Ok(())
}

/// Menu events arrive on the main thread; the check itself runs off it.
#[cfg(target_os = "macos")]
pub fn on_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    if event.id() == CHECK_MENU_ID {
        let app = app.clone();
        std::thread::spawn(move || {
            app.state::<Updater>().check(true);
        });
    }
}
