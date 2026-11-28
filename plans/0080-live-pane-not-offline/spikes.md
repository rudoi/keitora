# 0080 spikes

Part of 0080; see `00-overview.md`.

All live spikes run on this Windows machine through the isolated launcher (`yarn dev:isolated`,
`scripts/dev-herdr.mjs`) after Phase 0 lands (`01-phase0-isolation.md`). They must never touch
the user's running herdr. Spikes 1-3 and 5 are blocking for the phases that use them. Spike 2
is not blocking. Spike 4 (Codex `-c notify`) is dropped: D6.

Results go in **`plans/0080-live-pane-not-offline/spike-results.md`** (create it at the first
spike; template at the end). Raw captures stay under the dev home (`$DEV_HOME`, below) and are
never committed. Fixtures and results use placeholder names only (`alpha`, `my-game`,
`C:/Users/x`).

Open question for the human before the first live spike: Spikes 1, 3 and 5 launch real Claude,
Codex and agy sessions in the dev app. They use real agent logins and quota, write transcripts
into the real `~/.claude/projects` and Codex sessions dirs (for throwaway dev repo paths only),
and keep the machine busy for roughly 30-60 minutes. OK to run them while the main herdr is in
use, or wait for a quiet window? **Do not start a live spike until the human has said yes.**

## Conventions

- **Shell:** Git Bash on Windows. Where a Windows-only query is needed, call PowerShell as
  `powershell.exe -NoProfile -Command "..."`. Set `MSYS_NO_PATHCONV=1` for any command that
  passes a `/x` style argument.
- **"Clean terminal":** a terminal opened from the Start menu (Git Bash or Windows Terminal),
  not one hosted by Keitora or herdr. `env | grep '^HERDR_'` there must print nothing.
- **"Dev pane":** a shell pane inside the dev app started by `yarn dev:isolated`. If it opens
  PowerShell, run `bash` first so the commands below work as written.
- Variables used below (set in each terminal that needs them):

  ```bash
  REPO=/d/repos/keitora            # this checkout (use the worktree you run the spikes from)
  DEV_HOME="$(cd "$REPO" && node -e "import('./scripts/dev-herdr.mjs').then(m=>console.log(m.devHomeFor()+'-herdr'))")"
  # DEV_HOME is the dev app's KEITORA_HOME. Its herdr state is "$DEV_HOME/herdr"; its socket
  # pointer is "$DEV_HOME/herdr/config/herdr/herdr.sock"; its app log is "$DEV_HOME/logs/keitora.log".
  ```

  (`devHomeFor` is exported and importing `dev-herdr.mjs` does not start the launcher.)
- **Dev-app limits that matter here:** the launcher sets `KEITORA_HERDR_NO_INTEGRATION=1`.
  That turns off herdr's `integration install` **and** Keitora's global writers (agy hooks,
  agy trust), so agy hook behaviour and herdr's session-id hooks cannot be tested through the
  dev app; Spike 3b covers agy in a scratch config dir. Claude and Codex project-file hooks
  are not global writes and are installed by the dev app as usual.
- **Autonomy** (permission prompts on or off) is a Keitora setting per provider
  (Settings, providers section). Spike steps that need prompts say "autonomy off".
- Run the isolation re-check below (`s0_check`) at the start and the end of every spike
  session and after any step that starts or stops herdr.

```bash
# paste into every terminal that runs spike steps (after DEV_HOME is set)
s0_check () {
  echo "--- real herdr processes (must match the S0 step-2 baseline) ---"
  powershell.exe -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='herdr.exe'\" | Select-Object ProcessId,ParentProcessId,CreationDate | Format-Table -HideTableHeaders"
  echo "--- sockets (mtime only) ---"
  ls -l --time-style=full-iso "$HOME/.keitora/herdr/config/herdr/herdr.sock" 2>&1 | awk '{print $6, $7, $9}'
  ls -l --time-style=full-iso "$DEV_HOME/herdr/config/herdr/herdr.sock" 2>&1 | awk '{print $6, $7, $9}'
}
```

## Gating summary

