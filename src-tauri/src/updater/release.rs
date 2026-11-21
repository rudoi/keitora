//! Pure assessment of a GitHub "latest release" response: is it newer than the running
//! build, does it carry a signed installer for this platform, and what manifest do we hand
//! the updater plugin to download it. No I/O here.

use semver::Version;
use serde::Deserialize;

/// Release notes are shown as plain text; `--generate-notes` bodies can be long.
pub const NOTES_LIMIT: usize = 4000;

/// The platforms Keitora publishes in-app updates for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    WindowsX64,
    MacArm64,
    /// Only a copy running as an AppImage: a deb or rpm install is owned by the system
    /// package manager, and the plugin can only replace the file `$APPIMAGE` names.
    LinuxX64AppImage,
}

impl Platform {
    /// The running build's platform, or `None` where no update artifacts are published.
    pub fn current() -> Option<Platform> {
        Self::detect(
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::var_os("APPIMAGE").is_some_and(|p| !p.is_empty()),
        )
    }

    /// [`Platform::current`] from its inputs: the target os and arch, and whether the AppImage
    /// runtime set `$APPIMAGE`.
    pub fn detect(os: &str, arch: &str, appimage: bool) -> Option<Platform> {
        match (os, arch) {
            ("windows", "x86_64") => Some(Platform::WindowsX64),
            ("macos", "aarch64") => Some(Platform::MacArm64),
            ("linux", "x86_64") if appimage => Some(Platform::LinuxX64AppImage),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Platform::WindowsX64 => "Windows x64",
            Platform::MacArm64 => "macOS arm64",
            Platform::LinuxX64AppImage => "Linux x64 AppImage",
        }
    }
}

/// A newer release with a signed installer for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub version: Version,
    pub notes: String,
    pub release_url: String,
    pub published_at: Option<String>,
    pub asset_url: String,
    pub sig_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    UpToDate,
    Available(Candidate),
    Failed(String),
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Parse a release tag (`v1.2.3` or `1.2.3`) as a semver version.
pub fn tag_version(tag: &str) -> Option<Version> {
    let raw = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(raw).ok()
}

/// The updater artifact name the release pipeline publishes for `platform`. The macOS
/// tarball is named after the `.app`, with no version or arch; the AppImage is its own
/// updater artifact.
pub fn installer_name(platform: Platform, version: &Version) -> String {
    match platform {
        Platform::WindowsX64 => format!("Keitora_{version}_x64-setup.exe"),
        Platform::MacArm64 => "Keitora.app.tar.gz".to_string(),
        Platform::LinuxX64AppImage => format!("Keitora_{version}_amd64.AppImage"),
    }
}

/// Decide what a `/releases/latest` JSON body means for the running build. The version
/// comparison runs before any asset checks, so an old or equal release with a broken asset
/// list is still "up to date".
pub fn assess(json: &str, current: &Version, platform: Platform) -> Assessment {
    let Ok(release) = serde_json::from_str::<GhRelease>(json) else {
        return Assessment::Failed("malformed release".into());
    };
    let Some(version) = tag_version(&release.tag_name) else {
        return Assessment::Failed(format!("bad tag {:?}", release.tag_name));
    };
    if version <= *current {
        return Assessment::UpToDate;
    }
    let name = installer_name(platform, &version);
    let Some(asset) = release.assets.iter().find(|a| a.name == name) else {
        return Assessment::Failed(format!("release has no {} installer", platform.label()));
    };
    let sig_name = format!("{name}.sig");
    let Some(sig) = release.assets.iter().find(|a| a.name == sig_name) else {
        return Assessment::Failed("release is not signed".into());
    };
    Assessment::Available(Candidate {
        version,
        notes: truncate_notes(release.body.as_deref().unwrap_or("")),
        release_url: release.html_url,
        published_at: release.published_at,
        asset_url: asset.browser_download_url.clone(),
        sig_url: sig.browser_download_url.clone(),
    })
}

/// Cut `body` to [`NOTES_LIMIT`] chars on a char boundary, appending `…` when cut.
pub fn truncate_notes(body: &str) -> String {
    match body.char_indices().nth(NOTES_LIMIT) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_string(),
    }
}

