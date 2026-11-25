//! The vendor install script: herdr's own `install.ps1` / `install.sh`, run hidden with a
//! scrubbed environment, then located and checked on disk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::floor::HERDR_FLOOR;
use super::manifest::Platform;
use super::{binary_version, FailureKind, InstallFailure, Installed, Proc, ProcError, ProcSpec};

/// Hard limit for one script run.
pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Hard limit for the script-host probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Hard limit for downloading the unix install script.
pub const SCRIPT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

const PS_INSTALL: &str = "irm https://herdr.dev/install.ps1 | iex";
const SH_SCRIPT_URL: &str = "https://herdr.dev/install.sh";
const PS_LANGUAGE_MODE: &str = "$ExecutionContext.SessionState.LanguageMode";

/// Env vars the script reads that could redirect it away from a plain stable install.
pub const SCRUBBED_VARS: [&str; 4] = [
    "HERDR_HOME",
    "HERDR_INSTALL_DIR",
    "HERDR_MANIFEST_URL",
    "HERDR_EXPECTED_BUILD_ID",
];

/// Where the unix install script is downloaded before it runs.
pub fn script_file(keitora_home: &Path) -> PathBuf {
    keitora_home.join("tmp").join("herdr-install.sh")
}

/// The unix download step: fetch the script to `dest` with `curl`, whose exit code is
/// checked on its own (a `curl | sh` pipe reports only `sh`'s, so a failed download
/// would look like a successful install).
pub fn download_command(dest: &Path) -> ProcSpec {
    let dest = dest.to_string_lossy();
    ProcSpec::new(
        "curl",
        &[
            "-fsSL",
            "--proto",
            "=https",
            "--create-dirs",
            "-o",
            &dest,
            SH_SCRIPT_URL,
        ],
    )
}

/// The exact command that runs the vendor script on `platform` (env not yet applied).
/// On unix it runs the file [`download_command`] saved to `script`.
pub fn script_command(platform: Platform, script: &Path) -> ProcSpec {
    if platform.is_windows() {
        ProcSpec::new(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                PS_INSTALL,
            ],
        )
    } else {
        ProcSpec::new("sh", &[&*script.to_string_lossy()])
    }
}