| Spike | Gates | Blocking |
|---|---|---|
| S0 | every live spike (run first, every session) | yes |
| S1 | Phase 1 (`02-engine-core.md`), Phase 2 (`03-turns-and-cli.md`); also Phase 4 bypass priority | yes |
| S2 | nothing (push follow-up only) | no |
| S3 | Phase 4 Claude part (`05-hook-placement.md`) | yes |
| S3b | Phase 4 agy part (config-dir override, re-assert) | no (decides one detail; default branch exists) |
| S5 | Phase 5 (`06-reply-fallbacks.md`); the agy part (S5-agy) gates s27b | yes (agy part: decides s27b only) |

## Global abort criteria (all spikes)

Stop, close the dev window, run `s0_check`, record what happened, and tell the human if any of
these occur:
- a step-5 isolation check in Spike 0 fails, or a later `s0_check` shows a real-herdr process
  gone or a real socket mtime changed;
- any command prints or touches a path under the real `~/.keitora/herdr`, `%APPDATA%`/personal
  herdr dirs, or `~/.claude`/`~/.codex`/`~/.gemini` **writes** other than the transcripts and
  trust entries the agent itself makes for throwaway paths;
- an agent hits a login prompt, a usage limit or a paid-plan warning (do not enter credentials
  or switch accounts; record and stop that provider);
- the session exceeds 90 minutes, or the human asks to stop.

## Spike 0: isolation gate (run first, every session)

Steps marked **[clean]** run from a clean terminal, **[dev pane]** inside the dev app.

1. **[clean]** Confirm no inherited herdr env:
   `env | grep -i '^HERDR_' ; echo "exit=$?"` must print nothing (exit 1). Abort if it prints
   anything and Phase 0 has not landed; if Phase 0 has landed, continue (the launcher strips
   them) but record the names (names only, never values).
2. **[clean]** Record the real servers (read-only, no isolation env):
   - processes: `s0_check` (save the printed process list as the **baseline**);
   - the server herdr itself reports from a clean terminal:
     `herdr status server --json` (record `running`, `pid`, `socket`; this is the "real" socket
     the dev app must never touch; also note whether it equals `~/.keitora/herdr/...`).
   Do not print or open socket-pointer files.
3. **[clean]** Start the dev app: `cd "$REPO" && RUST_LOG=info yarn dev:isolated`. Expect the
   launcher lines `stripped inherited herdr env: ...` (if any) and the build. **Abort** on
   `refusing to launch` (either the first check or the re-check): record the reason text.
4. **[clean]** After the window opens, read the boot classification:
   `grep "at launch the server is" "$DEV_HOME/logs/keitora.log" | tail -1`. It must end with
   `: Fresh`. **Abort** on `Adopt`, `RecoverDifferentInstance` or `Indeterminate`: close the
   window; the launcher refuses to stop a server it cannot prove is its own.
5. **[clean]** With the window up:
   - `ls "$DEV_HOME/herdr/config/herdr/herdr.sock"` exists; `cat` it only if you need the pid
     field (it is a `<pid>:<nonce>` pointer; print the pid part only). That pid must be a
     `herdr.exe` **not** in the step-2 baseline:
     `powershell.exe -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='herdr.exe'\" | Select-Object ProcessId,ParentProcessId,CreationDate"`;
   - the real socket mtime is unchanged (compare with `s0_check` at step 2);
   - every baseline pid is still alive.
   Abort and close the dev window if any check fails.
6. **[dev pane]** Before any probe: `echo "$HERDR_SOCKET_PATH"` must point under `$DEV_HOME`
   (compare the printed path to `$DEV_HOME` by eye). Abort if it does not. `echo "$KEITORA_HOME"`
   must equal `$DEV_HOME` as well.
7. Teardown: close the window (or Ctrl+C the launcher). Expect `herdr server stop` for the
   isolated server only. **[clean]** re-run `s0_check`; the baseline processes and the real
   socket mtime must be unchanged and the dev socket's server gone.

Record in `spike-results.md`: the verdict (pass/fail), the herdr version (`herdr --version`),
the boot class line, and any `stripped ...` names.

## Spike 1: herdr `agent list` semantics and latency (blocking for Phases 1-2)

