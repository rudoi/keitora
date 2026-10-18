# Keitora

A desktop app for running several Claude Code sessions at once, one per git worktree, from a
single window.

![Keitora with a Claude Code session open, and each project's worktrees, tasks and status in the sidebar](docs/images/focus.png)

If you keep a few worktrees of a repo and a Claude Code session in each, you probably also keep a
pile of terminal tabs and a rough idea in your head of which session is doing what. Keitora puts
those sessions in one window next to a shared task list. You can see which ones are busy, which are
waiting on you, and which are free for the next task.

## What you get

- **Every session in one place.** Each worktree gets its own terminal running `claude` (or
  `codex`). Look at one at a time in Focus view, or at all of them in Grid view.
- **Status at a glance.** The sidebar shows each worktree as idle, busy, waiting for you, or
  offline, along with its branch, the task it's on, and progress for long builds. Keitora gets
  these states from Claude Code hooks, which it installs for you.
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

Keitora runs on macOS and Windows. There are no prebuilt downloads yet, so setup means installing
a couple of tools and building the app once.

### 1. Install Claude Code

Follow Anthropic's [Claude Code setup guide](https://code.claude.com/docs/en/setup), then check
that `claude` starts from a new terminal. Keitora runs `claude` in each worktree, so it needs to
work on its own first. Codex is optional; install it too if you want to use it in some worktrees.

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

- **macOS:** open the `.dmg` in `src-tauri/target/release/bundle/dmg/` and drag Keitora to
  Applications.
- **Windows:** run the installer in `src-tauri/target/release/bundle/nsis/`. It isn't signed yet,
  so SmartScreen may warn you; choose **More info**, then **Run anyway**.

### 5. Add your first project

1. Open Keitora and click **+** next to *Projects*, then choose your repo's folder.
2. Pick which of its git worktrees to track. You can create more from the sidebar later; new ones
   go in `~/.keitora/<project>` unless you choose another folder in **Settings**.
3. Click a worktree to open its Claude Code session, and add tasks with **New** on the board.

Keitora saves a project's settings in a `keitora.toml` at the repo root and installs the Claude Code
hooks that report each session's status. If the `keitora` command isn't on your `PATH`, Keitora
offers to add it to `~/.local/bin`. Make sure that folder is on your `PATH`; on Windows it usually
isn't by default.

### If something isn't working

- **The sidebar says herdr wasn't found.** The card lists everywhere Keitora looked. Either fix your
  `PATH` or point Keitora at the binary with `herdr_bin` under `[pty]` in `keitora.toml`, then
  press **Restart** on that card.
- **A session shows offline and never starts.** Check that `claude` runs in a terminal opened in
  that worktree. If it does, look in Keitora's log at `~/.keitora/logs/keitora.log`.

## The `keitora` command

The command-line tool works with the same task list as the app, so you, or Claude, can manage tasks
from inside any worktree:

```sh
keitora task list                  # open and in-progress tasks
keitora task new "Fix pause menu"  # add a task
keitora task claim <id>            # take a task for this worktree
keitora task done <id>             # finish it, and see what it unblocked
```

If `keitora` isn't on your `PATH`, the app offers to install it for you.

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
