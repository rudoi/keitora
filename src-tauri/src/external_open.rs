//! Open a URL or file in the user's default app.
//!
//! On Linux the opener plugin hands the target to `xdg-open` with Keitora's own environment, so a
//! browser, editor or file manager started from the AppImage would load the image's libraries and
//! theme. There the openers are run here instead, with the image's environment stripped (see
//! [`keitora_tasks::appimage_env`]). Elsewhere this is the opener plugin.

use tauri::AppHandle;

/// Open a local file or folder with its default app.
pub fn open_path(app: &AppHandle, path: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        linux::open(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_path(path, None::<String>)
            .map_err(|e| e.to_string())
    }
}

/// Open a web or mail link in the default app.
pub fn open_url(app: &AppHandle, url: &str) -> Result<(), String> {
    check_url(url)?;
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        linux::open(url)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(url, None::<String>)
            .map_err(|e| e.to_string())
    }
}

/// Open a link from the webview (terminal links, release notes, dependency docs).
#[tauri::command(async)]
pub fn open_external_url(app: AppHandle, url: String) -> Result<(), String> {
    open_url(&app, &url).map_err(|e| format!("Could not open link: {e}"))
}

/// The links the webview may open: the schemes the opener plugin's default scope allows.
fn check_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| format!("{url} is not a link"))?;
    match parsed.scheme() {
        "http" | "https" | "mailto" | "tel" => Ok(()),
        scheme => Err(format!("{scheme}: links are not opened")),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::Stdio;

    /// Try each opener the `open` crate knows (`xdg-open`, `gio`, …) until one starts, as the
    /// plugin does, with the AppImage's environment stripped.
    pub fn open(target: &str) -> Result<(), String> {
        let mut last = None;
        for mut cmd in open::commands(target) {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            keitora_tasks::appimage_env::scrub(&mut cmd);
            match cmd.spawn() {
                Ok(mut child) => {
                    // Reap it, or it stays a zombie until Keitora exits.
                    std::thread::spawn(move || child.wait());
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or_else(|| "no opener found".to_string(), |e| e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_and_mail_links_are_allowed() {
        for url in [
            "https://example.com/a",
            "http://example.com",
            "mailto:user@example.com",
            "tel:123",
        ] {
            assert_eq!(check_url(url), Ok(()), "{url}");
        }
    }

    #[test]
    fn other_schemes_and_non_links_are_refused() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "smb://srv/share",
            "/home/me/file",
            "",
        ] {
            assert!(check_url(url).is_err(), "{url}");
        }
    }
}