Run after S0 passes, in the dev app, in a demo worktree (the launcher seeds throwaway projects
under `$DEV_HOME/demo`; use one of them, call it `alpha`).

**Probe (dev pane; start in a separate pane and leave running):**

```bash
OUT="$KEITORA_HOME/spike1.ndjson"
mark () { printf '{"mark":"%s","t":%s}\n' "$1" "$(date +%s.%N)" >> "$OUT"; }
while true; do
  printf '{"t":%s,"agent_list":%s}\n' "$(date +%s.%N)" "$("$HERDR_BIN_PATH" agent list | tr -d '\n')" >> "$OUT"
  sleep 0.25
done
```

Second pane, every 2 s, for the `pane list` `done` question:
`while true; do printf '{"t":%s,"pane_list":%s}\n' "$(date +%s.%N)" "$("$HERDR_BIN_PATH" pane list | tr -d '\n')" >> "$KEITORA_HOME/spike1.ndjson"; sleep 2; done`.

(If `herdr agent list` output is not single-line JSON in 0.9.5, store it raw and note that in
the results.)

Call `mark <label>` from a third pane at each step so the capture is segmented.

**Per provider (claude, codex, agy), launched from the dev UI in `alpha`:**

1. a fresh launch to ready (`mark launch`);
2. a short prompt ("reply with ok"); (`mark prompt1`)
3. a tool turn ("list the files in this directory"); (`mark tool`)
4. a permission prompt, autonomy off ("create a file named x.txt"); approve it; (`mark perm`)
5. an interrupt mid-turn (long prompt, press Esc/Ctrl+C after 2 s); (`mark interrupt`)
6. `/clear`; (`mark clear`)
7. two prompts back to back (second submitted while the first runs); (`mark double`)
8. focusing the pane after a turn finishes unfocused (switch to another pane before the turn
   ends, then focus it: `done` -> `idle`); (`mark focus`)
