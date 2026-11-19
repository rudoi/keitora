# 0075 — Agent update: type the update line in the pane's own shell

## Goal

Running a Codex/Claude update from Keitora must work whatever shell herdr starts in the
update pane. Today the line is POSIX-only (`'C:/…/npm.cmd' install -g …; echo
"__KEITORA_AGENT_UPDATE_DONE:$?"`); on Windows herdr usually starts **PowerShell**, where
it fails twice:

1. A quoted path followed by arguments is a `ParserError` (`Unexpected token 'install'`) —
   PowerShell needs the call operator: `& 'C:/…/npm.cmd' install …`.
2. `$?` is `True`/`False` in PowerShell, not an exit code, so even a running update would
   never print a parseable sentinel and the dialog would sit on "Updating…" until the
   15-minute watch bound.

## Why the shell varies

herdr picks the pane shell itself (`herdr workspace create` has no shell option). From the
herdr 0.9.3 binary: it honours `SHELL` when set, else falls back to `pwsh`/`powershell`
on Windows; on macOS it uses the user's login shell. So:

- Windows, Keitora launched from Explorer/Start → PowerShell (the reported bug).
- Windows, Keitora (and so herdr) launched from a Git Bash terminal → bash (`SHELL` set) —
  why the POSIX-only line passed when it was built.
- macOS → the login shell: zsh by default, bash, or **fish** (which rejects `$?`:
  "In fish, please use $status").

Predicting herdr's choice would mean re-implementing it; asking the live pane is exact.

## Scope

**In**
- `agent_update_run` creates the update pane itself (a plain shell), detects that pane's
  shell, and types the update line rendered for it — all before replying to the dialog.
- Shell detection from the pane's process (herdr `pane process-info` → `shell_pid` →
  process name).
- Rendering per shell family: **PowerShell** (`pwsh`, `powershell`), **POSIX** (`bash`,
  `zsh`, `sh`, `dash`, `ksh`, …) and **fish**.
- The dialog's `<TerminalPane>` only attaches (it already attaches before it would spawn).
- Unit tests for classification, rendering (quoting edge cases), sentinel parsing of each
  shell's echo, the start-run flow, and the herdr backend methods.

**Out**
- `cmd.exe` and other shells (nu, elvish, xonsh) as first-class targets: they get the
  platform fallback (below) and a log line. PowerShell + bash were confirmed enough; fish
  is included because it is a realistic macOS login shell and costs one match arm.
- The `guidance` text the update banner offers for the user's own terminal (unchanged).
- Choosing herdr's shell (no herdr option exists).
- A per-run nonce in the sentinel (dropped: the run spawns a fresh pane — `spawn` replaces
  a same-key pane and closes leftover reserved panes — so no old exit line can be read).

## Architecture

```
dialog ──agent_update_run──▶ AgentUpdates::start_run(backend, cli)        (one at a time)
                               1. begin_run → RunTicket { UpdateCommand, gen, … }
                               2. backend.kill_live(reserved key)          (as today)
                               3. backend.spawn(reserved key, home, size None, command None)
                                    → pane gen
                               4. backend.pane_shell(key) → Option<name>
                                    → ShellFamily::classify | fallback(os)
                               5. model::render_run_line(&cmd, family)
                               6. backend.run_line(key, pane gen, line)    (herdr `pane run`)
                               any error → close() the run, Err to the dialog
                             spawn_watcher(ticket)  — reads history for the sentinel (unchanged)
                             ◀── RunPlan { command: display text, cwd }
dialog ── <TerminalPane> attaches to the existing reserved pane (no command prop)
```

Typing happens inside the same blocking call that created the pane, before anything can
attach, replace or close it — the property today's spawn-time typing has. Everything
shell-specific lives in `agent_updates::model` as pure, tested functions.

## Technical design

### Model (`src-tauri/src/agent_updates/model.rs`)

- `UpdateCommand { program: String, program_is_path: bool, args: &'static str }` — what
  Keitora runs, kept structured until the shell is known. `program` is the found npm
  (`/`-separated, `program_is_path = true`) or a bare name (`npm`, `brew`, `claude`,
  `codex`); `args` is the static rest (`install -g @openai/codex@latest`, `upgrade codex`,
  `update`). `program_is_path` is set only when an npm was found beside the install —
  never guessed from the string — so POSIX output for bare commands is unchanged.
- `UpdateCommands { run: Option<UpdateCommand>, guidance: String }`; `guidance` keeps
  today's text exactly (the existing exact-string tests at `model.rs` ~705–746 keep
  passing; only the `run` field's type changes).
