//! `~/.keitora/` root resolution shared by the app's on-disk state (logs, the herdr
//! home, the global status store) and the CLI.

use std::path::PathBuf;

/// Environment override for the Keitora file root (test isolation). When unset the
/// root is `<home>/.keitora`.
pub const KEITORA_HOME_ENV: &str = "KEITORA_HOME";

/// The Keitora file root directory: `$KEITORA_HOME` if set, else `<home>/.keitora`.
///
/// `<home>` is `$HOME` on unix and `%USERPROFILE%` on Windows — a GUI process
/// launched from Explorer/the Start menu/the installer inherits no `HOME`, so
/// relying on it there would silently resolve the root to `/tmp`.
pub fn keitora_home() -> PathBuf {
    if let Some(over) = std::env::var_os(KEITORA_HOME_ENV) {
        return PathBuf::from(over);
    }
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(unix)]
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".keitora")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shares the crate-wide env lock: other test modules also mutate `KEITORA_*`.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        &crate::TEST_ENV_LOCK
    }

    #[test]
    fn keitora_home_honors_override() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(KEITORA_HOME_ENV);
        std::env::set_var(KEITORA_HOME_ENV, "/tmp/keitora-paths-test");
        assert_eq!(keitora_home(), PathBuf::from("/tmp/keitora-paths-test"));
        match prev {
            Some(v) => std::env::set_var(KEITORA_HOME_ENV, v),
            None => std::env::remove_var(KEITORA_HOME_ENV),
        }
    }

    #[test]
    fn keitora_home_defaults_under_the_user_home() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(KEITORA_HOME_ENV);
        std::env::remove_var(KEITORA_HOME_ENV);
        let got = keitora_home();
        if let Some(v) = prev {
            std::env::set_var(KEITORA_HOME_ENV, v);
        }
        assert!(got.ends_with(".keitora"), "unexpected root: {got:?}");
        #[cfg(windows)]
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            assert_eq!(got, PathBuf::from(profile).join(".keitora"));
        }
        #[cfg(unix)]
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(got, PathBuf::from(home).join(".keitora"));
        }
    }
}