9. **agy only:** a prompt that starts background work ("start a background task that sleeps
   30 s, then report") ; (`mark bg`)
10. **codex only:** one launch without `--dangerously-bypass-hook-trust`, in a dev pane:
    `"$HERDR_BIN_PATH" agent start spike-codex --kind codex --pane "$HERDR_PANE_ID" --timeout 20000 -- -c check_for_update_on_startup=false`
    then run steps 1-2; record whether `agent_session` appears in `agent list` (`mark nobypass`).
    (If `agent start` refuses a pane already showing a shell prompt, open a fresh shell pane
    and use its `$HERDR_PANE_ID`.)

**Record (per provider, in a table):**
- `completion_seq` before the first turn, after each turn, on an interrupt, on blocked -> idle,
  and after `/clear`; classify it with this rubric and write the label:
  - **counter:** non-null, rises by exactly 1 per working -> idle, never falls;
  - **transition-id:** non-null after a completed turn and equal to (or tracking) the
    `state_change_seq` of that idle transition; may change without +1;
  - **null-when-idle-without-work / absent:** null or missing at least once while idle.
- its relation to `state_change_seq` (equal? greater? independent?);
- whether `launch_pending` and `interactive_ready` are present, and their values from launch to
  ready;
- latency from the `mark prompt1` instant to the first `working` snapshot, and from the visibly
  finished screen (note wall-clock from your own observation, or the terminal bell if any) to
  `idle`; report min/median over steps 2, 3, 7;
- whether `pane list` ever reports `done` (grep the pane-list lines);
- steps 5, 6, 7: does an interrupt produce a `completion_seq` change? does `/clear` change
  `agent_session`? do two quick prompts yield one or two completions?

**Abort criteria (S1):** an agent asks to log in; a probe loop hangs `agent list` for more
than 10 s (kill the loops, record, stop); herdr or the dev app crashes (record the log tail).

## Spike 2: push transport (not blocking)

Run only if the human wants it; skipping leaves the polling design final.

- Read herdr 0.9.5's socket code (public source, read-only) for the Windows pipe naming behind
  the pointer file. No commands needed.
- Run a scratch client (outside the repo, in `$DEV_HOME/spike2/`) against the **isolated**
  server only. First assert `"$HERDR_SOCKET_PATH"` is under `$DEV_HOME` (S0 step 6). Subscribe
  to global `pane.updated`, `pane.agent_detected` and `pane.exited`, plus one per-pane
  `pane.agent_status_changed`.
- Record whether `pane.updated` fires on every status change, the reconnect behaviour after
  `herdr server stop`/start of the isolated server, and `events_lost`.
- **Abort:** if the client cannot prove it is connected to the isolated socket (path or pid not
  in the dev home), stop; never fall back to the default socket.

## Spike 3: Claude per-launch hooks (blocking for Phase 4, Claude part)

Goal: decide whether `claude --settings <file>` hooks fire in a never-trusted worktree, so
Phase 4 can move Claude hooks off the project file.

Setup **[clean]** (no Keitora involvement):

```bash
SCRATCH=/d/scratch/spike3                       # placeholder; any new empty dir outside any registered project
mkdir -p "$SCRATCH/wt-untrusted" "$SCRATCH/wt with space"
git -C "$SCRATCH/wt-untrusted" init -q && git -C "$SCRATCH/wt with space" init -q
LOG="$SCRATCH/spike3.log"; : > "$LOG"
cat > "$SCRATCH/spike3.json" <<EOF
{ "hooks": {
  "SessionStart":      [ { "matcher": "", "hooks": [ { "type": "command", "command": "echo \"\$(date +%s) launch SessionStart \$PWD\" >> $LOG" } ] } ],
  "UserPromptSubmit":  [ { "matcher": "", "hooks": [ { "type": "command", "command": "echo \"\$(date +%s) launch UserPromptSubmit\" >> $LOG" } ] } ],
  "Stop":              [ { "matcher": "", "hooks": [ { "type": "command", "command": "echo \"\$(date +%s) launch Stop\" >> $LOG" } ] } ],
  "PermissionRequest": [ { "matcher": "", "hooks": [ { "type": "command", "command": "echo \"\$(date +%s) launch PermissionRequest\" >> $LOG" } ] } ]
} }
EOF
```

Steps **[dev pane]** (shell pane in the dev app so `HERDR_*`/`KEITORA_*` are the isolated ones;
`cd` to the scratch dirs; these directories are *not* registered in Keitora, so Keitora writes
no hooks there):

1. **Untrusted, before the dialog.** `cd "$SCRATCH/wt-untrusted" && claude --settings "$SCRATCH/spike3.json"`.
   The trust dialog should appear. Do **not** accept yet. Record `SessionStart` in `$LOG`
   (present/absent). (`cat "$LOG"`.)
2. **After accepting trust.** Accept; send "reply with ok"; record the `UserPromptSubmit` and
   `Stop` lines. Then, with autonomy off (launch without `--dangerously-skip-permissions`; do
   not add it), ask "create a file x.txt" and record `PermissionRequest`. Exit claude.
3. **Declined trust.** In a fresh dir (`mkdir "$SCRATCH/wt2"`), launch the same way, answer
   "no / exit" at the dialog; record whether any hook fired.
4. **Coexistence.** (a) herdr's user-level `SessionStart` still fires: if herdr's Claude
   integration is installed on this machine (`herdr integration status` from a clean terminal,
   read-only), in the dev pane after step 2 `"$HERDR_BIN_PATH" agent list` should show an
   `agent_session` for a herdr-managed agent; if it is not installed, record "not testable".
   (b) **Double fire:** create `$SCRATCH/wt-untrusted/.claude/settings.local.json` with a
   `Stop` hook that appends `project Stop` to the same log, plus a second `Stop` entry that is
   guarded: `[ -z "$KEITORA_LAUNCH_HOOKS" ] && echo project-guarded >> $LOG`. Launch once
   normally with `--settings` (and `export KEITORA_LAUNCH_HOOKS=1` first), once without the env
   var; record which lines each run writes.
5. **Inline JSON through `agent start` (quoting).** In a dev pane:
   `"$HERDR_BIN_PATH" agent start spike3-json --kind claude --pane "$HERDR_PANE_ID" --timeout 20000 -- --settings '{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"echo json-stop >> '"$LOG"'"}]}]}}'`
   then prompt it and check `$LOG` for `json-stop`. Record whether the JSON survived (claude
   starts without a settings parse error) and what the pane shell was (PowerShell/cmd/bash).
6. **File path through `agent start`, plain and with a space.**
   `"$HERDR_BIN_PATH" agent start spike3-file --kind claude --pane "$HERDR_PANE_ID" --timeout 20000 -- --settings "$SCRATCH/spike3.json"`
   (plain path); repeat with the file copied to `"$SCRATCH/wt with space/spike3.json"`. Record
   fired / not fired / launch error for each.
7. Clean up: close the spike panes (closing the pane in the
   UI is enough).

**Record:** a table of (scenario -> hooks fired). Specifically: fires before trust accepted?
after? with trust declined? coexists with herdr's user hook? project-file hooks also fire
(double fire)? guard works? inline JSON works? plain path works? path with a space works?

**Abort criteria (S3):** claude shows a login prompt; the trust dialog cannot be dismissed;
`agent start` leaves the dev pane unusable (close it, record).

## Spike 3b: agy hooks file vs herdr's integration install (non-blocking)

Question: does `herdr integration install antigravity-cli` keep or drop Keitora's `keitora`
key, and does `ANTIGRAVITY_CLI_CONFIG_DIR` relocate the file? **[clean]** terminal only; no dev
app needed; touches no running server.

1. Back up the real file first and remember its hash (do not print contents):
   `cp "$HOME/.gemini/config/hooks.json" "$SCRATCH/real-hooks.backup.json" 2>/dev/null; sha256sum "$HOME/.gemini/config/hooks.json" 2>&1 | tee "$SCRATCH/real-hooks.sha"`.
2. Make a scratch config dir and seed a `keitora` key and a foreign key:
   `CFG="$SCRATCH/agy-config"; mkdir -p "$CFG"; printf '{"keitora":{"SessionStart":[{"type":"command","command":"echo keitora-marker"}]},"foreign":{}}\n' > "$CFG/hooks.json"`.
3. Run the install with the override and a scratch `HOME`/`USERPROFILE` as a second guard:
   `ANTIGRAVITY_CLI_CONFIG_DIR="$CFG" HOME="$SCRATCH/home" USERPROFILE="$SCRATCH/home" herdr integration install antigravity-cli`
   (if it errors because `agy` is not installed, record that and stop this spike).
4. Record: did `"$CFG/hooks.json"` change? is the `keitora` key still byte-identical? is the
   `foreign` key kept? was a `herdr` key added? Did anything appear under `$SCRATCH/home`?
5. **Safety check:** `sha256sum "$HOME/.gemini/config/hooks.json"` must equal the step-1 hash.
   **If it differs, restore the backup immediately** (`cp "$SCRATCH/real-hooks.backup.json" "$HOME/.gemini/config/hooks.json"`),
   record that the override was not honoured, and tell the human.

**Abort:** the real file hash changed (restore, record, stop).

## Spike 5: transcript fixtures (blocking for Phase 5)

- From the Spike 1 runs, copy the Claude `.jsonl` (under `~/.claude/projects/<encoded dev repo
  path>/`) and Codex rollout files (under `~/.codex/sessions/...`) for the **dev repo paths
  only** into `$DEV_HOME/spike5/raw/`. Never copy transcripts from other paths.
- Scrub into `$DEV_HOME/spike5/scrubbed/`: replace the dev repo path and the user's home with
  `C:/Users/x/...` / `/Users/me/...` placeholders, replace any prompt text that is not the
  scripted prompts with `<redacted>`, drop auth/token fields. The Phase 5 implementer commits
  only scrubbed files, after a second read-through.
- Cover a plain turn, a tool turn, a denied permission, an interrupt, `/clear`, compaction
  (`/compact`) and a Codex resume (`codex resume <id>`).
- Confirm Claude's final-line `stop_reason` and Codex `task_complete`/`turn_aborted` (one line
  each in the results with the exact field paths).

