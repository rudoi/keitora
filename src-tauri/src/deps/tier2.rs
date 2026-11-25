//! Direct download: fetch the release asset into a staging dir under Keitora's managed
//! root, verify its sha256, unpack it safely, check that it runs, and move it into place
//! as `<keitora_home>/bin/herdr/<version>/`.

use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use super::floor::HERDR_FLOOR;
use super::manifest::{Net, Platform, Release};
use super::{
    binary_version, FailureKind, Fs, InstallFailure, Installed, Proc, Stage, UnpackError,
    MAX_UNPACKED_BYTES,
};

/// Prefix of the per-run staging dirs inside the managed root.
pub const STAGING_PREFIX: &str = ".staging-";
/// Most entries a release archive may hold.
const MAX_ENTRIES: usize = 10_000;

/// `<keitora_home>/bin/herdr`: every managed version lives in a subdir named after it.
pub fn managed_root(keitora_home: &Path) -> PathBuf {
    keitora_home.join("bin").join("herdr")
}

/// What to install and where.
pub struct Request<'a> {
    pub release: &'a Release,
    pub platform: Platform,
    pub keitora_home: &'a Path,
}

/// Whether a raw archive entry name is relative and stays inside the destination: no
/// `..` segment, no leading slash or backslash, no drive or UNC prefix.
fn raw_name_is_safe(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return false;
    }
    !name.split(['/', '\\']).any(|seg| seg == "..")
}

/// Unpack a zip into the existing directory `dest`. Every entry must pass
/// `enclosed_name()` and [`raw_name_is_safe`]; symlinks are refused; the real number of
/// bytes written (not the header's claim) is capped at `max_bytes` in total.
pub fn unpack_zip<R: Read + Seek>(
    reader: R,
    dest: &Path,
    max_bytes: u64,
) -> Result<(), UnpackError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| UnpackError::Corrupt(e.to_string()))?;
    if archive.len() > MAX_ENTRIES {
        return Err(UnpackError::TooLarge);
    }
    let mut written = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| UnpackError::Corrupt(e.to_string()))?;
        let raw = entry
            .name()
            .map(|n| n.into_owned())
            .map_err(|e| UnpackError::Corrupt(e.to_string()))?;
        if entry.is_symlink() || !raw_name_is_safe(&raw) {
            return Err(UnpackError::Unsafe(raw));
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| UnpackError::Unsafe(raw.clone()))?;
        if rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(UnpackError::Unsafe(raw));
        }
        let out_path = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|e| UnpackError::Io(e.to_string()))?;
            continue;
        }
        if written.saturating_add(entry.size()) > max_bytes {
            return Err(UnpackError::TooLarge);
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| UnpackError::Io(e.to_string()))?;
        }
        // `create_new`: a duplicate entry name never overwrites an earlier one.
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .map_err(|e| UnpackError::Io(format!("{raw}: {e}")))?;
        let budget = max_bytes - written;
        let n = io::copy(&mut (&mut entry).take(budget + 1), &mut out)
            .map_err(|e| UnpackError::Corrupt(format!("{raw}: {e}")))?;
        if n > budget {
            return Err(UnpackError::TooLarge);
        }
        out.flush().map_err(|e| UnpackError::Io(e.to_string()))?;
        written += n;
        #[cfg(unix)]
        if entry.unix_mode().is_some_and(|m| m & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(())
}

fn fail(kind: FailureKind, message: impl Into<String>) -> InstallFailure {
    InstallFailure::new(kind, message)
}

fn io_fail(what: &str, e: io::Error) -> InstallFailure {
    fail(FailureKind::Other, format!("{what}: {e}"))
}

/// The dir inside `pkg` that holds the executable: `pkg` itself, or its only subdir
/// when the archive wraps everything in one top-level folder.
fn content_root(fs: &dyn Fs, pkg: &Path, exe_name: &str) -> Option<PathBuf> {
    if fs.exists(&pkg.join(exe_name)) {
        return Some(pkg.to_path_buf());
    }
    let entries = fs.list(pkg).ok()?;
    match entries.as_slice() {
        [only] if fs.is_dir(only) && fs.exists(&only.join(exe_name)) => Some(only.clone()),
        _ => None,
    }
}

