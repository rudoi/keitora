# Loss report

Inputs compared: the original request, the first proposal (da39d26), the user's answers to the open
questions (refinement cycle 1), and two automated review cycles (code references and plugin source
in cycle 2; a hardening audit in cycle 3). Final document: `proposal.md` (c6f07e0).

## Preserved
- Public repo, so plain HTTPS with no `gh` CLI and no token. Discovery goes through `/releases/latest`.
- The updater design is ported from an existing app: a dynamic manifest, `restore_update`, plugin
  verify and install, a sticky candidate, emit-on-change, 30 s then 6 h scheduling, and cleanup of
  temp folders the plugin leaves behind.
- Every planning answer: minisign signing, Windows and macOS, keep sessions alive on update, auto
  check with a settings toggle.
- Refine answers 1–4: NSIS relaunches by itself; `Keitora.app.tar.gz`; an overlay mechanism; a macOS
  menu item.

## Modified (justified)
- **Overlay transport:** the overlay is written to a JSON file and passed with `--config <path>`,
  instead of inline JSON. The mechanism is the same, `bundleOverlay()` in `release.lib.ts`; only
  how it is passed changed. Why: yarn 1 and Windows shells can mangle inline JSON quotes.
- **Secret placement:** the secrets live in the tag-only `release` environment, with a Windows tag
  twin job, rather than as repository secrets. Why: repository secrets are readable by same-repo
  PR, schedule and dispatch runs, and this matches how the macOS certificate is already guarded.
- **macOS overlay:** it also sets `targets: ["app","dmg"]`. Why: without the `app` target the
  bundler builds no tarball.
- **Tauri version:** bumped 2.11 → ≥ 2.12. Why: `restore_update` needs plugin ≥ 2.14, and that
  plugin needs tauri 2.12.
- **Settings write path:** a dedicated `set_update_auto_check` command instead of extending
  `save_user_settings`. Why: it matches the existing pattern (`set_skip_permissions`).

## Intentionally dropped
- The ported app's `gh` classification, auth probes and pause machinery. These have no meaning
  for a public repo.
- Persisting dismissals, cancelling a download, and prerelease channels. These are listed as out of
  scope.

## Unintentionally dropped
- None found.