**Abort:** a transcript contains something that cannot be scrubbed with confidence (a secret,
unrelated content): do not copy it; record the gap.

### S5-agy: Antigravity session files (gates the agy reader stage)

Run in the isolated dev app, reusing the scratch config dir from Spike 3b
(`ANTIGRAVITY_CLI_CONFIG_DIR` set to a directory created by the spike). Run one agy session in
a dev-repo worktree with two scripted prompts (plain turn, then a tool turn).

- Find where agy writes its transcript or session files: list only paths created or modified
  during this spike (diff a before/after listing of the scratch config dir and, if the variable
  is not honoured, of the default agy/gemini state dirs, **listing paths and sizes only**).
  Never open or read anything in the user's real agy/gemini dirs that this spike did not create.
- Record the file format: one file or many, JSON or JSONL, turn boundaries, where the assistant
  text sits, timestamps (per line or per file), whether tool output is interleaved.
- Record whether herdr's agy `agent_session` id (recorded by herdr's agy integration hook,
  visible in `agent list`) maps to exactly one file (name or content contains the id), whether
  it stays stable across a second prompt, and whether files are shared across workspaces (run a
  second agy session in another dev worktree and check that the first session's file is not
  reused or appended to).
- Scrub copies into `$DEV_HOME/spike5/scrubbed/agy-*` with the same rules as above.

