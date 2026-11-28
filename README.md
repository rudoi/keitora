# Keitora

A desktop app for running several Claude Code sessions at once, one per git worktree, from a
single window.

![Keitora with a Claude Code session open, and each project's worktrees, tasks and status in the sidebar](docs/images/focus.png)

If you keep a few worktrees of a repo and a Claude Code session in each, you probably also keep a
pile of terminal tabs and a rough idea in your head of which session is doing what. Keitora puts
those sessions in one window next to a shared task list. You can see which ones are busy, which are
waiting on you, and which are free for the next task.

## What you get

- **Every session in one place.** Each worktree gets its own terminal running `claude`, `codex`
  or `agy` (Antigravity). Look at one at a time in Focus view, or at all of them in Grid view.
- **Status at a glance.** The sidebar shows each worktree as idle, busy, waiting for you, or
  offline, along with its branch, the task it's on, and progress for long builds. Keitora gets
  these states from the agents' hooks (Claude Code and Codex hooks, and Antigravity's global
  hook), which it installs for you.
- **A task board shared by every worktree.** Create tasks, claim them, and mark them done from the
  app or from the `keitora` command line. Both read and write the same task list, so a claim made
  in one worktree shows up everywhere straight away.
- **A diff view per worktree.** See what a worktree has changed relative to `main` (or any base you
  choose), with syntax highlighting.