- `UpdateCommand::display()` — shown in the dialog ("Running …") and sent on the wire as
  the entry's `command`: program (double-quoted only when it contains whitespace) + args.
  Never typed.
- `ShellFamily { PowerShell, Posix, Fish }`; `ShellFamily::classify(name) ->
  Option<ShellFamily>`: lowercase, strip any directory, a leading `-` (login shells) and a
  trailing `.exe`; `pwsh`/`powershell` → PowerShell; `bash`/`zsh`/`sh`/`dash`/`ksh`/
  `mksh`/`yash`/`ash` → Posix; `fish` → Fish; else `None`.
- `ShellFamily::fallback(windows: bool)` — PowerShell on Windows (herdr's own default
  without `SHELL`), Posix elsewhere. Used when detection fails or the shell is unknown.
- `render_run_line(cmd, family, windows) -> String` (replaces `build_run_command`):
  - **Posix**: `{p} {args}; echo "SENTINEL$?"`, `p` = `posix_quote(program)` when
    `program_is_path`, else bare — byte-identical to today.
  - **Fish**: `{p} {args}; echo "SENTINEL$status"`, `p` = fish single-quote when a path
    (`\` → `\\`, `'` → `\'`), else bare.
  - **PowerShell**: `$LASTEXITCODE = 1; & {p} {args}; echo "SENTINEL$LASTEXITCODE"`, `p` =
    PowerShell single-quote, doubling each of `'` `‘` `’` `‚` `‛` (PowerShell treats the
    curly ones as single quotes too). Seeding `$LASTEXITCODE` makes a command that never
    ran (not found → no native exit) report 1 instead of a stale `0` or an empty sentinel;
    a native command (`npm.cmd`, `claude.exe`) overwrites it. With `windows`, a bare `npm`
    renders as `npm.cmd` so PowerShell does not pick `npm.ps1` (blocked under the default
    execution policy). Verified: parses cleanly in pwsh 7 and Windows PowerShell 5.1.
- `parse_sentinel` unchanged; tests extend to each shell's echoed command line (`…$status"`,
  `…$LASTEXITCODE"`, `PS C:\…> ` prefix) never matching.
- `assess(.., command: Option<String>)` keeps its signature (display text).

### Agent updates (`agent_updates/mod.rs`, `commands.rs`)

- `AgentUpdateEntry` keeps `command: Option<String>` on the wire (display text; the
  frontend only tests it for null — `src/lib/agentUpdates.ts`, fixtures unchanged) and
  gains `#[serde(skip)] run: Option<UpdateCommand>`, which `begin_run` reads.
- `RunTicket` carries the `UpdateCommand`; `RunPlan.command` becomes its `display()`.
- New `AgentUpdates::start_run(&self, backend: &dyn PtyBackend, cli, windows) ->
  Result<RunTicket, String>`, serialized by a `run_lock: Mutex<()>`: steps 1–6 above. On
  any failure after `begin_run` it calls `close()` (bumps the run gen, clears `running`)
  and returns a readable error (`couldn't open the update pane: …` / `couldn't start the
  update: …`). Detection failure is not an error: fallback + `log::info!`.
- `agent_update_run` calls `start_run` with the managed backend and `cfg!(windows)`, then
  `spawn_watcher` as today. The watcher and `watch` are unchanged (same `WATCH_MAX`).
- Stale doc comments describing "the dialog spawns with the command" / "POSIX syntax (Git
  Bash on Windows)" are updated (`commands.rs` `agent_update_run`, `RunPlan`, model docs,
  `RunPlan` in `src/lib/ipc.ts`).

### Backend (`backend/mod.rs`, `herdr/mod.rs`, `herdr/ops.rs`, `herdr/real.rs`, fakes)

Two new `PtyBackend` methods with defaults (other backends untouched):

- `fn pane_shell(&self, key) -> Result<Option<String>, PtyBackendError>` — the process name
  of the session's shell; default `Ok(None)`. herdr: `keymap.get_current(key)` (a pane of
  a replaced server counts as absent → `NoSuchSession`) → `ops.pane_process_info(pane_id)`
  → `ops.process_name(shell_pid)`.
- `fn run_line(&self, key, gen: u64, line: &str) -> Result<(), PtyBackendError>` — type a
  command line into a plain shell session and submit it, only if the session is still at
  `gen`; default unsupported error. herdr: `gate.admit_session(key)`, `get_current(key)`
  whose recorded gen must equal `gen` (else `NoSuchSession`), then `ops.pane_run(pane_id,
  &[line])` (what `spawn` does with `command` today).