**Abort:** the files cannot be scrubbed with confidence, or the only way to find them is to read
the real agy/gemini dirs: record the gap and take the "unreliable" row. Privacy: copy only files
this spike's session created; never commit raw files; record only names, shapes and field
paths in `spike-results.md`.

Observations for `spike-results.md`: file root path pattern (placeholdered), format, turn
boundary marker, assistant text field path, `agent_session` id present y/n, id-to-file mapping
unique y/n, stable across prompts y/n, shared across workspaces y/n, config-dir honoured y/n.

## Optional, user-run: macOS reproduction checklist for A1 (agy shows offline on a Mac)

Not a gated spike and not run by an agent: all gated spikes run on Windows (D1), so report A1
(a live agy chat reads grey/offline on macOS) has never been reproduced on a Mac. If the
human has a Mac and a few minutes, this checklist collects the facts that decide which half of
A1 applies there (silent hooks vs the missing herdr presence the engine now covers). It is
read-only except step 6, uses the installed app (not the isolated launcher; nothing here starts
herdr or a dev app), and its results go into `spike-results.md` under "A1 macOS (optional)".
Paths below are placeholders; scrub before pasting anything.

1. **Where the app runs from.** Note whether Keitora was launched from `/Applications` or from
   the downloaded `.dmg`/`Downloads`. In a terminal: `ps -o command= -p $(pgrep -x keitora | head -1)`
   and look for `/private/var/folders/.../AppTranslocation/...` in the path. Record yes/no
   (`translocated`, feeds `AgyFacts.translocated` and the `agy_plumbing` message in `07`).