/// Lowercased, slash-normalised form for a prefix check that ignores case on Windows.
fn norm(s: &str, platform: Platform) -> String {
    let s = s.replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if platform.is_windows() {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn is_inside(value: &str, root: &Path, platform: Platform) -> bool {
    let (v, r) = (
        norm(value, platform),
        norm(&root.to_string_lossy(), platform),
    );
    !r.is_empty() && (v == r || v.starts_with(&format!("{r}/")))
}

/// Apply the script's env policy to `spec`, given the current process env `vars`:
/// remove [`SCRUBBED_VARS`] and any `XDG_*` that points inside `keitora_home`, and pin
/// `HERDR_CHANNEL=stable`. Everything else, proxy variables included, is inherited.
pub fn apply_script_env(
    spec: &mut ProcSpec,
    vars: &[(String, String)],
    keitora_home: &Path,
    platform: Platform,
) {
    spec.env_remove = SCRUBBED_VARS.iter().map(|s| s.to_string()).collect();
    for (k, v) in vars {
        if k.starts_with("XDG_") && is_inside(v, keitora_home, platform) {
            spec.env_remove.push(k.clone());
        }
    }
    spec.env_set = vec![("HERDR_CHANNEL".to_string(), "stable".to_string())];
}

/// Remove ANSI escape sequences (CSI, OSC, and two-byte escapes) and stray control
/// characters other than tab from one output line.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters/intermediates up to a final byte in 0x40..=0x7e.
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Where the script left herdr. Windows: the newest existing standalone release
/// (`~/.herdr/packages/standalone/releases/<ver>-<triple>/herdr.exe`), never the
/// `current` junction a later vendor install would retarget. macOS: `~/.local/bin/herdr`.
pub fn installed_path(platform: Platform, home: &Path) -> Option<PathBuf> {
    if platform.is_windows() {
        // The platform's own exe name, not this build's: the two differ when a test
        // reasons about a Windows layout on a unix host.
        keitora_tasks::herdr_release_exes_in(
            &keitora_tasks::herdr_releases_dir_with_home(home),
            platform.exe_name(),
        )
        .into_iter()
        .next()
    } else {
        let p = home.join(".local").join("bin").join("herdr");
        p.is_file().then_some(p)
    }
}

/// What to run the script for.
pub struct Request<'a> {
    pub platform: Platform,
    /// The user's home (injected; tests use a temp dir).
    pub home: &'a Path,
    pub keitora_home: &'a Path,
    /// The current process environment.
    pub env: &'a [(String, String)],
    /// The version of the herdr the script replaces in place (an upgrade). The run
    /// only succeeds if the installed version then differs from it.
    pub previous: Option<&'a semver::Version>,
}

/// How a script (or its download) that could not run maps to a failure.
fn spawn_failure(e: ProcError, program: &Path, what: &str, timeout: Duration) -> InstallFailure {
    match e {
        ProcError::Timeout => InstallFailure::new(
            FailureKind::Timeout,
            format!("{what} did not finish within {} s", timeout.as_secs()),
        ),
        ProcError::NotFound => InstallFailure::new(
            FailureKind::PolicyBlocked,
            format!("{} is not available", program.display()),
        ),
        ProcError::Spawn(m) => InstallFailure::new(
            FailureKind::PolicyBlocked,
            format!("could not start {what}: {m}"),
        ),
    }
}

/// Download the unix install script to `dest`. A non-zero `curl` exit is a network
/// failure, never a script that "succeeded" without running.
fn download_script(proc_: &dyn Proc, req: &Request<'_>, dest: &Path) -> Result<(), InstallFailure> {
    let mut spec = download_command(dest);
    apply_script_env(&mut spec, req.env, req.keitora_home, req.platform);
    let out = proc_
        .run(&spec, SCRIPT_DOWNLOAD_TIMEOUT, &mut |_| {})
        .map_err(|e| {
            spawn_failure(
                e,
                &spec.program,
                "the install script download",
                SCRIPT_DOWNLOAD_TIMEOUT,
            )
        })?;
    if out.success() {
        return Ok(());
    }
    let code = out
        .code
        .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
    let detail = out
        .stderr
        .lines()
        .rev()
        .map(|l| strip_ansi(l).trim().to_string())
        .find(|l| !l.is_empty())
        .map(|l| format!(": {l}"))
        .unwrap_or_default();
    Err(InstallFailure::new(
        FailureKind::Network,
        format!("the install script could not be downloaded (curl exited with {code}){detail}"),
    ))
}

/// Run the vendor script, streaming ANSI-stripped output lines to `on_line`. Success
/// needs exit 0 (on unix, of the download and of the script), the binary at
/// [`installed_path`], its `--version` at least the floor, and, when it replaced a
/// herdr in place, a version other than the one it replaced.
pub fn run(
    proc_: &dyn Proc,
    req: &Request<'_>,
    on_line: &mut dyn FnMut(&str),
) -> Result<Installed, InstallFailure> {
    let script = script_file(req.keitora_home);
    if req.platform.is_windows() {
        return run_script(proc_, req, &script, on_line);
    }
    download_script(proc_, req, &script)?;
    let result = run_script(proc_, req, &script, on_line);
    let _ = std::fs::remove_file(&script);
    result
}

fn run_script(
    proc_: &dyn Proc,
    req: &Request<'_>,
    script: &Path,
    on_line: &mut dyn FnMut(&str),
) -> Result<Installed, InstallFailure> {
    let mut spec = script_command(req.platform, script);
    apply_script_env(&mut spec, req.env, req.keitora_home, req.platform);
    let mut last_line = String::new();
    let out = proc_
        .run(&spec, SCRIPT_TIMEOUT, &mut |line| {
            let clean = strip_ansi(line);
            if !clean.trim().is_empty() {
                last_line = clean.trim().to_string();
            }
            on_line(&clean);
        })
        .map_err(|e| spawn_failure(e, &spec.program, "the install script", SCRIPT_TIMEOUT))?;
    if !out.success() {
        let code = out
            .code
            .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
        let detail = if last_line.is_empty() {
            String::new()
        } else {
            format!(": {last_line}")
        };
        return Err(InstallFailure::new(
            FailureKind::Other,
            format!("the install script exited with {code}{detail}"),
        ));
    }
    let path = installed_path(req.platform, req.home).ok_or_else(|| {
        InstallFailure::new(
            FailureKind::Incomplete,
            "the install script finished but herdr was not found where it installs",
        )
    })?;
    let (version, _) = binary_version(proc_, &path).map_err(|e| {
        InstallFailure::new(
            FailureKind::Incomplete,
            format!("installed herdr would not run: {e}"),
        )
    })?;
    if version < HERDR_FLOOR {
        return Err(InstallFailure::new(
            FailureKind::TooOldAfterInstall,
            format!("the install script installed herdr {version}, below {HERDR_FLOOR}"),
        ));
    }
    if req.previous == Some(&version) {
        return Err(InstallFailure::new(
            FailureKind::Incomplete,
            format!("the install script finished but herdr is still {version}"),
        ));
    }
    Ok(Installed { path, version })
}

/// Whether the script host can run the vendor script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptHost {
    Usable,
    /// PowerShell runs but not in `FullLanguage` (Constrained Language Mode).
    Restricted(String),
    /// PowerShell could not run at all (missing, AppLocker, timeout).
    Blocked,
}