- New `HerdrOps::process_name(pid) -> Option<String>` test seam: real impl reuses
  `RealHerdr`'s `sysinfo::System` with a single-pid refresh (as
  `sample_process_stats` does); the herdr fake answers from a scripted map. Keeps herdr
  backend tests off real OS processes.
- `backend::fake::FakeBackend` scripts `pane_shell` and records `run_line` calls so
  `start_run` is tested end to end.

### Frontend (`src/components/AgentUpdateDialog.tsx`, `TerminalPane.tsx`, `ipc.ts`)

- The dialog stops passing `command`; it still shows `step.plan.command` (display text).
- `TerminalPane`'s `command` prop is removed (no other caller); `ptySpawn` is called with
  no command. `pty_spawn`'s backend `command` parameter stays (generic API).
- The attach-first path already finds the backend-created pane; if it is gone (closed by
  the user's daemon restart), the fallback spawn opens a plain shell — harmless.

### macOS

No macOS-specific code: zsh/bash render exactly as today, fish gains a working line, the
sysinfo lookup is cross-platform and reports `zsh`; `classify` also handles `-zsh`.

## Dependencies

None new: `sysinfo` (`features = ["system"]`) is already used by the herdr backend.

## Risks

- **Typing before the shell's prompt is ready.** Same as today (spawn typed straight after
  `workspace create`; the screenshot shows the line arriving intact). herdr queues pane
  input in the pty.
- **Recovery.** A spawn refused during recovery now fails `agent_update_run` immediately
  with the error, instead of the old 15-minute wait (reserved panes are never resumed).
- **`$LASTEXITCODE` with `.cmd` shims.** PowerShell sets it from `cmd.exe`'s exit; npm's
  shim exits with node's code. Covered by the live smoke on Windows.
- **Process name lookup fails** (pid gone, refresh miss) → fallback family, logged.

## Test plan

- Model: classify table (incl. `-zsh`, `C:\…\pwsh.exe`, `bash.exe`, unknown); render per
  family (bare + path, spaces, `'`, curly quotes for PowerShell, `\`/`'` for fish), POSIX
  byte-identical to the old line, `npm` → `npm.cmd` only for Windows PowerShell; display
  text; sentinel matrix for each shell's echo.
- `start_run` (FakeBackend): spawns plain then types once with the spawn's gen; PowerShell
  / bash / fish / unknown→fallback / detection error→fallback; spawn failure and run-line
  failure → Err + run closed (`running` cleared, gen bumped); a refused run (no command)
  spawns nothing.
- herdr backend (fake ops): `pane_shell` names the pane's shell, `NoSuchSession` without a
  pane; `run_line` types via `pane_run`, refuses a stale gen and a gated (recovering) key.
- Updated existing tests: `run_plan_carries_the_sentinel_and_home` (plan command is the
  display text now), `run_command_appends_the_sentinel` (→ render tests).
- `cargo test --workspace`, `yarn test`, `tsc --noEmit`.
- Live smoke (owed to the user): update Codex from the dialog with herdr on PowerShell,
  and from a Git Bash–launched Keitora.

## Refinement log

- **Auto review 1** (independent agent; references verified, PowerShell line parsed in pwsh
  7.6 + 5.1): switched from watcher-polls-then-types to backend-creates-pane-and-types in
  `agent_update_run` (removes the wrong-pane race, type-ahead, and the recovery-refusal
  15-minute hang); `run_line` is gen-checked and gated; `get_current` for both new methods;
  `process_name` seam for deterministic tests; wire `command` stays a display string with a
  `serde(skip)` run field; PowerShell curly-quote escaping; explicit `program_is_path`;
  existing tests that change are named; `TerminalPane.command` prop removed. Dropped: a
  sentinel nonce (fresh pane per run makes it unnecessary).
- **Staging review** (`/simplify`, `/code-review high`): one render template for all
  shells; PowerShell reports `$(if ($?) {0} else {$LASTEXITCODE})` so a function or
  alias that succeeds reads as success (checked in pwsh 7 and 5.1); a known-but-unsupported
  shell (cmd.exe, nu, …) is refused at once instead of the fallback (only an unreadable
  shell falls back); a failed start closes the pane it opened; `close()` takes `run_lock`
  so a close during a start supersedes it before the pane is killed.