/// Run `--version` on a candidate and require it to be at least the floor.
fn verify_runs(proc_: &dyn Proc, exe: &Path) -> Result<semver::Version, InstallFailure> {
    let (version, _) = binary_version(proc_, exe).map_err(|e| {
        fail(
            FailureKind::BlockedByAV,
            format!("the downloaded herdr would not run ({e}); antivirus may have blocked it"),
        )
    })?;
    if version < HERDR_FLOOR {
        return Err(fail(
            FailureKind::TooOldAfterInstall,
            format!("downloaded herdr reports {version}, below {HERDR_FLOOR}"),
        ));
    }
    Ok(version)
}

/// Download, verify and install `req.release`. `progress` sees forward-only stages
/// (`Downloading` with byte counts, then `Installing`, then `Verifying`). An existing
/// version dir that runs is reused without downloading. On any failure the staging dir
/// is removed and no version dir is created.
pub fn install(
    net: &dyn Net,
    proc_: &dyn Proc,
    fs: &dyn Fs,
    req: &Request<'_>,
    progress: &mut dyn FnMut(Stage, u64, Option<u64>),
) -> Result<Installed, InstallFailure> {
    let root = managed_root(req.keitora_home);
    let exe_name = req.platform.exe_name();
    let final_dir = root.join(req.release.version.to_string());
    let final_exe = final_dir.join(exe_name);

    if fs.exists(&final_exe) {
        if let Ok(version) = verify_runs(proc_, &final_exe) {
            return Ok(Installed {
                path: final_exe,
                version,
            });
        }
        log::warn!(
            "[deps] existing {} does not run; replacing it",
            final_dir.display()
        );
    }

    fs.create_dir_all(&root)
        .map_err(|e| io_fail("create the managed herdr dir", e))?;
    let staging = root.join(format!("{STAGING_PREFIX}{:016x}", rand::random::<u64>()));
    fs.create_dir_all(&staging)
        .map_err(|e| io_fail("create a staging dir", e))?;
    let result = install_staged(net, proc_, fs, req, &staging, &final_dir, progress);
    if fs.exists(&staging) {
        if let Err(e) = fs.remove_dir_all(&staging) {
            log::warn!("[deps] could not remove {}: {e}", staging.display());
        }
    }
    result
}

fn install_staged(
    net: &dyn Net,
    proc_: &dyn Proc,
    fs: &dyn Fs,
    req: &Request<'_>,
    staging: &Path,
    final_dir: &Path,
    progress: &mut dyn FnMut(Stage, u64, Option<u64>),
) -> Result<Installed, InstallFailure> {
    let exe_name = req.platform.exe_name();
    let asset = staging.join("asset.download");
    progress(Stage::Downloading, 0, None);
    net.download(&req.release.url, &asset, &mut |done, total| {
        progress(Stage::Downloading, done, total)
    })
    .map_err(|e| fail(FailureKind::from(&e), format!("download failed: {e}")))?;

    progress(Stage::Installing, 0, None);
    let digest = fs
        .sha256(&asset)
        .map_err(|e| io_fail("hash the download", e))?;
    if digest != req.release.sha256 {
        return Err(fail(
            FailureKind::Checksum,
            format!(
                "checksum mismatch: expected {}, got {digest}",
                req.release.sha256
            ),
        ));
    }

    let pkg = staging.join("pkg");
    fs.create_dir_all(&pkg)
        .map_err(|e| io_fail("create the unpack dir", e))?;
    if req.release.url.to_ascii_lowercase().ends_with(".zip") {
        // The Windows asset is a zip holding herdr.exe plus its ConPTY bundle files;
        // every entry is unpacked side by side.
        fs.unpack_zip(&asset, &pkg, MAX_UNPACKED_BYTES)
            .map_err(|e| fail(FailureKind::Other, e.to_string()))?;
        let _ = fs.remove_file(&asset);
    } else {
        // The macOS asset is the bare executable.
        let exe = pkg.join(exe_name);
        fs.rename(&asset, &exe)
            .map_err(|e| io_fail("move the download", e))?;
        fs.set_executable(&exe)
            .map_err(|e| io_fail("mark herdr executable", e))?;
    }
    let content = content_root(fs, &pkg, exe_name).ok_or_else(|| {
        fail(
            FailureKind::Incomplete,
            format!("the download has no {exe_name}"),
        )
    })?;

    progress(Stage::Verifying, 0, None);
    let version = verify_runs(proc_, &content.join(exe_name))?;
    if version != req.release.version {
        log::warn!(
            "[deps] manifest said {} but the binary reports {version}",
            req.release.version
        );
    }

    if fs.exists(final_dir) {
        // A leftover that failed the reuse check: replace it.
        fs.remove_dir_all(final_dir)
            .map_err(|e| io_fail("remove the broken version dir", e))?;
    }
    fs.rename(&content, final_dir)
        .map_err(|e| io_fail("move herdr into place", e))?;
    Ok(Installed {
        path: final_dir.join(exe_name),
        version,
    })
}

