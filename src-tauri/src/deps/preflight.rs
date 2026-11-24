//! Pure install decisions: where the herdr in use lives, which install tier to try, and
//! which route an upgrade takes.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::{Platform, Release};

/// Where an herdr binary lives, which decides how it may be upgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallLayout {
    /// Keitora's own copy under `<keitora_home>/bin/herdr/`.
    Managed,
    /// herdr's vendor install: `~/.herdr/packages/standalone/` (Windows and macOS) or
    /// `~/.local/bin` (macOS).
    Standalone,
    /// Anything else (Homebrew, a package manager, a hand-picked path). Never replaced
    /// automatically.
    External,
}

/// A comparable form of `p`: forward slashes, no `\\?\` prefix, no trailing slash, and
/// lowercased on Windows (case-insensitive filesystem).
fn norm(p: &Path, platform: Platform) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut s = s.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if platform.is_windows() {
        s.make_ascii_lowercase();
    }
    s
}

/// Whether `path` is strictly inside the directory `root`.
fn is_under(path: &Path, root: &Path, platform: Platform) -> bool {
    let (p, r) = (norm(path, platform), norm(root, platform));
    !r.is_empty() && p.len() > r.len() && p.starts_with(&r) && p.as_bytes()[r.len()] == b'/'
}

/// Classify `path` (the herdr executable in use). All roots are parameters so the
/// decision is pure.
pub fn classify_layout(
    path: &Path,
    keitora_home: &Path,
    home: &Path,
    platform: Platform,
) -> InstallLayout {
    if is_under(path, &keitora_home.join("bin").join("herdr"), platform) {
        return InstallLayout::Managed;
    }
    let standalone = home.join(".herdr").join("packages").join("standalone");
    if is_under(path, &standalone, platform) {
        return InstallLayout::Standalone;
    }
    if !platform.is_windows() && is_under(path, &home.join(".local").join("bin"), platform) {
        return InstallLayout::Standalone;
    }
    InstallLayout::External
}

/// Whether `path` sits under a Homebrew prefix.
pub fn is_homebrew_path(path: &Path) -> bool {
    let p = path.to_string_lossy().replace('\\', "/");
    [
        "/opt/homebrew/",
        "/usr/local/Cellar/",
        "/home/linuxbrew/.linuxbrew/",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
}

/// An install tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// herdr's own install script.
    Script,
    /// sha256-verified download from the release manifest into Keitora's managed dir.
    Direct,
    /// Instructions plus a "Browse for herdr" picker.
    Manual,
}

/// Why no automatic tier applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualReason {
    /// The OS/arch has no herdr build Keitora knows about.
    UnsupportedPlatform,
    /// The manifest could not be reached.
    Offline,
    /// The manifest was reached but could not be read.
    ManifestUnavailable,
    /// No release is protocol-compatible, at least the floor, newer than the installed
    /// one, and published for this platform.
    NoCompatibleRelease,
}

/// What came of fetching the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOutcome {
    Offline,
    /// Reached but unreadable (bad JSON, HTTP error, refused host).
    Unavailable,
    Loaded {
        /// The output of `select_release`.
        selected: Option<Release>,
        /// Whether `selected` is the manifest's top-level latest version.
        selected_is_latest: bool,
    },
}

/// Everything the tier choice depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightInput {
    pub platform: Option<Platform>,
    pub manifest: ManifestOutcome,
    /// A release (not debug) build.
    pub release_build: bool,
    /// `KEITORA_HOME` is set (a dev or test home).
    pub keitora_home_set: bool,
    /// `KEITORA_HERDR_BIN` is set (a dev override).
    pub herdr_bin_env_set: bool,
    /// The script-host probe passed (always true on unix).
    pub script_host_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierChoice {
    Use(Tier),
    Manual(ManualReason),
}

/// Script only in a release build with no dev env overrides, when the script would
/// install exactly the selected release and the script host works; else Direct when a
/// compatible release exists; else Manual with the reason.
pub fn choose_tier(i: &PreflightInput) -> TierChoice {
    if i.platform.is_none() {
        return TierChoice::Manual(ManualReason::UnsupportedPlatform);
    }
    let selected_is_latest = match &i.manifest {
        ManifestOutcome::Offline => return TierChoice::Manual(ManualReason::Offline),
        ManifestOutcome::Unavailable => {
            return TierChoice::Manual(ManualReason::ManifestUnavailable)
        }
        ManifestOutcome::Loaded { selected: None, .. } => {
            return TierChoice::Manual(ManualReason::NoCompatibleRelease)
        }
        ManifestOutcome::Loaded {
            selected: Some(_),
            selected_is_latest,
        } => *selected_is_latest,
    };
    let script = i.release_build
        && !i.keitora_home_set
        && !i.herdr_bin_env_set
        && selected_is_latest
        && i.script_host_ok;
    TierChoice::Use(if script { Tier::Script } else { Tier::Direct })
}