/// The probe command: prints PowerShell's language mode.
pub fn probe_command() -> ProcSpec {
    ProcSpec::new(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            PS_LANGUAGE_MODE,
        ],
    )
}

/// Interpret the probe's result: `FullLanguage` on stdout is usable; another mode is
/// restricted; a failure to run, a timeout, or a non-zero exit with no output is
/// blocked.
pub fn interpret_probe(result: &Result<super::ProcOutput, ProcError>) -> ScriptHost {
    let Ok(out) = result else {
        return ScriptHost::Blocked;
    };
    let mode = out.stdout.trim();
    if out.success() && mode == "FullLanguage" {
        return ScriptHost::Usable;
    }
    if mode.is_empty() {
        return ScriptHost::Blocked;
    }
    ScriptHost::Restricted(mode.lines().last().unwrap_or(mode).trim().to_string())
}

/// Probe the script host. Always usable on unix (`sh` needs no probe), without spawning.
pub fn probe_script_host(platform: Platform, proc_: &dyn Proc) -> ScriptHost {
    if !platform.is_windows() {
        return ScriptHost::Usable;
    }
    interpret_probe(&proc_.run(&probe_command(), PROBE_TIMEOUT, &mut |_| {}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::fakes::{ok, FakeProc};
    use crate::deps::ProcOutput;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn windows_command_line_is_exact() {
        let spec = script_command(Platform::WindowsX86_64, Path::new("unused"));
        assert_eq!(spec.program, PathBuf::from("powershell"));
        assert_eq!(
            spec.args,
            [
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "irm https://herdr.dev/install.ps1 | iex"
            ]
        );
    }

    #[test]
    fn macos_command_line_is_exact() {
        let file = script_file(Path::new("/Users/me/.keitora"));
        assert_eq!(file, Path::new("/Users/me/.keitora/tmp/herdr-install.sh"));
        let download = download_command(&file);
        assert_eq!(download.program, PathBuf::from("curl"));
        let dest = file.to_string_lossy().to_string();
        assert_eq!(
            download.args,
            [
                "-fsSL",
                "--proto",
                "=https",
                "--create-dirs",
                "-o",
                dest.as_str(),
                "https://herdr.dev/install.sh"
            ]
        );
        for p in [Platform::MacosAarch64, Platform::MacosX86_64] {
            let spec = script_command(p, &file);
            assert_eq!(spec.program, PathBuf::from("sh"));
            assert_eq!(spec.args, [dest.as_str()]);
        }
    }

    #[test]
    fn env_scrub_list_and_channel() {
        let mut spec = script_command(Platform::WindowsX86_64, Path::new("unused"));
        let vars = env(&[
            ("HERDR_HOME", r"C:\Users\x\.keitora\herdr"),
            ("HTTPS_PROXY", "http://proxy.example.com:8080"),
            ("NO_PROXY", "localhost"),
            ("PATH", r"C:\Windows"),
        ]);
        apply_script_env(
            &mut spec,
            &vars,
            Path::new(r"C:\Users\x\.keitora"),
            Platform::WindowsX86_64,
        );
        assert_eq!(
            spec.env_remove,
            [
                "HERDR_HOME",
                "HERDR_INSTALL_DIR",
                "HERDR_MANIFEST_URL",
                "HERDR_EXPECTED_BUILD_ID"
            ]
        );
        assert_eq!(
            spec.env_set,
            [("HERDR_CHANNEL".to_string(), "stable".to_string())]
        );
        // Proxy variables are inherited, never removed or overridden.
        for k in ["HTTPS_PROXY", "NO_PROXY", "PATH"] {
            assert!(!spec.env_remove.iter().any(|r| r == k), "{k}");
        }
    }

    #[test]
    fn xdg_inside_keitora_home_is_removed_outside_kept() {
        let mut spec = script_command(Platform::MacosAarch64, Path::new("unused"));
        let vars = env(&[
            ("XDG_CONFIG_HOME", "/Users/me/.keitora/herdr/config"),
            ("XDG_STATE_HOME", "/Users/me/.keitora"),
            ("XDG_DATA_HOME", "/Users/me/.local/share"),
            ("XDG_CACHE_HOME", "/Users/me/.keitora-other/cache"),
        ]);
        apply_script_env(
            &mut spec,
            &vars,
            Path::new("/Users/me/.keitora"),
            Platform::MacosAarch64,
        );
        let removed: Vec<&str> = spec.env_remove.iter().map(String::as_str).collect();
        assert!(removed.contains(&"XDG_CONFIG_HOME"));
        assert!(removed.contains(&"XDG_STATE_HOME"));
        assert!(!removed.contains(&"XDG_DATA_HOME"));
        assert!(!removed.contains(&"XDG_CACHE_HOME"), "sibling prefix kept");

        // Windows compares case-insensitively and across separators.
        let mut spec = script_command(Platform::WindowsX86_64, Path::new("unused"));
        apply_script_env(
            &mut spec,
            &env(&[("XDG_CONFIG_HOME", "c:/users/X/.KEITORA/herdr/config")]),
            Path::new(r"C:\Users\x\.keitora"),
            Platform::WindowsX86_64,
        );
        assert!(spec.env_remove.iter().any(|k| k == "XDG_CONFIG_HOME"));
    }

    #[test]
    fn strip_ansi_removes_escapes() {
        assert_eq!(
            strip_ansi("\x1b[32mInstalled\x1b[0m herdr 0.9.6"),
            "Installed herdr 0.9.6"
        );
        assert_eq!(strip_ansi("\x1b]0;title\x07done"), "done");
        assert_eq!(strip_ansi("\x1b]8;;https://x\x1b\\link"), "link");
        assert_eq!(strip_ansi("50%\r"), "50%");
        assert_eq!(strip_ansi("a\tb"), "a\tb");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    /// Lay out a vendor install under `home` for `platform`.
    fn place_binary(home: &Path, platform: Platform) -> PathBuf {
        let exe = if platform.is_windows() {
            keitora_tasks::herdr_releases_dir_with_home(home)
                .join("0.9.6-x86_64-pc-windows-msvc")
                .join(platform.exe_name())
        } else {
            home.join(".local").join("bin").join("herdr")
        };
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"bin").unwrap();
        exe
    }

    fn request<'a>(
        platform: Platform,
        home: &'a Path,
        vars: &'a [(String, String)],
    ) -> Request<'a> {
        Request {
            platform,
            home,
            keitora_home: Path::new("/nonexistent/keitora-home"),
            env: vars,
            previous: None,
        }
    }

    /// The script call answers `script`; `--version` answers `version`; the unix
    /// download succeeds.
    fn proc_with(script: Result<ProcOutput, ProcError>, version: &'static str) -> FakeProc {
        FakeProc::new(move |spec| {
            if spec.args.iter().any(|a| a == "--version") {
                Ok(ok(version))
            } else if spec.program == Path::new("curl") {
                Ok(ok(""))
            } else {
                script.clone()
            }
        })
    }

    #[test]
    fn success_on_each_platform() {
        for platform in [Platform::WindowsX86_64, Platform::MacosAarch64] {
            let home = tempfile::tempdir().unwrap();
            let exe = place_binary(home.path(), platform);
            let proc_ = proc_with(Ok(ok("\x1b[1minstalled\x1b[0m")), "herdr 0.9.6");
            let mut lines = Vec::new();
            let got = run(&proc_, &request(platform, home.path(), &[]), &mut |l| {
                lines.push(l.to_string())
            })
            .unwrap();
            assert_eq!(got.path, exe);
            assert_eq!(got.version, semver::Version::new(0, 9, 6));
            assert_eq!(lines, ["installed"]);
            let calls = proc_.calls();
            let script = calls
                .iter()
                .find(|(s, _)| s.program != Path::new("curl"))
                .unwrap();
            assert_eq!(script.1, SCRIPT_TIMEOUT);
            assert_eq!(script.0.env_set[0].0, "HERDR_CHANNEL");
        }
    }

    /// A failed `curl` stops the run as a network failure before `sh` ever runs, so
    /// an empty script can't pass for an install.
    #[test]
    fn a_failed_script_download_is_a_network_failure() {
        let home = tempfile::tempdir().unwrap();
        place_binary(home.path(), Platform::MacosAarch64);
        let proc_ = FakeProc::new(|spec| {
            if spec.program == Path::new("curl") {
                Ok(ProcOutput {
                    code: Some(6),
                    stdout: String::new(),
                    stderr: "curl: (6) Could not resolve host: herdr.dev\n".into(),
                })
            } else {
                Ok(ok("herdr 0.9.6"))
            }
        });
        let err = run(
            &proc_,
            &request(Platform::MacosAarch64, home.path(), &[]),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind, FailureKind::Network);
        assert!(
            err.message.contains("Could not resolve host"),
            "{}",
            err.message
        );
        let programs: Vec<PathBuf> = proc_.calls().into_iter().map(|(s, _)| s.program).collect();
        assert_eq!(programs, [PathBuf::from("curl")], "sh never ran");
    }

    /// Windows has no separate download step.
    #[test]
    fn windows_runs_the_script_without_curl() {
        let home = tempfile::tempdir().unwrap();
        place_binary(home.path(), Platform::WindowsX86_64);
        let proc_ = proc_with(Ok(ok("done")), "herdr 0.9.6");
        run(
            &proc_,
            &request(Platform::WindowsX86_64, home.path(), &[]),
            &mut |_| {},
        )
        .unwrap();
        assert!(proc_
            .calls()
            .iter()
            .all(|(s, _)| s.program != Path::new("curl")));
    }

    /// An upgrade that leaves the same version in place did not upgrade anything.
    #[test]
    fn an_upgrade_that_leaves_the_old_version_is_incomplete() {
        for platform in [Platform::WindowsX86_64, Platform::MacosAarch64] {
            let home = tempfile::tempdir().unwrap();
            place_binary(home.path(), platform);
            let proc_ = proc_with(Ok(ok("done")), "herdr 0.9.6");
            let same = semver::Version::new(0, 9, 6);
            let older = semver::Version::new(0, 9, 3);
            let mut req = request(platform, home.path(), &[]);
            req.previous = Some(&same);
            let err = run(&proc_, &req, &mut |_| {}).unwrap_err();
            assert_eq!(err.kind, FailureKind::Incomplete, "{platform:?}");
            assert!(err.message.contains("still 0.9.6"), "{}", err.message);
            req.previous = Some(&older);
            assert_eq!(run(&proc_, &req, &mut |_| {}).unwrap().version, same);
        }
    }

    #[test]
    fn windows_picks_the_newest_release_dir() {
        let home = tempfile::tempdir().unwrap();
        let root = keitora_tasks::herdr_releases_dir_with_home(home.path());
        for v in [
            "0.9.5-x86_64-pc-windows-msvc",
            "0.9.10-x86_64-pc-windows-msvc",
        ] {
            std::fs::create_dir_all(root.join(v)).unwrap();
            std::fs::write(root.join(v).join("herdr.exe"), b"bin").unwrap();
        }
        let got = installed_path(Platform::WindowsX86_64, home.path()).unwrap();
        assert!(got.to_string_lossy().contains("0.9.10"));
    }

    #[test]
    fn exit_zero_without_the_binary_is_incomplete() {
        for platform in [Platform::WindowsX86_64, Platform::MacosAarch64] {
            let home = tempfile::tempdir().unwrap();
            let proc_ = proc_with(Ok(ok("done")), "herdr 0.9.6");
            let err = run(&proc_, &request(platform, home.path(), &[]), &mut |_| {}).unwrap_err();
            assert_eq!(err.kind, FailureKind::Incomplete, "{platform:?}");
        }
    }

    #[test]
    fn a_version_below_the_floor_is_too_old_after_install() {
        let home = tempfile::tempdir().unwrap();
        place_binary(home.path(), Platform::WindowsX86_64);
        let proc_ = proc_with(Ok(ok("done")), "herdr 0.9.3");
        let err = run(
            &proc_,
            &request(Platform::WindowsX86_64, home.path(), &[]),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind, FailureKind::TooOldAfterInstall);
    }

    #[test]
    fn an_unrunnable_installed_binary_is_incomplete() {
        let home = tempfile::tempdir().unwrap();
        place_binary(home.path(), Platform::MacosAarch64);
        let proc_ = proc_with(Ok(ok("done")), "garbage");
        let err = run(
            &proc_,
            &request(Platform::MacosAarch64, home.path(), &[]),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind, FailureKind::Incomplete);
    }

    #[test]
    fn script_failures_are_classified() {
        let home = tempfile::tempdir().unwrap();
        let cases = [
            (Err(ProcError::Timeout), FailureKind::Timeout),
            (Err(ProcError::NotFound), FailureKind::PolicyBlocked),
            (
                Err(ProcError::Spawn("blocked by group policy".into())),
                FailureKind::PolicyBlocked,
            ),
            (
                Ok(ProcOutput {
                    code: Some(1),
                    stdout: "error: download failed\n".into(),
                    stderr: String::new(),
                }),
                FailureKind::Other,
            ),
        ];
        for (script, want) in cases {
            let proc_ = proc_with(script.clone(), "herdr 0.9.6");
            let err = run(
                &proc_,
                &request(Platform::WindowsX86_64, home.path(), &[]),
                &mut |_| {},
            )
            .unwrap_err();
            assert_eq!(err.kind, want, "{script:?}");
        }
        let proc_ = proc_with(
            Ok(ProcOutput {
                code: Some(1),
                stdout: "error: download failed\n".into(),
                stderr: String::new(),
            }),
            "",
        );
        let err = run(
            &proc_,
            &request(Platform::WindowsX86_64, home.path(), &[]),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            err.message.contains("error: download failed"),
            "{}",
            err.message
        );
    }

    #[test]
    fn probe_command_is_exact() {
        let spec = probe_command();
        assert_eq!(spec.program, PathBuf::from("powershell"));
        assert_eq!(
            spec.args,
            [
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ExecutionContext.SessionState.LanguageMode"
            ]
        );
    }

    #[test]
    fn probe_interpretation_table() {
        let out = |code, stdout: &str| {
            Ok(ProcOutput {
                code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        };
        let cases = [
            (out(Some(0), "FullLanguage\r\n"), ScriptHost::Usable),
            (
                out(Some(0), "ConstrainedLanguage\n"),
                ScriptHost::Restricted("ConstrainedLanguage".into()),
            ),
            (
                out(Some(1), "RestrictedLanguage"),
                ScriptHost::Restricted("RestrictedLanguage".into()),
            ),
            (out(Some(1), ""), ScriptHost::Blocked),
            (out(None, "  "), ScriptHost::Blocked),
            (out(Some(0), ""), ScriptHost::Blocked),
            (Err(ProcError::NotFound), ScriptHost::Blocked),
            (Err(ProcError::Timeout), ScriptHost::Blocked),
            (
                Err(ProcError::Spawn("access denied".into())),
                ScriptHost::Blocked,
            ),
        ];
        for (result, want) in cases {
            assert_eq!(interpret_probe(&result), want, "{result:?}");
        }
    }

    #[test]
    fn probe_always_passes_on_unix_without_spawning() {
        let proc_ = FakeProc::new(|_| panic!("must not spawn"));
        assert_eq!(
            probe_script_host(Platform::MacosAarch64, &proc_),
            ScriptHost::Usable
        );
        assert!(proc_.calls().is_empty());
        let proc_ = FakeProc::version("FullLanguage");
        assert_eq!(
            probe_script_host(Platform::WindowsX86_64, &proc_),
            ScriptHost::Usable
        );
        assert_eq!(proc_.calls()[0].0, probe_command());
        assert_eq!(proc_.calls()[0].1, PROBE_TIMEOUT);
    }
}
