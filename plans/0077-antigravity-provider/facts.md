# Antigravity CLI — verified facts

These were checked on 2026-11-26 against `agy` 1.5.18 and herdr 0.9.5 on Windows 11. Anything marked
**unverified** was not tested here, and the live smoke test must confirm it.

## Binary and install

- **Binary name.** It is `agy`. It is unrelated to `antigravity` on PATH, which is the VS Code-style IDE
  launcher. That launcher prints `1.107.0` and a commit hash, and discovery must never pick it.
- **Windows install location.** The official PowerShell installer puts the binary at
  `%LOCALAPPDATA%\agy\bin\agy.exe`. Verified from the installed file and from
  `install.ps1`: `$TARGET_DIR = Join-Path $env:LOCALAPPDATA "agy\bin"`.
  - The installer does not add that folder to the PATH of already-running processes. A Keitora started
    before the install will not find `agy` on PATH, so discovery must check the folder directly.
- **unix install location.** `install.sh` uses `TARGET_DIR="$HOME/.local/bin"` and `BINARY_PATH=$TARGET_DIR/agy`.
  It accepts `-d/--dir` for a custom location.
- **Installer staging.**
  - Windows: `%LOCALAPPDATA%\antigravity\staging`.
  - unix: `~/.cache/antigravity`.

## Version and updates

- **`agy --version`.** Prints exactly `1.5.18` (a bare semver) on stdout and exits 0.
- **Self-update.** `agy` updates itself in the background on every launch. From `cli.log`:
  `auto_updater.go] Spawned background update process`. The result is written to
  `~/.gemini/antigravity-cli/updater/update_status.json`, for example
  `{"success":true,"message":"Already on the latest version."}`, alongside
  `~/.gemini/antigravity-cli/last_check.timestamp`.
  - We saw no blocking update prompt on launch. That is **unverified** for the case where an update is
    actually pending.
- **`agy update`.** The subcommand exists ("Update CLI") and takes no flags.
- **Release manifest.** The official installers use
  `GET https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/<platform>.json`,
  which returns `{"version":"1.5.18","url":"…","sha512":"…"}`.
  - **Platform keys:** `windows_amd64`, `windows_arm64`, `darwin_amd64`, `darwin_arm64`, `linux_amd64`,
    `linux_arm64`, plus `linux_<arch>_musl` on musl.
  - `windows_amd64` and `darwin_arm64` were fetched and both say 1.5.18.
  - A hyphenated key (`windows-amd64`) returns 404.

## CLI flags (`agy --help`, 1.5.18)

| Purpose | Flags |
|---|---|
| Resume by id | `--conversation <id>`, with aliases `-r` and `--resume` |
| Continue the most recent conversation | `-c` / `--continue` |
| Initial prompt, interactive | `-i` / `--prompt-interactive` |
| Single prompt, non-interactive | `-p` / `--print` / `--prompt`, with `--output-format text\|json\|stream-json` and `--json-schema` |
| Auto-approve all tool permission requests | `--dangerously-skip-permissions` |
| Execution mode | `--mode accept-edits\|auto\|plan` |
| Sandbox | `--sandbox` |
| Session-only settings overlay, never saved | `--settings <json or path>` |
| Other | `--model`, `--effort`, `--agent`, `--add-dir`, `--project`, `--new-project`, `--log-file`, `--remote-control`, `--remote-agent` |

**Subcommands:** `acp`, `agent(s)`, `changelog`, `help`, `install`, `mcp`, `models`, `plugin(s)`,
`remote-control`, `update`, `mic-serve`.

**Headless output.** `-p … --output-format json` returns
`{"conversation_id":"<uuid>","status":"SUCCESS","response":"…",…}`.

## On-disk state (`~/.gemini/antigravity-cli/`)

- **Per conversation.** Each conversation gets `brain/<conversationId>/` (also the `artifactDirectoryPath`)
  and `conversations/<conversationId>.db`. Conversation ids are lowercase UUIDs.
- **Transcript.** The hook payload's `transcriptPath` is
  `brain/<id>/.system_generated/logs/transcript_full.jsonl`. Google's docs say `transcript.jsonl`; trust
  the payload.
- **Settings.** `settings.json` holds user settings such as `colorScheme`.
  - The settings struct has a `trustedWorkspaces` key. We verified it in the binary's strings; it is a
    JSON and proto field.
  - The element shape (string paths, or objects) is **unverified**, because neither form produced a parse
    error.
  - A malformed `settings.json` is logged and ignored, and `agy` falls back to defaults. It does not crash.
- **Logs.** `cli.log` and `log/cli-<timestamp>.log`.
- **Shared config.** `~/.gemini/config/config.json` holds permission grants shared with the Antigravity
  app. `~/.gemini/config/mcp_config.json` holds MCP servers.

## Hooks (verified with a probe workspace and `agy -p`)

**Load locations.** `~/.gemini/config/hooks.json` (global) and `<workspace>/.agents/hooks.json`. The log
line is `hooks_manager.go] loaded N named hooks from M hooks.json file(s)`.

**File shape.** A JSON object keyed by *hook name*. Each name maps to an object keyed by event, plus an
optional `"enabled": false`.
- **Non-tool events** (`SessionStart`, `PreInvocation`, `PostInvocation`, `Stop`) take an array of
  handlers directly:
  `"Stop": [ {"type":"command","command":"…","timeout":10} ]`.
- **Tool events** (`PreToolUse`, `PostToolUse`) take matcher groups:
  `"PreToolUse": [ {"matcher":"*","hooks":[ {handler} ]} ]`.
- If you wrap non-tool events in `{"hooks":[…]}`, the file fails to parse with
  `command hook must specify 'command'`. When that happens the whole file is ignored, and `agy` keeps
  running.

**Windows shell.** Hook commands run under **cmd.exe**: `%COMSPEC%` expands, while `$PSVersionTable` and
`$0` stay literal. The environment includes `PSModulePath`, and `SHELL` is inherited from the parent.

**Events that fire for one prompt with one tool call**, in order: `SessionStart` → `PreInvocation` →
`PreToolUse` → `PostToolUse` → `PostInvocation` → `PreInvocation` → `PostInvocation` → `Stop`.

**Payload.** It arrives on stdin as JSON with camelCase keys.
- **Common fields:** `conversationId`, `cwd`, `hookEventName`, `workspacePaths`, `transcriptPath`,
  `artifactDirectoryPath`, `modelName` and `lastUserInput`.
- `PreInvocation` and `PostInvocation` add `invocationNum` and `initialNumSteps`.
- `Stop` adds `executionNum`, `terminationReason` (seen: `"NO_TOOL_CALL"`), `error`, `fullyIdle` (seen:
  `true`) and `continuationActive`.

**Events that do not exist.** There are no `UserPromptSubmit`, `Notification`, `PermissionRequest` or
`SessionEnd` hook events. The strings appear in the binary, but the docs list only the six events above.

**Trust.** Workspace hooks loaded in `-p` mode without any trust prompt. Whether the interactive TUI
gates them is **unverified**.

## herdr 0.9.5

- **Launch.** `herdr agent start --kind agy` is a supported kind, and herdr resolves the canonical
  executable `agy`.
- **Integration install.** `herdr integration install antigravity-cli` installs
  `~/.gemini/config/hooks/herdr-agent-state.ps1`. It is not installed on this machine yet.
- **Session source string.** The herdr binary contains `herdr:antigravity_cli`, so we expect
  `agent_session.source == "herdr:antigravity_cli"` with agent `antigravity_cli`. This is **unverified**
  live, so `cli_of` should accept `antigravity_cli`, `antigravity-cli` and `agy`.