/// How to upgrade the herdr currently in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpgradeRoute {
    /// Download a new managed version side by side and repin.
    Direct,
    /// Re-run the vendor script; fall back to a direct download if it can't be used
    /// or fails.
    ScriptThenDirect,
    /// Leave the user's copy alone: show `instruction`, and offer a managed copy instead.
    External { instruction: String },
}

/// The upgrade route for an herdr at `path` with layout `layout`.
pub fn upgrade_route(layout: InstallLayout, path: &Path) -> UpgradeRoute {
    match layout {
        InstallLayout::Managed => UpgradeRoute::Direct,
        InstallLayout::Standalone => UpgradeRoute::ScriptThenDirect,
        InstallLayout::External => UpgradeRoute::External {
            instruction: if is_homebrew_path(path) {
                "brew upgrade herdr".to_string()
            } else {
                "Upgrade herdr the way you installed it, or install a Keitora-managed copy."
                    .to_string()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn windows_layouts() {
        let w = Platform::WindowsX86_64;
        let kh = p(r"C:\Users\x\.keitora");
        let home = p(r"C:\Users\x");
        let cases = [
            (
                r"C:\Users\x\.keitora\bin\herdr\0.9.6\herdr.exe",
                InstallLayout::Managed,
            ),
            // Case and separators don't matter on Windows; nor does the verbatim prefix.
            (
                r"c:/users/X/.KEITORA/bin/herdr/0.9.6/herdr.exe",
                InstallLayout::Managed,
            ),
            (
                r"\\?\C:\Users\x\.keitora\bin\herdr\0.9.6\herdr.exe",
                InstallLayout::Managed,
            ),
            (
                r"C:\Users\x\.herdr\packages\standalone\releases\0.9.6-x86_64-pc-windows-msvc\herdr.exe",
                InstallLayout::Standalone,
            ),
            (
                r"C:\Users\x\.herdr\packages\standalone\current\herdr.exe",
                InstallLayout::Standalone,
            ),
            // `~/.local/bin` is only a vendor location on macOS.
            (r"C:\Users\x\.local\bin\herdr.exe", InstallLayout::External),
            (r"D:\tools\herdr.exe", InstallLayout::External),
            // Sibling directories with a shared prefix are not "under".
            (
                r"C:\Users\x\.keitora\bin\herdr-old\herdr.exe",
                InstallLayout::External,
            ),
            (r"C:\Users\x\.keitora\bin\herdr", InstallLayout::External),
        ];
        for (path, want) in cases {
            assert_eq!(classify_layout(&p(path), &kh, &home, w), want, "{path}");
        }
    }

    #[test]
    fn macos_layouts() {
        for platform in [Platform::MacosAarch64, Platform::MacosX86_64] {
            let kh = p("/Users/me/.keitora");
            let home = p("/Users/me");
            let cases = [
                (
                    "/Users/me/.keitora/bin/herdr/0.9.6/herdr",
                    InstallLayout::Managed,
                ),
                ("/Users/me/.local/bin/herdr", InstallLayout::Standalone),
                (
                    "/Users/me/.herdr/packages/standalone/releases/0.9.6-aarch64-apple-darwin/herdr",
                    InstallLayout::Standalone,
                ),
                ("/opt/homebrew/bin/herdr", InstallLayout::External),
                (
                    "/usr/local/Cellar/herdr/0.9.6/bin/herdr",
                    InstallLayout::External,
                ),
                ("/usr/local/bin/herdr", InstallLayout::External),
                // Case matters on macOS paths here.
                (
                    "/Users/me/.KEITORA/bin/herdr/0.9.6/herdr",
                    InstallLayout::External,
                ),
            ];
            for (path, want) in cases {
                assert_eq!(
                    classify_layout(&p(path), &kh, &home, platform),
                    want,
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn a_custom_keitora_home_is_honoured() {
        let got = classify_layout(
            &p("/tmp/dev-home/bin/herdr/0.9.6/herdr"),
            &p("/tmp/dev-home"),
            &p("/Users/me"),
            Platform::MacosAarch64,
        );
        assert_eq!(got, InstallLayout::Managed);
    }

    fn release() -> Release {
        Release {
            version: semver::Version::new(0, 9, 6),
            protocol: 22,
            url: "https://github.com/herdrdev/herdr/releases/download/v0.9.6/h.zip".into(),
            sha256: "1".repeat(64),
        }
    }

    fn happy() -> PreflightInput {
        PreflightInput {
            platform: Some(Platform::WindowsX86_64),
            manifest: ManifestOutcome::Loaded {
                selected: Some(release()),
                selected_is_latest: true,
            },
            release_build: true,
            keitora_home_set: false,
            herdr_bin_env_set: false,
            script_host_ok: true,
        }
    }

    #[test]
    fn choose_tier_table() {
        let direct = TierChoice::Use(Tier::Direct);
        let cases: Vec<(&str, PreflightInput, TierChoice)> = vec![
            ("happy path", happy(), TierChoice::Use(Tier::Script)),
            (
                "dev build",
                PreflightInput {
                    release_build: false,
                    ..happy()
                },
                direct.clone(),
            ),
            (
                "KEITORA_HOME set",
                PreflightInput {
                    keitora_home_set: true,
                    ..happy()
                },
                direct.clone(),
            ),
            (
                "KEITORA_HERDR_BIN set",
                PreflightInput {
                    herdr_bin_env_set: true,
                    ..happy()
                },
                direct.clone(),
            ),
            (
                "script host blocked",
                PreflightInput {
                    script_host_ok: false,
                    ..happy()
                },
                direct.clone(),
            ),
            (
                "selected is not latest",
                PreflightInput {
                    manifest: ManifestOutcome::Loaded {
                        selected: Some(release()),
                        selected_is_latest: false,
                    },
                    ..happy()
                },
                direct.clone(),
            ),
            (
                "offline",
                PreflightInput {
                    manifest: ManifestOutcome::Offline,
                    ..happy()
                },
                TierChoice::Manual(ManualReason::Offline),
            ),
            (
                "manifest unreadable",
                PreflightInput {
                    manifest: ManifestOutcome::Unavailable,
                    ..happy()
                },
                TierChoice::Manual(ManualReason::ManifestUnavailable),
            ),
            (
                "no asset / no compatible release",
                PreflightInput {
                    manifest: ManifestOutcome::Loaded {
                        selected: None,
                        selected_is_latest: false,
                    },
                    ..happy()
                },
                TierChoice::Manual(ManualReason::NoCompatibleRelease),
            ),
            (
                "unsupported platform",
                PreflightInput {
                    platform: None,
                    ..happy()
                },
                TierChoice::Manual(ManualReason::UnsupportedPlatform),
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(choose_tier(&input), want, "{name}");
        }
    }

    #[test]
    fn upgrade_route_per_layout() {
        assert_eq!(
            upgrade_route(
                InstallLayout::Managed,
                &p("/Users/me/.keitora/bin/herdr/0.9.5/herdr")
            ),
            UpgradeRoute::Direct
        );
        assert_eq!(
            upgrade_route(InstallLayout::Standalone, &p("/Users/me/.local/bin/herdr")),
            UpgradeRoute::ScriptThenDirect
        );
        assert_eq!(
            upgrade_route(InstallLayout::External, &p("/opt/homebrew/bin/herdr")),
            UpgradeRoute::External {
                instruction: "brew upgrade herdr".into()
            }
        );
        match upgrade_route(InstallLayout::External, &p(r"D:\tools\herdr.exe")) {
            UpgradeRoute::External { instruction } => {
                assert!(instruction.contains("Keitora-managed copy"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wire_names() {
        assert_eq!(serde_json::to_value(Tier::Script).unwrap(), "script");
        assert_eq!(serde_json::to_value(Tier::Direct).unwrap(), "direct");
        assert_eq!(
            serde_json::to_value(InstallLayout::Standalone).unwrap(),
            "standalone"
        );
        assert_eq!(
            serde_json::to_value(ManualReason::NoCompatibleRelease).unwrap(),
            "no_compatible_release"
        );
        assert_eq!(
            serde_json::to_value(UpgradeRoute::ScriptThenDirect).unwrap(),
            serde_json::json!({"type": "script_then_direct"})
        );
    }
}
