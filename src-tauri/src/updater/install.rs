//! Plugin glue: download, verify and install a candidate through `tauri-plugin-updater`.
//! Every decision is made by the phase helpers on [`Updater`]; this file only moves bytes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};
use url::Url;

use super::release::{self, Candidate};
use super::{is_unsafe_mac_location, Updater};
use crate::host::Quitting;

/// `restore_update` never requests the endpoint, but the plugin's builder insists on a
/// non-empty https list. The `.invalid` TLD can't resolve.
const UNUSED_ENDPOINT: &str = "https://updates.invalid/keitora";
/// Bounds the whole installer download (the plugin only offers a total-request timeout), so a
/// stalled connection can't hang forever. Generous enough for a slow link: a 30 MB installer
/// needs only ~17 KB/s.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub const DEV_BUILD_ERROR: &str = "Updates can't be installed from a development build.";
pub const MAC_LOCATION_ERROR: &str = "Move Keitora to your Applications folder, then try again.";

fn build_updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let endpoint = Url::parse(UNUSED_ENDPOINT).map_err(|e| e.to_string())?;
    app.updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

/// Download and verify the current candidate, leaving it `Ready` in memory.
pub async fn download(app: AppHandle) -> Result<(), String> {
    // Refuse before downloading when the install would be refused anyway.
    location_problem()?;
    let updater = app.state::<Updater>();
    let Some(candidate) = updater.begin_download()? else {
        return Ok(());
    };
    match fetch_verified(&app, &candidate).await {
        Ok((update, bytes)) => {
            log::info!("[updater] downloaded and verified {}", candidate.version);
            updater.finish_download(candidate.version.to_string(), update, Arc::new(bytes));
            Ok(())
        }
        Err(e) => {
            log::warn!("[updater] download of {} failed: {e}", candidate.version);
            updater.fail_download();
            Err(e)
        }
    }
}

async fn fetch_verified(
    app: &AppHandle,
    candidate: &Candidate,
) -> Result<(Update, Vec<u8>), String> {
    // Fetched now, not at check time, so a re-uploaded asset gets its fresh signature.
    let sig_url = candidate.sig_url.clone();
    let handle = app.clone();
    let sig = tauri::async_runtime::spawn_blocking(move || {
        handle.state::<Updater>().fetch_text(&sig_url)
    })
    .await
    .map_err(|e| format!("signature fetch task failed: {e}"))?
    .map_err(|e| format!("couldn't fetch the update signature: {e}"))?;

    let manifest = release::manifest(candidate, &sig);
    let mut update = build_updater(app)?
        .restore_update(manifest)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the release is not newer than this version".to_string())?;
    // `restore_update` leaves the per-update timeout unset (the builder's only covers
    // `check`), so bound the download here.
    update.timeout = Some(DOWNLOAD_TIMEOUT);

    let updater = app.state::<Updater>();
    // `download` verifies the signature and the signed version before returning.
    let bytes = update
        .download(
            |chunk, total| updater.record_progress(chunk, total, Instant::now()),
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok((update, bytes))
}

/// Install the ready package and relaunch. Only returns on error: on Windows the plugin
/// exits the process after starting the installer (which relaunches Keitora); elsewhere the
/// bundle is swapped in place and we restart. herdr is left running either way.
pub async fn apply(app: AppHandle) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err(DEV_BUILD_ERROR.into());
    }
    let updater = app.state::<Updater>();
    let (update, bytes) = updater.begin_apply()?;

    if let Err(e) = location_problem() {
        updater.fail_apply();
        return Err(e);
    }

    // While the phase is Applying the window can't be closed (see the close guard in host.rs),
    // so nothing interrupts the swap.
    let result = tauri::async_runtime::spawn_blocking(move || {
        update.install(bytes.as_slice()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("install task failed: {e}"))
    .and_then(|r| r);

    match result {
        Ok(()) => {
            // Our own exit must not trip the quit-confirm guard.
            app.state::<Quitting>().set(true);
            app.restart()
        }
        Err(e) => {
            log::warn!("[updater] install failed: {e}");
            updater.fail_apply();
            Err(e)
        }
    }
}

/// Why the running copy can't be replaced in place, if it can't: a macOS app running from a
/// mounted dmg or under App Translocation.
fn location_problem() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        if let Ok(exe) = std::env::current_exe() {
            if is_unsafe_mac_location(&exe) {
                return Err(MAC_LOCATION_ERROR.into());
            }
        }
    }
    Ok(())
}