/// Delete stray staging dirs (left by a crash mid-install) under the managed root.
/// Touches nothing else. Returns how many were removed.
pub fn cleanup_staging(fs: &dyn Fs, keitora_home: &Path) -> usize {
    let root = managed_root(keitora_home);
    let Ok(entries) = fs.list(&root) else {
        return 0;
    };
    entries
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(STAGING_PREFIX))
                && fs.is_dir(p)
        })
        .filter(|p| fs.remove_dir_all(p).is_ok())
        .count()
}

/// Remove old managed versions, keeping the one `in_use` points into, every version at
/// least as new as it, and the newest one older than it (a rollback target). Without an
/// in-use managed version the two newest are kept. Names that are not versions (and
/// staging dirs) are never touched. Returns the removed dirs.
///
/// The herdr agent integration hooks (the Claude and Codex hook scripts that
/// `herdr integration install` writes) do not store an absolute herdr path: they call
/// `$HERDR_BIN_PATH` when the pane env sets it, else `herdr` from `PATH`. A live pane's
/// `HERDR_BIN_PATH` points at the running server's binary, so the in-use version must
/// never be pruned while it runs, which the rule above guarantees.
pub fn prune(fs: &dyn Fs, keitora_home: &Path, in_use: Option<&Path>) -> Vec<PathBuf> {
    let root = managed_root(keitora_home);
    let Ok(entries) = fs.list(&root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .iter()
        .filter(|p| fs.is_dir(p))
        .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
        .filter(|n| !n.starts_with('.') && semver::Version::parse(n).is_ok())
        .collect();
    keitora_tasks::sort_versions_desc(&mut names);

    let in_use_name = in_use
        .and_then(|p| p.strip_prefix(&root).ok())
        .and_then(|rel| rel.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_string)
        .filter(|n| names.contains(n));
    let keep: Vec<&String> = match &in_use_name {
        Some(cur) => {
            let pos = names.iter().position(|n| n == cur).unwrap_or(0);
            names.iter().take(pos + 2).collect()
        }
        None => names.iter().take(2).collect(),
    };
    names
        .iter()
        .filter(|n| !keep.contains(n))
        .map(|n| root.join(n))
        .filter(|dir| match fs.remove_dir_all(dir) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("[deps] could not prune {}: {e}", dir.display());
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::fakes::{FakeFs, FakeProc};
    use crate::deps::manifest::fake::{FakeNet, Reply};
    use crate::deps::{ProcError, RealFs};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::Ordering;
    use zip::write::SimpleFileOptions;

    const WIN_URL: &str =
        "https://github.com/herdrdev/herdr/releases/download/v0.9.6/herdr-x86_64-pc-windows-msvc.zip";
    const MAC_URL: &str =
        "https://github.com/herdrdev/herdr/releases/download/v0.9.6/herdr-aarch64-apple-darwin";

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// An in-memory zip of `(name, contents)` file entries.
    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn win_zip() -> Vec<u8> {
        zip_of(&[
            ("herdr.exe", b"MZ fake herdr"),
            ("conpty.dll", b"fake conpty"),
            ("OpenConsole.exe", b"fake console"),
        ])
    }

    fn release(url: &str, body: &[u8]) -> Release {
        Release {
            version: semver::Version::new(0, 9, 6),
            protocol: 22,
            url: url.to_string(),
            sha256: sha(body),
        }
    }

    fn run(
        net: &FakeNet,
        proc_: &FakeProc,
        fs: &dyn Fs,
        rel: &Release,
        platform: Platform,
        home: &Path,
    ) -> (Result<Installed, InstallFailure>, Vec<Stage>) {
        let mut stages = Vec::new();
        let r = install(
            net,
            proc_,
            fs,
            &Request {
                release: rel,
                platform,
                keitora_home: home,
            },
            &mut |s, _, _| {
                if stages.last() != Some(&s) {
                    stages.push(s)
                }
            },
        );
        (r, stages)
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn windows_zip_installs_with_bundle_files() {
        let tmp = tempfile::tempdir().unwrap();
        let body = win_zip();
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let proc_ = FakeProc::version("herdr 0.9.6");
        let (r, stages) = run(
            &net,
            &proc_,
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        let got = r.unwrap();
        let dir = managed_root(tmp.path()).join("0.9.6");
        assert_eq!(got.path, dir.join("herdr.exe"));
        assert_eq!(got.version, semver::Version::new(0, 9, 6));
        assert_eq!(
            dir_names(&dir),
            ["OpenConsole.exe", "conpty.dll", "herdr.exe"]
        );
        // Only the version dir remains; staging is gone.
        assert_eq!(dir_names(&managed_root(tmp.path())), ["0.9.6"]);
        assert_eq!(
            stages,
            [Stage::Downloading, Stage::Installing, Stage::Verifying]
        );
        // `--version` ran from staging, before the move.
        let (spec, _) = &proc_.calls()[0];
        assert!(spec.program.to_string_lossy().contains(STAGING_PREFIX));
    }

    #[test]
    fn archive_with_a_single_top_level_folder_is_flattened() {
        let tmp = tempfile::tempdir().unwrap();
        let body = zip_of(&[
            ("herdr-0.9.6/herdr.exe", b"MZ"),
            ("herdr-0.9.6/conpty.dll", b"x"),
        ]);
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(
            r.unwrap().path,
            managed_root(tmp.path()).join("0.9.6").join("herdr.exe")
        );
    }

    #[test]
    fn checksum_mismatch_leaves_no_final_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let body = win_zip();
        let mut rel = release(WIN_URL, &body);
        rel.sha256 = "0".repeat(64);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let proc_ = FakeProc::version("herdr 0.9.6");
        let (r, _) = run(
            &net,
            &proc_,
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::Checksum);
        assert!(dir_names(&managed_root(tmp.path())).is_empty());
        assert!(proc_.calls().is_empty(), "nothing runs before verification");
    }

    #[test]
    fn version_failure_maps_to_blocked_by_av() {
        let tmp = tempfile::tempdir().unwrap();
        let body = win_zip();
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let proc_ = FakeProc::new(|_| Err(ProcError::Spawn("access is denied".into())));
        let (r, _) = run(
            &net,
            &proc_,
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::BlockedByAV);
        assert!(dir_names(&managed_root(tmp.path())).is_empty());
    }

    #[test]
    fn a_binary_below_the_floor_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let body = win_zip();
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.4"),
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::TooOldAfterInstall);
    }

    #[test]
    fn download_failures_are_classified_and_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = release(WIN_URL, b"x");
        let net = FakeNet::default().with(
            WIN_URL,
            Reply::Fail(crate::deps::manifest::NetError::Offline),
        );
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::Offline);
        assert!(dir_names(&managed_root(tmp.path())).is_empty());
    }

    #[test]
    fn zip_without_the_exe_is_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let body = zip_of(&[("readme.txt", b"hi")]);
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::Incomplete);
    }

    #[test]
    fn rename_failure_leaves_no_final_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let body = win_zip();
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        let fs = FakeFs::default();
        fs.fail_rename.store(true, Ordering::SeqCst);
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &fs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap_err().kind, FailureKind::Other);
        assert!(dir_names(&managed_root(tmp.path())).is_empty());
    }

    #[test]
    fn an_existing_version_dir_is_reused_without_downloading() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = managed_root(tmp.path()).join("0.9.6");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("herdr.exe"), b"MZ").unwrap();
        let rel = release(WIN_URL, b"unused");
        let net = FakeNet::default();
        let (r, stages) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert_eq!(r.unwrap().path, dir.join("herdr.exe"));
        assert!(net.requests().is_empty());
        assert!(stages.is_empty());
    }

    #[test]
    fn a_broken_existing_version_dir_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = managed_root(tmp.path()).join("0.9.6");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("herdr.exe"), b"broken").unwrap();
        let body = win_zip();
        let rel = release(WIN_URL, &body);
        let net = FakeNet::default().with(WIN_URL, Reply::Body(body));
        // The first `--version` (the existing copy) fails; later ones succeed.
        let n = std::sync::atomic::AtomicUsize::new(0);
        let proc_ = FakeProc::new(move |_| {
            if n.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ProcError::Spawn("bad image".into()))
            } else {
                Ok(crate::deps::fakes::ok("herdr 0.9.6"))
            }
        });
        let (r, _) = run(
            &net,
            &proc_,
            &RealFs,
            &rel,
            Platform::WindowsX86_64,
            tmp.path(),
        );
        assert!(r.is_ok());
        assert_eq!(
            std::fs::read(dir.join("herdr.exe")).unwrap(),
            b"MZ fake herdr"
        );
        assert_eq!(net.requests(), [WIN_URL]);
    }

    #[test]
    fn macos_bare_binary_is_installed_as_herdr() {
        let tmp = tempfile::tempdir().unwrap();
        let body = b"\xcf\xfa\xed\xfe fake mach-o".to_vec();
        let rel = release(MAC_URL, &body);
        let net = FakeNet::default().with(MAC_URL, Reply::Body(body.clone()));
        let (r, _) = run(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &rel,
            Platform::MacosAarch64,
            tmp.path(),
        );
        let exe = r.unwrap().path;
        assert_eq!(exe, managed_root(tmp.path()).join("0.9.6").join("herdr"));
        assert_eq!(std::fs::read(&exe).unwrap(), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    fn unpack(bytes: Vec<u8>, cap: u64) -> (Result<(), UnpackError>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        (unpack_zip(io::Cursor::new(bytes), &dest, cap), tmp)
    }

    #[test]
    fn zip_with_a_parent_dir_entry_is_rejected() {
        for name in [
            "../evil.exe",
            "a/../../evil.exe",
            "a\\..\\evil.exe",
            "a/../b.txt",
        ] {
            let (r, tmp) = unpack(zip_of(&[(name, b"x")]), MAX_UNPACKED_BYTES);
            assert!(matches!(r, Err(UnpackError::Unsafe(_))), "{name}: {r:?}");
            assert!(!tmp.path().join("evil.exe").exists());
        }
    }

    #[test]
    fn zip_with_an_absolute_entry_is_rejected() {
        for name in ["/etc/evil", "\\evil.exe", "C:/evil.exe", "C:\\evil.exe"] {
            let (r, _) = unpack(zip_of(&[(name, b"x")]), MAX_UNPACKED_BYTES);
            assert!(matches!(r, Err(UnpackError::Unsafe(_))), "{name}: {r:?}");
        }
    }

    #[test]
    fn zip_with_a_symlink_entry_is_rejected() {
        let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        w.add_symlink("herdr.exe", "/usr/bin/evil", SimpleFileOptions::default())
            .unwrap();
        let (r, _) = unpack(w.finish().unwrap().into_inner(), MAX_UNPACKED_BYTES);
        assert!(matches!(r, Err(UnpackError::Unsafe(_))), "{r:?}");
    }

    #[test]
    fn unpack_size_cap_is_enforced() {
        let big = vec![0u8; 4096];
        let (r, _) = unpack(zip_of(&[("a.bin", &big), ("b.bin", &big)]), 6000);
        assert_eq!(r, Err(UnpackError::TooLarge));
        let (r, _) = unpack(zip_of(&[("a.bin", &big)]), 4095);
        assert_eq!(r, Err(UnpackError::TooLarge));
        let (r, _) = unpack(zip_of(&[("a.bin", &big), ("b.bin", &big)]), 8192);
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn nested_entries_and_dirs_unpack() {
        let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        w.add_directory("sub/", SimpleFileOptions::default())
            .unwrap();
        w.start_file("sub/inner.txt", SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"in").unwrap();
        let (r, tmp) = unpack(w.finish().unwrap().into_inner(), MAX_UNPACKED_BYTES);
        r.unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("out").join("sub").join("inner.txt")).unwrap(),
            b"in"
        );
    }

    #[test]
    fn garbage_is_corrupt() {
        let (r, _) = unpack(b"not a zip".to_vec(), MAX_UNPACKED_BYTES);
        assert!(matches!(r, Err(UnpackError::Corrupt(_))));
    }

    fn mkdirs(home: &Path, names: &[&str]) {
        for n in names {
            std::fs::create_dir_all(managed_root(home).join(n)).unwrap();
        }
    }

    #[test]
    fn prune_keeps_in_use_and_one_previous() {
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(
            tmp.path(),
            &["0.9.5", "0.9.6", "0.9.7", "0.10.0", ".staging-abc", "notes"],
        );
        let in_use = managed_root(tmp.path()).join("0.10.0").join("herdr.exe");
        let removed = prune(&RealFs, tmp.path(), Some(&in_use));
        let mut removed: Vec<String> = removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        removed.sort();
        assert_eq!(removed, ["0.9.5", "0.9.6"]);
        assert_eq!(
            dir_names(&managed_root(tmp.path())),
            [".staging-abc", "0.10.0", "0.9.7", "notes"]
        );
    }

    #[test]
    fn prune_never_removes_an_older_in_use_version() {
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), &["0.9.5", "0.9.6", "0.9.7"]);
        let in_use = managed_root(tmp.path()).join("0.9.6").join("herdr.exe");
        prune(&RealFs, tmp.path(), Some(&in_use));
        // In use, everything newer, and one older are all kept.
        assert_eq!(
            dir_names(&managed_root(tmp.path())),
            ["0.9.5", "0.9.6", "0.9.7"]
        );
        mkdirs(tmp.path(), &["0.9.4"]);
        prune(&RealFs, tmp.path(), Some(&in_use));
        assert_eq!(
            dir_names(&managed_root(tmp.path())),
            ["0.9.5", "0.9.6", "0.9.7"]
        );
    }

    #[test]
    fn prune_without_a_managed_in_use_keeps_two_newest() {
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), &["0.9.5", "0.9.6", "0.9.7"]);
        prune(
            &RealFs,
            tmp.path(),
            Some(Path::new("/opt/homebrew/bin/herdr")),
        );
        assert_eq!(dir_names(&managed_root(tmp.path())), ["0.9.6", "0.9.7"]);
        assert!(prune(&RealFs, tmp.path(), None).is_empty());
    }

    #[test]
    fn staging_cleanup_removes_only_staging_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        mkdirs(tmp.path(), &[".staging-1", ".staging-2", "0.9.6", ".other"]);
        std::fs::write(managed_root(tmp.path()).join(".staging-file"), b"x").unwrap();
        assert_eq!(cleanup_staging(&RealFs, tmp.path()), 2);
        assert_eq!(
            dir_names(&managed_root(tmp.path())),
            [".other", ".staging-file", "0.9.6"]
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(cleanup_staging(&RealFs, empty.path()), 0);
    }

    /// End to end through the real HTTP client against a loopback server: fetch a
    /// manifest, select, download, verify, unpack. Only `--version` is faked. Not run by
    /// default; it binds 127.0.0.1 only.
    #[test]
    #[ignore = "binds a loopback HTTP server; run explicitly"]
    fn end_to_end_over_loopback_http() {
        use crate::deps::manifest::{parse_manifest, select_release, HttpNet, NetError};
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let body = win_zip();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asset_path =
            "/herdrdev/herdr/releases/download/v0.9.6/herdr-x86_64-pc-windows-msvc.zip";
        let manifest = format!(
            r#"{{"version": "0.9.6", "protocol": 22, "releases": {{"0.9.6": {{"protocol": 22,
                "assets": {{"windows-x86_64": "https://github.com{asset_path}"}},
                "sha256": {{"windows-x86_64": "{}"}} }} }} }}"#,
            sha(&body)
        );
        let served = (manifest.clone().into_bytes(), body.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let payload = if line.contains("latest.json") {
                    &served.0
                } else {
                    &served.1
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(payload).unwrap();
            }
        });

        let loopback = format!("{base}/");
        let http = HttpNet::with_policy(move |u| u.starts_with(&loopback));
        // The real allowlist still refuses anything else.
        assert!(matches!(
            http.get_text("https://example.com/latest.json"),
            Err(NetError::HostNotAllowed(_))
        ));
        let text = http.get_text(&format!("{base}/latest.json")).unwrap();
        let m = parse_manifest(&text, Platform::WindowsX86_64).unwrap();
        let rel = select_release(&m, None).unwrap();

        /// Serves the selected github URL from the loopback server.
        struct Rewrite(HttpNet, String);
        impl Net for Rewrite {
            fn get_text(&self, url: &str) -> Result<String, NetError> {
                self.0.get_text(url)
            }
            fn download(
                &self,
                url: &str,
                dest: &Path,
                on_bytes: &mut dyn FnMut(u64, Option<u64>),
            ) -> Result<(), NetError> {
                let local = url.replace("https://github.com", &self.1);
                self.0.download(&local, dest, on_bytes)
            }
        }
        let loopback = format!("{base}/");
        let net = Rewrite(
            HttpNet::with_policy(move |u| u.starts_with(&loopback)),
            base,
        );
        let tmp = tempfile::tempdir().unwrap();
        let got = install(
            &net,
            &FakeProc::version("herdr 0.9.6"),
            &RealFs,
            &Request {
                release: &rel,
                platform: Platform::WindowsX86_64,
                keitora_home: tmp.path(),
            },
            &mut |_, _, _| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(got.path).unwrap(), b"MZ fake herdr");
    }
}