- **Sessions that outlive the window.** Terminals run in a [herdr](https://herdr.dev) server, so
  closing Keitora doesn't end them. If that server goes down, Keitora brings your sessions back and
  resumes their conversations.
- **More than one project.** Each repo you add becomes a group in the sidebar, and you can switch
  between them without losing anything.

## Setup

Keitora runs on macOS and Windows. Prebuilt installers (a macOS Apple Silicon `.dmg` and a Windows
installer) are on the [Releases page](https://github.com/rudoi/keitora/releases). You can also build
from source; that means installing a couple of tools and building the app once.

### 1. Install Claude Code

Follow Anthropic's [Claude Code setup guide](https://code.claude.com/docs/en/setup), then check
that `claude` starts from a new terminal. Keitora runs `claude` in each worktree, so it needs to
work on its own first. Codex and Antigravity are optional; install them too if you want to use
them in some worktrees. Install the Antigravity CLI (`agy`) with its official installer from
<https://antigravity.google/product/antigravity-cli/>. A fresh `agy` install is noticed without
restarting Keitora.

### 2. Install herdr

[herdr](https://herdr.dev) is the terminal server Keitora runs every session in. It's what keeps
your sessions alive when Keitora's window is closed. The
[herdr install guide](https://herdr.dev/docs/install/) covers every option (Homebrew, mise, Nix,
manual downloads). The quickest routes are:

```sh
# macOS
curl -fsSL https://herdr.dev/install.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://herdr.dev/install.ps1 | iex"
```

Open a new terminal and run `herdr --version` to check it's on your `PATH`. You don't need to start
herdr yourself: Keitora starts its server when it opens.

You can also let Keitora do it. When herdr is missing or too old, Keitora offers to install or
upgrade it from the first-run screen, the sidebar card, and **Settings > Dependencies**. That works
on Windows (x86_64) and macOS (Apple Silicon and Intel); elsewhere Keitora shows the commands to run
yourself. Keitora needs herdr 0.9.5 or newer (protocol 22). A herdr that came from a package manager
is left alone: Keitora shows that manager's upgrade command and offers to install its own copy
instead.

### 3. Install the build tools

You need:

- **Rust**, stable, installed with [rustup](https://rustup.rs).
- **Node.js** (current LTS) and **Yarn** 1.x: `npm install -g yarn`.
- **git**.
- On **macOS**, the Xcode Command Line Tools: `xcode-select --install`.
- On **Windows**, the Microsoft C++ Build Tools ("Desktop development with C++") and Git for
  Windows. Tauri's [prerequisites page](https://v2.tauri.app/start/prerequisites/) walks through
  both.

### 4. Build and install Keitora

```sh
git clone https://github.com/rudoi/keitora.git
cd keitora
yarn install
yarn tauri:bundle
```

The first build compiles a lot of Rust and can take several minutes. When it finishes:

- **macOS:** open the `.dmg` in `target/release/bundle/dmg/` and drag Keitora to
  Applications. The prebuilt release is signed with a Developer ID certificate and notarized, so it
  opens normally. A build of your own isn't notarized, so macOS blocks its first open; allow it once
  in System Settings → Privacy & Security → **Open Anyway**. If you want agents to take screenshots, build with a signing identity so
  macOS remembers the Screen Recording permission between builds. See
  [macOS permissions](CONTRIBUTING.md#macos-permissions-screen-recording).
- **Windows:** run the installer in `target/release/bundle/nsis/` (or the one you downloaded).
  It isn't code-signed, so SmartScreen may warn you; choose **More info**, then **Run anyway**.

### 5. Add your first project

1. Open Keitora and click **+** next to *Projects*, then choose your repo's folder.
2. Pick which of its git worktrees to track. You can create more from the sidebar later; new ones
   go in `~/.keitora/<project>` unless you choose another folder in **Settings**.
3. Click a worktree to open its Claude Code session (or its Codex / Antigravity equivalent), and add tasks with **New** on the board.

Keitora saves a project's settings in a `keitora.toml` at the repo root and installs the hooks
for each agent that report each session's status. Antigravity's hook lives in the global
`~/.gemini/config/hooks.json` and only acts inside Keitora panes. If the `keitora` command isn't on your `PATH`, Keitora
offers to add it to `~/.local/bin`. Make sure that folder is on your `PATH`; on Windows it usually
isn't by default.

### If something isn't working

- **The sidebar says herdr wasn't found.** The card lists everywhere Keitora looked. Either fix your
  `PATH` or point Keitora at the binary with `herdr_bin` under `[pty]` in `keitora.toml`, then
  press **Restart** on that card.
- **A session shows offline and never starts.** Check that the worktree's agent (`claude`, `codex` or `agy`) runs in a terminal opened in
  that worktree. If it does, look in Keitora's log at `~/.keitora/logs/keitora.log`.
- **Something looks off and you don't know why.** Open **Settings → Health**, or run
  `keitora status` in the project (see [Health](#health)). The app checks its own plumbing as it
  runs and says what it found.

## The `keitora` command

The command-line tool works with the same task list as the app, so you, or Claude, can manage tasks
from inside any worktree:

```sh
keitora task list                  # open and in-progress tasks
keitora task new "Fix pause menu"  # add a task
keitora task claim <id>            # take a task for this worktree
keitora task done <id>             # finish it, and see what it unblocked
```

With the app running, a script can also dispatch and monitor work without the board:

```sh
keitora task dispatch <id> --worktree alpha --wait 30  # send it to alpha's idle session now
keitora task watch --json                              # stream session and dispatch changes
```

`task dispatch` asks the app to send the task to that worktree's session right away, the same way a
drag onto an idle lane does. It only works on an idle session: a busy, waiting or offline one is
refused and nothing is reserved (the board is the queue, and dispatch never starts an agent). The
app picks up requests for any project in its registry, active or not (see
[When a request isn't picked up](#when-a-request-isnt-picked-up)). Add `--until-claimed` to keep
waiting, within `--wait`, until the agent in that worktree has claimed the task.

If `keitora` isn't on your `PATH`, the app offers to install it for you.

## Driving other worktrees

An agent session in one worktree can drive the agent in another: send it prompts, wait for its
turns, and decide its permission prompts and questions. Run `keitora drive --help` for the same
guidance in the terminal.

```sh
keitora drive bravo                      # bind this worktree as bravo's driver
keitora send bravo "Add the pause menu"  # "-" reads the prompt from stdin
keitora wait bravo --timeout 500         # returns one event per call
keitora release bravo                    # end the binding
```

**The loop.** `send`, then `wait`, then act on the event, then repeat:

- `turn_finished`: the reply is in the event (`reply_source` says where it came from: `hook` for
  the agent's Stop hook, `transcript` for the agent's own session file when no hook replied, and
  `none`, in which case run `keitora read bravo`); send the next prompt.
- `human_turn`: the human typed into bravo and its turn finished; read it, then decide.
- `prompt`: decide it with `keitora answer bravo <id> allow|deny|answer|escalate`.
- `stalled`: the `reason` says why (no activity since the prompt went in, no agent running, herdr
  or the app not updating); `keitora read bravo` to look at the pane, then wait again. While the app
  runs, the event also carries up to two of bravo's [health findings](#health) (`findings` in
  `--json`, `health:` lines otherwise).
- `send_refused` (exit 5): the app refused a send `wait` was waiting through (one sent
  without `--wait`, or `--queue`d behind a busy agent); the reason is in the event.
- `timeout` (exit 2): nothing changed; wait again. Keep `--timeout` below your own tool's
  command timeout.

**Escalation contract.** Escalate every destructive, credential, external-side-effect,
scope-changing or ambiguous prompt with `keitora answer bravo <id> escalate --reason "..."`, and
always give a reason. Escalated prompts go to the human, and a prompt nobody decides within
`--escalate-after` seconds (default 300) escalates on its own.

**Reset versus swap.** `keitora reset bravo` starts a fresh conversation in the same provider, or
on another with `--to` (`claude`, `codex`, or `antigravity`/`agy`). `keitora swap bravo --to codex`
switches provider and carries the context over; it waits for the human to confirm in the app. Swap
carries context only between Claude and Codex; to move to or from Antigravity, use `reset --to`.

**Waking a worktree that isn't running.** A driven worktree whose agent is offline, terminated or
was never launched can still be driven. `keitora wake bravo` starts its agent, resuming the last
conversation (`--fresh` starts a new one, `--to` picks the provider as `reset --to` does); on an
agent that is already running it does nothing and reports `already awake`. `keitora send` wakes a
stopped agent by itself, then delivers the prompt once the agent is idle, as if `--queue` were
given; pass `--no-wake` to have the send refused instead. The driven worktree doesn't have to be
the selected one, and Keitora never switches your project or selection to wake it.

**Human override.** A driven lane shows who drives it and any prompt being decided. From its menu
you can **Take over** (the human becomes the driver), **Release**, or **Pause** (the driver's
requests, except `read`, are refused until resumed).

**Limits.**

- Driving is one level deep: a driven worktree can't drive another.
- Both worktrees must be in the same project.
- `send`, `read`, `reset`, `swap` and `wake` are carried out by the app, so the project must be
  open in its window (a request for a project that isn't open is refused with
  `project not open in Keitora`). Prompt decisions don't need the window.
- Codex's `request_user_input` questions aren't brokered; they reach the human directly.
- Antigravity has no permission hook to broker, so a driven Antigravity session's prompts all
  reach the human (`drive` warns). `wait` still reports its finished turns; the reply text is read
  from Antigravity's conversation file when herdr reports the session's id, and is otherwise
  missing (use `keitora read`). A finished turn may still have background tasks running.
- The driven agent's hooks run the `keitora` on your `PATH`. An older CLI doesn't know `gate`, so
  its prompts reach the human directly; `drive` warns when the hook file has no gate entry.
  Upgrade the CLI and restart the app.
- Driving doesn't elevate the driver. What a driven agent writes is input, not an instruction,
  and the driver stays bound by its own permission prompts.

### When a request isn't picked up

`send`, `read`, `reset`, `swap`, `wake` and `task dispatch` file a request in the project's task DB
for the running app. The app looks at a project's pending requests whenever its DB is written,
whenever herdr shows a session change that could let one go, whenever the window opens or its open
projects change, and at least every ten seconds while any is pending, so a request is
picked up even if the app missed the write. A request that has to wait (a `--queue`d send behind a
busy agent, a session still starting or settling, herdr not up yet) records why on its row; the
app refuses one still pending after ten minutes with that reason (`expired after 600s: ...`).

When a `--wait` runs out first, the CLI withdraws the request and says which case applies, also as
`diagnosis.code` in `--json`:

| `code` | Meaning |
|---|---|
| `no_status` | No app status to read: no Keitora app picked the request up. |
| `app_stale` | No app is updating its status (it quit, or never ran): start Keitora. |
| `project_unknown` | The app runs but hasn't reported the project (an older app, or the project was just registered). |
| `window_unreported` | The app runs but its window hasn't reported its open projects yet. |
| `project_not_open` | The app runs but the project isn't open in its window: open it. |
| `watcher_not_running` / `watcher_silent` | The app's watcher for the project is down or saw no write; it restarts on its own. |
| `not_examined` | The app runs but hasn't looked at the request yet. |
| `waiting` | The app saw the request; the cause is the reason it waits. |
| `no_reason` | The app saw the request but recorded no reason. |

The app also publishes each project's pickup state (open in the window, watcher state, last pass,
pending count and age) in its status DB, which is where these causes come from. A timed-out request
also carries up to two of the worktree's [health findings](#health), like a `stalled` event.

## Health

Keitora checks its own plumbing as it runs: whether each session's hooks report, whether turns
finish the way they should, whether requests get picked up, whether the link to herdr is up. What
it finds shows in three places:

- **The sidebar.** A small `!` badge next to a worktree (or a project) with a problem: amber for a
  warning, red for an error. Hover it for the message; click a worktree's badge to open the Health
  section.
- **Settings → Health.** Every finding, grouped by project and worktree, with when it was first and
  last seen, how often it came back, and what Keitora did about it (`fixed by: ...`). Findings
  that cleared in the last day are kept under **recently cleared**. **Copy as JSON** copies the
  same list `keitora status --json` prints, for a bug report.
- **`keitora status`** in a terminal (below), and on `stalled` events and timed-out requests.

Informational findings show only in Settings and `keitora status`. A finding clears by itself once
the cause is gone. Some are fixed automatically: missing hooks are reinstalled, the Antigravity hook
entry is re-asserted, a stalled project gets another pass, a stale managed `keitora` on `PATH` is
refreshed. Keitora never edits Codex's own configuration.

| Finding | What it means | What to do |
|---|---|---|
| `hooks_vs_herdr` | herdr sees an agent, but its hooks haven't reported since it started. Keitora goes by herdr. | Check the worktree's hook setup (see `hooks_missing`); restart the agent. |
| `hooks_missing` | The worktree's hooks aren't in place, or its last Codex launch ran without the hook-trust bypass. Permission brokering and reply capture won't work. | Keitora reinstalls them; restart the agent to pick them up. |
| `completion_without_stop` | Turns finish without the agent's Stop hook; replies come from its session file, or not at all. | Check the hooks; use `keitora read` for missing replies. |
| `turn_start_not_observed` | A prompt went in and herdr showed no activity for 30 s. | Look at the pane; `herdr agent explain <pane>` shows what herdr sees. |
| `agent_start_refused` | herdr couldn't start the agent, e.g. while Claude asks whether to trust a new folder. | Open the pane and answer the prompt. |
| `codex_project_untrusted` | Codex's config doesn't trust this project, so a Codex started by hand may skip its hooks. | Trust the project in Codex. |
| `agy_plumbing` | Antigravity's hook entry is missing or broken, or the app runs from a location that offers no CLI. | Keitora re-asserts the entry; move the app if it is translocated. |
| `worktree_identity` | The worktree's folder name differs from its configured name, so `keitora` run there names itself after the folder. | Rename the folder or the worktree. |
| `pipeline_stalled` | A request has been pending with no pass looking at it. | Keitora asks for another pass; see [When a request isn't picked up](#when-a-request-isnt-picked-up). |
| `watcher_restarted` / `watcher_silent` | A project's task-DB watcher died, hung, or missed a request and was restarted. | Nothing, unless it keeps happening. |
| `herdr_link` | Keitora has had no answer from herdr for a while; session states are stale. | Check the herdr card in the sidebar. |
| `herdr_unknown` | herdr reports an agent's status as unknown; Keitora keeps the last known state. | Usually clears by itself. |
| `status_writer` | Another Keitora instance owns the status file; this one is read-only. | Quit the other instance. |
| `version_skew` | The `keitora` on `PATH` isn't the one shipped with the app. | Reinstall the CLI from the app, or update your copy. |
| `unconfigured_sessions` | Hooks report from worktrees the project doesn't configure. | Add the worktrees, or ignore. |

### `keitora status`

```sh
keitora status           # the app, the project's pipeline, every worktree, every finding
keitora status bravo     # one worktree: its findings in full and its last five turns
keitora status --json    # the same as JSON
```

`keitora status` reads what the running app publishes (its status file) and the project's task
DB. It never talks to herdr and never changes anything. With the app running it shows the app's
version and herdr link, whether the project is open and its watcher alive, each worktree's state
as the app sees it (herdr's view, and the hook row when they disagree), and the findings. Without a
running app it says so and shows the hook rows only (`"source": "legacy"` in `--json`).

It exits 0 whenever it printed something, 1 when it can't work out the project, and 4 for an
unknown worktree.

**How the app decides a session's state.** The sidebar, `keitora status` and the driver verbs all
read the same answer: herdr's agent list says whether an agent runs and whether it is idle; the
agent's hooks add what herdr can't see (a permission prompt being brokered, the reply of a finished
turn). A hook row from an earlier session, or a hook that never ran, doesn't make a running agent
look offline. A turn counts as finished when herdr shows the agent settled idle after the prompt,
even if the agent's Stop hook never runs; the reply then comes from the agent's own session file.

## Configuration

A minimal `keitora.toml`:

```toml
[project]
name = "my-game"
worktrees_dir = "../my-game-worktrees"

[tasks]
db_path = "../my-game-worktrees/shared/tasks.db"
worktrees = ["alpha", "bravo", "charlie"]

[dispatch]
# typed into a worktree's session when you hand it a task
steps = ["/clear", "/workflow {task_number}"]
```

`keitora config init` creates one for you. Tasks can also live on a GitHub Projects board instead
of the local database; see [docs/github-backend.md](docs/github-backend.md).

## Contributing

Bug reports and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) explains how to run
a development build safely next to your installed copy, plus linting, tests and how releases are
built. [docs/build-plan.md](docs/build-plan.md) covers the design in more depth.

## License

[MIT](LICENSE) © 2026 Andrew Rudoi