2. **The hooks file.** `jq 'keys, .keitora' ~/.gemini/config/hooks.json` (do not paste other
   keys' contents). Record: is there a `keitora` key; does its `command` embed an absolute
   path that still exists (`ls -l` the path); does it still use the `$KEITORA_BIN` form
   (feeds `AgyFacts.key_present` / `path_ok`). Also `ls -l ~/.gemini/config/hooks.json`: did its
   mtime change after the last herdr start (herdr's integration install rewriting it, S3b).
3. **`KEITORA_BIN` in the pane.** In the affected agy worktree's pane shell (not inside agy):
   `echo "KEITORA_PANE=${KEITORA_PANE:+set} KEITORA_BIN=${KEITORA_BIN:-unset}"` and
   `[ -x "$KEITORA_BIN" ] && echo executable`. Record set/unset (feeds
   `AgyFacts.marker_env_ok`). Then, from the same pane, run the hook by hand:
   `"$KEITORA_BIN" session idle; echo exit=$?` (or the `keitora` on PATH) and re-open the lane
   in the rail: did it flip from grey to idle? (a hook that works by hand but not from agy =
   the env or the path, not the CLI.)
4. **Hook log.** If Phase 4 has landed: `tail -n 30 "<KEITORA_HOME>/logs/hooks.log"` (path from
   Settings > Health). Before Phase 4, note that no log exists and the hooks discard stderr.
5. **First-prompt behaviour.** In a fresh agy worktree: open the lane, wait for the chat to be
   ready, and note the rail state before typing (grey/offline expected while no hook has run),
   then type one short prompt and note whether the rail changes after the first prompt, after
   the answer, or never. Record the three states with timestamps. "Never" with a working hand
   run in step 3 points at agy not invoking the hook; "after the first prompt" matches silent
   SessionStart hooks.
6. **(Only with the human's OK; changes one thing.)** Launch agy once from a terminal that has
   `KEITORA_BIN` exported to the absolute path, in the same worktree, and repeat step 5.
   Record whether the rail now follows the chat. This isolates `KEITORA_BIN` as the cause.
7. **herdr's view.** `herdr agent list` (read-only; the installed herdr) and note, for that
   pane, `agent`, `agent_status`, `interactive_ready` and `agent_session`. This is what the
   engine reads instead of hooks, so it must say `agy` and a non-`unknown` status for the fix to
   help; if not, that is a separate finding.

What the results feed: step 1-3 -> `AgyFacts` fields and the `agy_plumbing` wording in
`05-hook-placement.md` section 8 and `07-self-diagnostics.md` (the translocated remedy text,
the absolute-path rule); step 5 -> whether the hook-side repro
`repro_a1_agy_hook_command_is_inert_without_keitora_bin` matches the Mac behaviour; step 7 ->
`repro_a1_agy_live_without_a_row_reads_idle_and_send_not_refused` is the right fixture. If a step
shows a cause the plan does not cover, add it to `spike-results.md` and tell the human before
Phase 4.

## What each spike result changes

Each row names the design branch the phases take. Phase numbers: 1 = `02-engine-core.md`,
2 = `03-turns-and-cli.md`, 4 = `05-hook-placement.md`, 5 = `06-reply-fallbacks.md`.

| Spike result | Phase 1 | Phase 2 | Phase 4 | Phase 5 |
|---|---|---|---|---|
| **S0 fails** (adopts, a second server is missing, or a real pid/socket changed) even after Phase 0 | Build on the fake backend with schema-derived semantics | Same; keep every predicate clause enabled | Fake-host tests only | Use only the existing captured fixtures, if any |
| **S1: `completion_seq` is a counter** (+1 per working->idle) | no change | Clause 1 is primary; clauses 2-3 stay as fallbacks | - | - |
| **S1: transition-id** (changes, not +1) | no change | No change: "differs from baseline and non-null" already handles it; document it | - | - |
| **S1: absent/null on some provider** | Diagnostics note it | That provider uses clauses 2-3 only | - | - |
| **S1: submit->`working` latency > 1 s** | Hot cadence stays 1 s | The send-settle hold and the "no activity" stall stay at 30 s | - | - |
| **S1: `interactive_ready` absent** | Gate on `launch_pending` and a non-`unknown` status only | - | - | - |
| **S1: `pane list` reports `done`** (or never) | Record; the engine uses `agent list` either way | - | - | - |
| **S1: agy background work shows `idle`** | Keep the busy veto | `turn_finished.caveat` stays | - | - |
| **S1: Codex without the bypass loses `agent_session`** | Resume diagnostics name it | - | Raise the bypass fix to first in the phase | - |
| **S1: `/clear` changes `agent_session`** | Engine treats an `agent_session` change as identity change (baseline invalid) | Baseline invalidated, outstanding turn ends `stalled: agent restarted` | Stale-writer guard keys on it as designed | - |
| **S2: stable transport + global status event** | - | - | - | Push becomes a follow-up task; not in this PR |
| **S2: no stable transport** | Polling final; record in herdr notes | - | - | - |
| **S3: per-launch hooks fire in untrusted worktrees** (before and after trust) | - | - | **Branch A:** implement section 2 (`--settings` file + `KEITORA_LAUNCH_HOOKS` guard); the project file stays as the hand-started fallback | - |
| **S3: fire only after trust is accepted** | - | - | **Branch B:** keep project-file hooks, installed on every launch and provider change; skip `--settings` and the guard; diagnostics note the trust requirement. (Both sources need trust, so `--settings` adds nothing) | - |
| **S3: do not fire, or the plain path fails to launch** | - | - | **Branch B** as above | - |
| **S3: both copies fire and the guard works** | - | - | Branch A with the guard exactly as specified | - |
| **S3: both copies fire and the guard does not suppress** | - | - | Branch B (do not ship double-firing hooks) | - |
| **S3: path with a space fails, plain works** | - | - | Branch A, keep the plain-argument rule | - |
| **S3: inline JSON works on Windows and macOS shells** | - | - | Still use the file path (one code path); note the result for a follow-up | - |
| **S3b: `keitora` key survives herdr's install** | - | - | Re-assert stays (cheap); mark it defensive | - |
| **S3b: install drops or rewrites the key** | - | - | Re-assert is required; keep as specified | - |
| **S3b: `ANTIGRAVITY_CLI_CONFIG_DIR` relocates the file** | - | - | Keep `hooks_file_in` override | - |
| **S3b: the variable is not honoured** | - | - | Drop the override; keep `~/.gemini/config/hooks.json` only | - |
| **S5: Claude final line lacks a reliable `end_turn`** | - | - | - | The Claude transcript is used only for reply text after a herdr- or hook-detected completion, never as a completion signal (already the design) |
| **S5: Codex `task_complete`/`turn_aborted` absent or unstable** | - | - | - | Rollout parsing limited to the last assistant message; no completion use |
| **S5-agy: `agent_session` id maps to one file, stable, not shared across workspaces** | - | - | - | agy mapping reliable: build the agy reader stage (s27b, `06-reply-fallbacks.md` agy section) |
| **S5-agy: no id, ambiguous or shared files, unscrubbable, or not found without reading real dirs** | - | - | - | agy stays `reply_source: none` plus the `keitora read` hint, documented; s27b commits only docs and tests asserting that |

If a result is ambiguous, record it as such, take the more conservative branch (the one that
keeps more fallbacks) and say so in `spike-results.md`.

## `spike-results.md` template

```
# 0080 spike results

Run on: <date>   Machine: Windows 11   herdr: <version>   Keitora commit: <sha>

## S0 isolation gate
verdict: pass|fail     boot class: <line>     stripped env names: <names|none>
baseline herdr pids: <...>     real socket mtime before/after: <...>
notes:

## S1 agent list
| provider | completion_seq label | relation to state_change_seq | launch_pending / interactive_ready | submit->working (min/med) | finish->idle (min/med) | pane list `done`? | notes |
(blank rows per provider)
codex without bypass: agent_session present? <yes/no>
agy background work: status during work: <idle/working>
/clear changes agent_session? <yes/no per provider>

## S2 push (skipped | results)

## S3 Claude --settings
| scenario | fired? |
(before trust / after trust / declined / coexist herdr hook / double fire / guard / inline JSON / plain path / path with space)

## S3b agy
config-dir honoured? <y/n>   keitora key survived? <y/n>   foreign key kept? <y/n>   real file hash unchanged? <y/n>

## S5 transcripts
fixtures captured: <list of scrubbed file names>   Claude end-of-turn field: <path>   Codex fields: <paths>
agy (S5-agy): root pattern: <path>   format: <...>   turn boundary: <...>   assistant text field: <path>   agent_session id present? <y/n>   id->file unique? <y/n>   stable across prompts? <y/n>   shared across workspaces? <y/n>   config-dir honoured? <y/n>   verdict: <reliable|unreliable>

## Decisions taken (one line per row of the decision table that applied)
```

## Singletonize decisions

- **Dev app cannot test global writers or herdr integration** (`KEITORA_HERDR_NO_INTEGRATION=1`
  by design). Added Spike 3b with a scratch config dir and a hash check on the real file
  instead of weakening the launcher's guard.
- **S3 tests the file-path form first** and the inline JSON form second, because the design
  (`05-hook-placement.md`) uses a file path; inline JSON is recorded only for a possible
  follow-up.
- **S3 also tests the double-fire guard**, because Phase 4's design depends on it.
- **Clean-terminal vs dev-pane split** is explicit per step: only S0 steps 6 and every probe
  use the dev pane; everything that proves isolation runs outside Keitora.
- **Ambiguous results take the conservative branch** (more fallbacks kept).
- **`completion_seq` classification rubric** added so "counter" vs "transition-id" is decided
  the same way by whoever runs the spike.