/// The plugin's "dynamic" release manifest for `candidate`, fed to
/// `Updater::restore_update` so the plugin downloads and verifies the asset.
pub fn manifest(candidate: &Candidate, sig_text: &str) -> serde_json::Value {
    let mut m = serde_json::json!({
        "version": candidate.version.to_string(),
        "notes": candidate.notes,
        "url": candidate.asset_url,
        "signature": sig_text.trim(),
    });
    if let Some(date) = &candidate.published_at {
        m["pub_date"] = serde_json::Value::String(date.clone());
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WIN_URL: &str = "https://example.com/dl/Keitora_0.4.1_x64-setup.exe";
    const MAC_URL: &str = "https://example.com/dl/Keitora.app.tar.gz";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn asset(name: &str) -> serde_json::Value {
        json!({ "name": name, "browser_download_url": format!("https://example.com/dl/{name}") })
    }

    fn release(tag: &str, assets: &[&str]) -> String {
        json!({
            "tag_name": tag,
            "body": "notes here",
            "published_at": "2026-11-01T12:00:00Z",
            "html_url": format!("https://example.com/releases/{tag}"),
            "assets": assets.iter().map(|a| asset(a)).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn full_release(tag: &str, ver: &str) -> String {
        let exe = format!("Keitora_{ver}_x64-setup.exe");
        let exe_sig = format!("{exe}.sig");
        release(
            tag,
            &[
                &exe,
                &exe_sig,
                "Keitora.app.tar.gz",
                "Keitora.app.tar.gz.sig",
                &format!("Keitora_{ver}_aarch64.dmg"),
                &format!("Keitora_{ver}_amd64.AppImage"),
                &format!("Keitora_{ver}_amd64.AppImage.sig"),
                &format!("Keitora_{ver}_amd64.deb"),
                &format!("Keitora-{ver}-1.x86_64.rpm"),
            ],
        )
    }

    #[test]
    fn newer_release_is_available_with_its_assets() {
        let a = assess(
            &full_release("v0.4.1", "0.4.1"),
            &v("0.4.0"),
            Platform::WindowsX64,
        );
        let Assessment::Available(c) = a else {
            panic!("expected available, got {a:?}")
        };
        assert_eq!(c.version, v("0.4.1"));
        assert_eq!(c.notes, "notes here");
        assert_eq!(c.release_url, "https://example.com/releases/v0.4.1");
        assert_eq!(c.published_at.as_deref(), Some("2026-11-01T12:00:00Z"));
        assert_eq!(c.asset_url, WIN_URL);
        assert_eq!(c.sig_url, format!("{WIN_URL}.sig"));
    }

    #[test]
    fn equal_and_older_releases_are_up_to_date() {
        let json = full_release("v0.4.0", "0.4.0");
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::WindowsX64),
            Assessment::UpToDate
        );
        assert_eq!(
            assess(&json, &v("0.5.0"), Platform::MacArm64),
            Assessment::UpToDate
        );
    }

    #[test]
    fn up_to_date_is_decided_before_asset_checks() {
        let json = release("v0.3.0", &[]);
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::WindowsX64),
            Assessment::UpToDate
        );
    }

    #[test]
    fn tags_parse_with_or_without_v() {
        assert_eq!(tag_version("v1.2.3"), Some(v("1.2.3")));
        assert_eq!(tag_version("1.2.3"), Some(v("1.2.3")));
        assert_eq!(tag_version("vv1.2.3"), None);
        assert_eq!(tag_version("latest"), None);
        let a = assess(
            &full_release("0.4.1", "0.4.1"),
            &v("0.4.0"),
            Platform::WindowsX64,
        );
        assert!(matches!(a, Assessment::Available(_)), "{a:?}");
    }

    #[test]
    fn versions_compare_numerically() {
        let a = assess(
            &full_release("v0.10.0", "0.10.0"),
            &v("0.9.0"),
            Platform::WindowsX64,
        );
        assert!(matches!(a, Assessment::Available(_)), "{a:?}");
        assert_eq!(
            assess(
                &full_release("v0.9.0", "0.9.0"),
                &v("0.10.0"),
                Platform::WindowsX64
            ),
            Assessment::UpToDate
        );
    }

    #[test]
    fn bad_tag_fails() {
        assert_eq!(
            assess(&release("nightly", &[]), &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("bad tag \"nightly\"".into())
        );
    }

    #[test]
    fn malformed_json_fails() {
        assert_eq!(
            assess("{not json", &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("malformed release".into())
        );
        assert_eq!(
            assess(r#"{"assets": []}"#, &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("malformed release".into())
        );
    }

    #[test]
    fn missing_installer_fails() {
        let json = release("v0.4.1", &["Keitora.app.tar.gz", "Keitora.app.tar.gz.sig"]);
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("release has no Windows x64 installer".into())
        );
        let json = release(
            "v0.4.1",
            &[
                "Keitora_0.4.1_x64-setup.exe",
                "Keitora_0.4.1_x64-setup.exe.sig",
            ],
        );
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::MacArm64),
            Assessment::Failed("release has no macOS arm64 installer".into())
        );
    }

    #[test]
    fn installer_for_another_version_counts_as_missing() {
        let json = release(
            "v0.4.1",
            &[
                "Keitora_0.4.0_x64-setup.exe",
                "Keitora_0.4.0_x64-setup.exe.sig",
            ],
        );
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("release has no Windows x64 installer".into())
        );
    }

    #[test]
    fn missing_signature_fails() {
        let json = release(
            "v0.4.1",
            &["Keitora_0.4.1_x64-setup.exe", "Keitora.app.tar.gz"],
        );
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::WindowsX64),
            Assessment::Failed("release is not signed".into())
        );
        assert_eq!(
            assess(&json, &v("0.4.0"), Platform::MacArm64),
            Assessment::Failed("release is not signed".into())
        );
    }

    #[test]
    fn each_platform_has_its_asset_name() {
        assert_eq!(
            installer_name(Platform::WindowsX64, &v("0.4.1")),
            "Keitora_0.4.1_x64-setup.exe"
        );
        assert_eq!(
            installer_name(Platform::MacArm64, &v("0.4.1")),
            "Keitora.app.tar.gz"
        );
        let a = assess(
            &full_release("v0.4.1", "0.4.1"),
            &v("0.4.0"),
            Platform::MacArm64,
        );
        let Assessment::Available(c) = a else {
            panic!("expected available, got {a:?}")
        };
        assert_eq!(c.asset_url, MAC_URL);
        assert_eq!(c.sig_url, format!("{MAC_URL}.sig"));

        assert_eq!(
            installer_name(Platform::LinuxX64AppImage, &v("0.4.1")),
            "Keitora_0.4.1_amd64.AppImage"
        );
        let a = assess(
            &full_release("v0.4.1", "0.4.1"),
            &v("0.4.0"),
            Platform::LinuxX64AppImage,
        );
        let Assessment::Available(c) = a else {
            panic!("expected available, got {a:?}")
        };
        assert_eq!(
            c.asset_url,
            "https://example.com/dl/Keitora_0.4.1_amd64.AppImage"
        );
        assert_eq!(
            c.sig_url,
            "https://example.com/dl/Keitora_0.4.1_amd64.AppImage.sig"
        );
    }

    #[test]
    fn linux_updates_only_when_running_as_an_appimage() {
        assert_eq!(
            Platform::detect("linux", "x86_64", true),
            Some(Platform::LinuxX64AppImage)
        );
        // A deb or rpm install updates through the package manager.
        assert_eq!(Platform::detect("linux", "x86_64", false), None);
        assert_eq!(Platform::detect("linux", "aarch64", true), None);
    }

    #[test]
    fn detect_maps_the_published_platforms() {
        assert_eq!(
            Platform::detect("windows", "x86_64", false),
            Some(Platform::WindowsX64)
        );
        assert_eq!(
            Platform::detect("macos", "aarch64", false),
            Some(Platform::MacArm64)
        );
        assert_eq!(Platform::detect("macos", "x86_64", false), None);
        assert_eq!(Platform::detect("windows", "aarch64", false), None);
    }

    #[test]
    fn missing_body_gives_empty_notes() {
        let json = json!({
            "tag_name": "v0.4.1",
            "body": null,
            "assets": [asset("Keitora.app.tar.gz"), asset("Keitora.app.tar.gz.sig")],
        })
        .to_string();
        let Assessment::Available(c) = assess(&json, &v("0.4.0"), Platform::MacArm64) else {
            panic!("expected available")
        };
        assert_eq!(c.notes, "");
        assert_eq!(c.published_at, None);
    }

    #[test]
    fn notes_truncate_on_a_char_boundary() {
        let short = "é".repeat(NOTES_LIMIT);
        assert_eq!(truncate_notes(&short), short);
        let long = "é".repeat(NOTES_LIMIT + 5);
        let cut = truncate_notes(&long);
        assert_eq!(cut.chars().count(), NOTES_LIMIT + 1);
        assert!(cut.ends_with("é…"));
        let mixed = format!("{}日本語", "a".repeat(NOTES_LIMIT - 1));
        assert_eq!(
            truncate_notes(&mixed),
            format!("{}日…", "a".repeat(NOTES_LIMIT - 1))
        );
    }

    fn candidate() -> Candidate {
        Candidate {
            version: v("0.4.1"),
            notes: "notes here".into(),
            release_url: "https://example.com/releases/v0.4.1".into(),
            published_at: Some("2026-11-01T12:00:00Z".into()),
            asset_url: WIN_URL.into(),
            sig_url: format!("{WIN_URL}.sig"),
        }
    }

    #[test]
    fn manifest_shape() {
        assert_eq!(
            manifest(&candidate(), "  c2lnbmF0dXJl\r\n"),
            json!({
                "version": "0.4.1",
                "notes": "notes here",
                "pub_date": "2026-11-01T12:00:00Z",
                "url": WIN_URL,
                "signature": "c2lnbmF0dXJl",
            })
        );
        let undated = Candidate {
            published_at: None,
            ..candidate()
        };
        assert!(manifest(&undated, "sig").get("pub_date").is_none());
    }

    #[test]
    fn manifest_deserializes_as_the_plugin_remote_release() {
        let release: tauri_plugin_updater::RemoteRelease =
            serde_json::from_value(manifest(&candidate(), "c2lnbmF0dXJl\n")).unwrap();
        assert_eq!(release.version, v("0.4.1"));
        assert_eq!(release.notes.as_deref(), Some("notes here"));
        assert!(release.pub_date.is_some());
        let target = "windows-x86_64";
        assert_eq!(release.download_url(target).unwrap().as_str(), WIN_URL);
        assert_eq!(release.signature(target).unwrap(), "c2lnbmF0dXJl");
    }
}
