# 0066 — Performance overlay: resource use per project / worktree / session

## Goal

Answer "which of my sessions is eating the machine right now?" at a glance. A toggleable
overlay breaks CPU, memory and disk IO down by **project → worktree → session**, sortable by
busiest, live every 2 s with a 150-sample (5 min) sparkline history, so worktrees can be
compared side by side rather than scanned card by card in the rail.

## Scope

**In**
- Backend sampler that attributes OS processes to Keitora sessions and rolls them up to
  worktree and project, plus three non-session buckets: **Keitora** (the app process, its
  WebView children, and its per-pane `herdr terminal session control` bridges), **herdr**
  (the server and its non-pane children, e.g. ConPTY hosts), and **Other** (everything
  unattributed; expandable into its top processes so stray build workers are easy to spot).
- Metrics: CPU % (normalised to the whole machine with `backend::normalize_cpu`), resident
  memory (sysinfo `Process::memory()`; on Windows the working set, so pages shared between
  processes are counted once per process and a sum can exceed machine used memory), disk read
  and write bytes per second.
- Path heuristics for processes that fall out of a pane's tree (detached build workers etc.):
  match by command-line `-Project=` (reusing `ubt::extract_project_from_args`), then by cwd,
  then by command-line paths, under exactly one registered worktree root.
- 150 samples of in-memory history per row, for sparklines.
- Toggle overlay UI: a button in the rail dock footer + the `Cmd+Shift+P` shortcut; a floating
  panel over the main area with a sortable table (CPU / Mem / IO / name), project rows
  collapsible into worktree rows, worktree rows expandable into their top processes; clicking a
  worktree row focuses its session.
- Sampling is **demand-driven**: it runs only while the overlay is open.

**Out (v1)**
- **Network IO** — dropped entirely (no machine-wide total either). There is no unprivileged
  per-process source on Windows (`sysinfo` lacks it; ETW / `GetPerTcpConnectionEStats` need
  elevation).
- GPU (not requested; PDH "GPU Engine" counters noted as a future option).
- Per-pane Windows Job Objects for exact accounting of short-lived children (follow-up if
  build CPU reads too low — see Risks).
- Any herdr upstream patch.
- Persisted history (history is dropped when the overlay closes); worktrees not registered in
  Keitora.
- Splitting Claude subagents inside one `claude.exe`.
- Draggable / resizable panel, Esc-to-close (Esc belongs to the terminal), a headless/web host.

## Architecture

```
herdr `pane process-info` ──► RootCache (pane → shell_pid + foreground pids, per server generation)
                                   │
sysinfo full refresh (2 s) ──► Vec<ProcSample> (pid, ppid, start, cpu, mem, disk bps, name)
                                   │
 claim_owners()  — pure: pane trees, then herdr subtree, then Keitora subtree
 resolve inferred worktrees (I/O, cached) for the unclaimed
 rollup()        — pure: owners + inferred → rows, top procs
                                   │
                                   ▼
          PerfSnapshot { machine, rows[] } stored in PerfState, history rings updated
                                   │  event `perf://snapshot` (payload null) while leased
                                   ▼
 usePerfSnapshot() ──pulls `perf_snapshot`──► PerfOverlay (sortable table + sparklines)
```

### Attribution order (per process, each tick)
1. **Pane tree**: the process is a root pid of a session's workspace (its pane's `shell_pid`,
   or a foreground pid herdr reports outside the shell's tree, e.g. Git Bash agents) or a
   descendant of one. A parent link counts only when the parent is in the snapshot **and**
   `child.start >= parent.start` (start times are whole seconds, equality allowed) — Windows
   keeps stale parent PIDs and reuses PIDs.
2. **Known overhead** (only for processes still unclaimed): descendants of the herdr server
   pid (including the server) → **herdr**; then descendants of Keitora's own pid
   (`std::process::id()`, including itself) → **Keitora**. herdr is checked first because the
   server may itself be a child of Keitora. The same start-time guard applies.
3. **Path heuristic** (only for processes still unclaimed): see "Path inference" below. Result
   cached by `(pid, start_time)`. Rows made this way are counted in `inferred_count` and
   procs are flagged `inferred` so the UI can mark them.
4. Otherwise **Other**.

A process is claimed once; a pid already claimed by an earlier root (roots are processed in
ascending `workspace_id` order) is never re-claimed.

Session → worktree is 1:1 today (`SessionKey {project, worktree}`) but agents may split extra
panes into a session's herdr workspace, so roots are grouped by **workspace id**, not pane id:
the keymap gives `workspace_id → SessionKey` (one pane per key); `pane list` gives every pane
of each such workspace (see `PerfSource::panes`).

### Path inference (step 3) — exact rules
Worktree roots come from the registry: every project's `Available` worktrees as
`(project resolved name, worktree name, path)`, composite id `"{project}/{worktree}"`. Paths are
normalised with `keymap::normalize_either_separator` and compared with
`ubt::is_component_prefix` (case-insensitive ASCII, component-wise). `match_root(path, roots)`
returns the root with the **longest** path that is a component-prefix of `path`; if two
different roots share that longest length, it returns `None` (ambiguous).
1. `ubt::extract_project_from_args(argv)` → `-Project=` path → `match_root`. Only when
   `ubt::is_build_mode(argv)` is true or the argv has no `-Mode=`.
2. Else the process cwd → `match_root`.
3. Else every argv entry: if it starts with `-` or `/`, take the text after its first `=`
   (entries without `=` are skipped); strip one quote pair; keep it only if it looks absolute
   (`X:\`, `X:/`, `\\`, or a leading `/`); `match_root` each. If all kept paths that matched
   resolve to the **same** root, use it; if they disagree or none match → `None`.

## Technical Design

### Backend — new module `src-tauri/src/perf/` (declare `mod perf;` in `lib.rs`)

#### `perf/mod.rs` — constants, IPC types, `PerfState`, commands
Constants:
```rust
pub const TICK: Duration = Duration::from_secs(2);
pub const TICK_LOW_HEADROOM: Duration = Duration::from_secs(5);
pub const FIRST_TICK_DELAY: Duration = Duration::from_secs(1); // after the priming refresh
pub const LEASE: Duration = Duration::from_secs(10);
pub const HISTORY_LEN: usize = 150;
pub const TOP_PROCS: usize = 8;
pub const MAX_INFER_PER_TICK: usize = 64;
pub const SNAPSHOT_EVENT: &str = "perf://snapshot";
```
IPC types (all `#[derive(Debug, Clone, PartialEq, Serialize)]`, field names snake_case, **no**
`rename_all` on structs — same as `DaemonStats` (`backend/mod.rs:349`); `Option` fields
serialize as `null` (no `skip_serializing_if`, unlike `DaemonStats`, so the TS mirror needs no
normaliser)):
```rust
pub struct PerfSnapshot { pub at_ms: u64 /* unix ms */, pub machine: PerfMachine, pub rows: Vec<PerfRow> }
pub struct PerfMachine {
    pub cpu_percent: f32,   // sysinfo global_cpu_usage()
    pub mem_used: u64, pub mem_total: u64,
    pub disk_read_bps: u64, pub disk_write_bps: u64, // sum over ALL processes' per-second deltas
}
#[serde(rename_all = "snake_case")]
pub enum PerfRowKind { Project, Worktree, Keitora, Herdr, Other }
pub struct PerfRow {
    pub key: String,            // "project:{p}", "worktree:{p}/{w}", "keitora", "herdr", "other"
    pub kind: PerfRowKind,
    pub project: Option<String>,   // resolved project name; Some for project + worktree rows
    pub worktree: Option<String>,  // Some for worktree rows
    pub resolving: bool,        // session has a pane but its roots are not fetched yet
    pub cpu_percent: f32,       // whole-machine %, summed over the row's processes
    pub mem_bytes: u64,
    pub disk_read_bps: u64, pub disk_write_bps: u64,
    pub proc_count: u32, pub inferred_count: u32,
    pub history: PerfHistory,   // oldest first, at most HISTORY_LEN entries each
    pub top_procs: Vec<PerfProc>, // empty for project rows; <= TOP_PROCS for the rest
}
pub struct PerfHistory { pub cpu: Vec<f32>, pub mem: Vec<u64> }
pub struct PerfProc {
    pub pid: u32, pub name: String, pub cpu_percent: f32, pub mem_bytes: u64,
    pub disk_read_bps: u64, pub disk_write_bps: u64, pub inferred: bool,
}
```
Row emission: `keitora`, `herdr`, `other` rows are always present (zeros allowed); a `worktree`
row is present when the worktree has a live session pane in the keymap **or** received ≥ 1
process this tick; a `project` row is present when it has ≥ 1 worktree row and its values are the
sum of its worktree rows (its own history ring is fed with those sums). `top_procs` sort:
`cpu_percent` desc, then `mem_bytes` desc, then `pid` asc.

`PerfSource` (the seam to herdr, so the module is testable without it):
```rust
pub struct PerfPane { pub pane_id: String, pub workspace_id: String, pub session: SessionKey }
pub struct PaneProcessInfo { pub shell_pid: u32, pub foreground_pids: Vec<u32> } // defined in backend/herdr/cli.rs
pub trait PerfSource: Send + Sync {
    /// Every pane of every workspace that a current-generation keymap entry owns, joined to its session.
    fn panes(&self) -> Vec<PerfPane>;
    fn pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError>;
    fn server_pid(&self, sys: &mut sysinfo::System) -> Option<u32>;
    fn generation(&self) -> u64;      // keymap.generation().0
    fn low_headroom(&self) -> bool;   // StatusCache.low
}
```
`NoSource` (also in `perf/mod.rs`) returns empty / `None` / `0` / `false` and
`Err(HerdrError::Timeout)`; it is used when the backend has no source (see wiring).

`PerfState` (managed state; `#[derive(Default)]`): `lease: Lease`, `latest:
Mutex<Option<PerfSnapshot>>`, `running: AtomicBool`.
`Lease` is a pure type over an injectable `Instant`: `renew(now)` sets `until = now + LEASE`,
`release()` clears it, `is_active(now) -> bool`. There is **no refcount**: there is one webview and
one overlay, so a single idempotent lease is equivalent.

Commands (all `#[tauri::command(async)]`, registered next to `crate::activity::worktree_activity`
in `host.rs`'s `generate_handler!`; add `app.manage(crate::perf::PerfState::default());` next to
`ActivityState` in `host.rs` setup):
- `perf_subscribe(app: AppHandle, state: State<PerfState>, backend: State<SharedBackend>) -> ()`:
  renew the lease; if `running` is false, set it and spawn the sampler thread (below).
- `perf_unsubscribe(state: State<PerfState>) -> ()`: `lease.release()`; wake the thread.
- `perf_snapshot(state: State<PerfState>) -> Option<PerfSnapshot>`: clone of `latest`.

#### Sampler thread (`perf/sampler.rs`)
Spawned by `perf_subscribe` with: `Arc<dyn PerfSource>` (`backend.perf_source()`, or
`Arc::new(NoSource)`), `Arc<dyn EmitSink>` (`Arc::new(app.clone())`, `backend/emit.rs:9`), a
worktree-roots closure `Arc<dyn Fn() -> Vec<WorktreeRoot> + Send + Sync>` (host wires it to
`app.state::<ActivityState>().worktrees()`, mapping each `WtRef` to a `WorktreeRoot`), the
`PerfState` handle, and the lease. It owns **two** `sysinfo::System`s: `sys` (full refreshes
only) and `ident_sys` (only ever passed to `PerfSource::server_pid`, whose cold path refreshes
cmd/exe of every process and must not pollute `sys`).

Loop:
1. Prime: `sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind)` and
   `sys.refresh_cpu_usage()`, `sys.refresh_memory()`; wait `FIRST_TICK_DELAY`.
2. Each tick (`recv_timeout` on a wake channel, so `perf_unsubscribe` interrupts the wait):
   if `!lease.is_active(now)` → clear `latest`, drop rings, set `running = false`, exit.
   Otherwise:
   a. `sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind)` with
      `kind = ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage()`;
      `sys.refresh_cpu_usage()`; `sys.refresh_memory()`.
   b. `elapsed` = measured seconds since the previous refresh (`Instant`, never an assumed 2 s).
      Per process: `cpu_percent = normalize_cpu(p.cpu_usage(), cores)` with `cores =
      std::thread::available_parallelism()` (sysinfo already scales by logical CPU count, so
      `cpu_usage()` is percent-of-one-core; same convention as `sample_process_stats`,
      `backend/herdr/mod.rs:1644`); `disk_*_bps = DiskUsage::{read_bytes,written_bytes} as f64
      / elapsed` rounded. A `(pid, start_time)` seen for the first time in this sampler run
      reports cpu 0 and disk 0 (sysinfo's first delta is the process's lifetime total).
   c. Build `Vec<ProcSample>` (pid, `p.parent()`, `p.start_time()`, name via
      `p.name().to_string_lossy()`, the metrics above). `ProcSample` is plain data so every
      later step is testable without sysinfo.
   d. `herdr_pid = source.server_pid(&mut ident_sys)`, `keitora_pid = std::process::id()`.
   e. `roots` from `RootCache` (below); `owners = claim_owners(&procs, &roots, herdr_pid,
      keitora_pid)`.
   f. For unclaimed `(pid, start)` keys missing from the `inferred` cache, take at most
      `MAX_INFER_PER_TICK` (lowest pid first; the rest wait for the next tick and count as Other
      meanwhile): `sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), false,
      ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet)
      .with_cwd(UpdateKind::OnlyIfNotSet))` (the pattern of `procs.rs::scan`), read
      `cmd_of(p)` (`real.rs:621`) and `p.cwd()`, run `infer_worktree`, store the
      `Option<String>` result (failures to read = `None`, still cached). Drop cache entries whose
      `(pid, start)` is no longer in the snapshot.
   g. `rollup(...)` → rows; push each row's `(cpu, mem)` into its ring in `history.rs`; fill
      `history` from the rings; set `PerfState.latest`; `emit(SNAPSHOT_EVENT, Value::Null)`.
   h. Next wait = `TICK_LOW_HEADROOM` if `source.low_headroom()` else `TICK`.
      (`status_loop.rs` slows to 10 s when low; the overlay uses 5 s because the user is
      looking at it. The low flag is read from `StatusCache.low`, not recomputed.)

#### `perf/attribute.rs` — pure, no I/O
```rust
pub struct ProcSample { pub pid: u32, pub ppid: Option<u32>, pub start: u64, pub name: String,
    pub cpu_percent: f32, pub mem_bytes: u64, pub disk_read_bps: u64, pub disk_write_bps: u64 }
pub struct Root { pub workspace_id: String, pub pids: Vec<u32> } // shell pid first, then foreground pids
pub enum Owner { Session(String /* workspace_id */), Herdr, Keitora }
pub struct WorktreeRoot { pub project: String, pub worktree: String, pub path: String }
pub fn claim_owners(procs: &[ProcSample], roots: &[Root], herdr_pid: Option<u32>, keitora_pid: u32)
    -> HashMap<u32, Owner>;
pub fn match_root(path: &str, roots: &[WorktreeRoot]) -> Option<usize>;
pub fn infer_worktree(argv: &[String], cwd: Option<&str>, roots: &[WorktreeRoot]) -> Option<usize>;
pub fn rollup(procs: &[ProcSample], owners: &HashMap<u32, Owner>, inferred: &HashMap<(u32, u64), String>,
    panes: &[PerfPane], resolving: &HashSet<String>) -> Vec<RowAccum>; // RowAccum: values without history
```
`rollup` sums per row, builds `top_procs`, derives `project` rows from worktree rows, and applies
the row-emission rules above. Inferred cache values are composite worktree ids.

#### `perf/roots.rs` — RootCache
- `RootEntry { shell_pid: u32, foreground_pids: Vec<u32>, shell_start: Option<u64>,
  fetched_at: Instant, generation: u64 }`; `RootCache` holds `Mutex<HashMap<pane_id, RootEntry>>`
  and `Mutex<HashMap<pane_id, Instant>>` of failed fetches (retry after `FETCH_RETRY = 10 s`).
- Constants: `ROOT_REFRESH = 60 s` (re-fetch each pane's foreground pids this often; the shell pid
  itself never changes for a pane), `IDLE_WAIT = 1 s`.
- Pure methods (tested): `next_to_fetch(&self, panes, generation, now) -> Option<String>` (a pane
  with no entry or an entry of another generation first, then the entry with the oldest
  `fetched_at` older than `ROOT_REFRESH`; skip panes in the failure window),
  `store(pane_id, info, generation, now)`, `record_failure(pane_id, now)`,
  `invalidate_generation(generation)` (drops all entries of other generations),
  `bind_and_validate(&mut self, procs: &[ProcSample])` (first sighting of `shell_pid` records its
  `start`; if the pid is later absent or has a different `start`, the entry is dropped so it is
  refetched), `roots(&self, panes) -> Vec<Root>`, `resolving(&self, panes) -> HashSet<workspace_id>`
  (workspaces with a pane lacking a valid entry).
- `fetch_step(source: &dyn PerfSource, cache: &RootCache, now: Instant) -> bool` does one fetch
  (returns whether it called herdr). A background thread started with the sampler loops
  `fetch_step`, sleeping `IDLE_WAIT` when nothing was fetched, and exits with the sampler. Calls
  are strictly sequential (each CLI call costs ~350 ms); they never run on the sampler thread.
  A `pane_not_found` error removes the pane's entry; any other error → `record_failure`.
- `HerdrOps::pane_process_info(&self, pane_id: &str) -> Result<PaneProcessInfo, HerdrError>` is
  added to the trait (`backend/herdr/ops.rs`). `RealHerdr` runs
  `self.run(CallClass::Quick, &["pane", "process-info", "--pane", pane_id])` (build the argv in a
  tiny pure fn so a test pins it; `--pane` is confirmed from herdr source) and parses with
  `cli::parse_pane_process_info(&Value) -> Option<PaneProcessInfo>`, mapping `None` to
  `HerdrError::Parse { message: "pane process-info: unexpected shape" }`.
- `parse_pane_process_info`: `RealHerdr::run` returns the whole JSON; other herdr output nests the
  payload under `result` (`/result/panes`, `/result/pane`). Read `/result/process_info`, falling
  back to `/process_info` when there is no `result` envelope. Required: `shell_pid` (u64 that
  fits u32, else `None`). `foreground_processes` is optional; collect each entry's `pid` (skip
  entries without a valid one), drop duplicates and the shell pid. Ignore every other field.
  The documented shape is `{"type":"pane_process_info","process_info":{"pane_id","shell_pid",
  "foreground_process_group_id","tty","foreground_processes":[{"pid","name","argv0","argv",
  "cmdline","cwd"}]}}`.

#### Wiring herdr into the module
- `PtyBackend` (`backend/mod.rs:466`) gets `fn perf_source(&self) -> Option<Arc<dyn
  crate::perf::PerfSource>> { None }` (default, like `daemon_stats` at `:617`). Only
  `HerdrBackend` overrides it, returning `Arc::new(HerdrPerfSource { ops:
  Arc::clone(&self.ops), keymap: Arc::clone(&self.keymap), status_cache:
  Arc::clone(&self.status_cache) })`. `HerdrPerfSource` lives in a new
  `backend/herdr/perf_source.rs`.
- `panes()`: `let current = keymap.generation()`; the workspace→session map is
  `keymap.entries()` filtered to `p.server_generation == current`; the pane list is
  `status_loop::session_panes(&*ops, &keymap, &status_cache, Instant::now())` (cached 5 s / 30 s
  low, so it costs a `pane list` at most that often); keep panes whose `workspace_id` is in the
  map.
- `server_pid` → `ops.server_pid(sys)`; `generation()` → `keymap.generation().0`;
  `low_headroom()` → `status_cache.lock().unwrap().low`.
- Keitora's bridges need no special handling: they are spawned by Keitora
  (`RealHerdr::spawn_bridge`, `real.rs:450`), so they are descendants of `std::process::id()`.

#### `perf/history.rs`
`HistoryStore { rings: HashMap<String, Ring> }`, `Ring { cpu: VecDeque<f32>, mem:
VecDeque<u64>, absent: u32 }`. `push(key, cpu, mem)`; `end_tick(present: &HashSet<String>)`
pushes `(0.0, 0)` into rings of absent rows, increments `absent` (reset on presence) and removes a
ring once `absent >= HISTORY_LEN`. Rings are capped at `HISTORY_LEN` (oldest evicted).

### Frontend
Types and wrappers in `src/lib/ipc.ts` (uses the file's `invoke` helper, `ipc.ts:11`): TS mirrors
`PerfSnapshot`, `PerfMachine`, `PerfRowKind` (`"project" | "worktree" | "keitora" | "herdr" |
"other"`), `PerfRow`, `PerfHistory`, `PerfProc` with identical snake_case field names and
`string | null` for the Rust `Option<String>`; `perfSubscribe(): Promise<void>`,
`perfUnsubscribe(): Promise<void>`, `perfSnapshot(): Promise<PerfSnapshot | null>`.

New files:
- `src/lib/perf.ts` — **pure** (no React, no Tauri). Exports:
  `type PerfSortKey = "cpu" | "mem" | "io" | "name"`, `interface PerfSort { key: PerfSortKey;
  dir: "asc" | "desc" }`, `DEFAULT_PERF_SORT = { key: "cpu", dir: "desc" }`,
  `nextSort(current, key)` (same key toggles dir; a new key starts `desc` for cpu/mem/io and
  `asc` for name), `ioOf(row) = disk_read_bps + disk_write_bps`,
  `buildPerfView(rows, sort) -> { groups: { project: PerfRow; worktrees: PerfRow[] }[];
  pinned: PerfRow[]; attributedShare: number | null }` (groups ordered and, inside each group,
  worktrees ordered by the sort; ties by name ascending; `pinned` always `keitora`, `herdr`,
  `other` in that order and never sorted; `attributedShare` = sum of cpu of all rows except
  `other` and project rows, divided by the sum including `other`, `null` when that total is
  below 0.05), `toggleInSet(set, key) -> Set<string>`, and `sparklinePoints(values: number[],
  width: number, height: number, max: number, slots = 150) -> string` (the `points` attribute:
  sample `i` of `n` is at `x = (slots - n + i) / (slots - 1) * width`, `y = height - (v /
  max) * height`, 2 decimals, `max` clamped to ≥ 1e-9, empty array → `""`).
- `src/lib/usePerfSnapshot.ts` — `usePerfSnapshot(enabled: boolean): PerfSnapshot | null`.
  While `enabled`: call `perfSubscribe()` immediately and every 4 s (lease renewal; the backend
  lease is 10 s); `useLiveResource(perfSnapshot, "perf://snapshot", DAEMON_EVENTS_ONLY)`
  (`sessions.ts:189`, `intervalMs: 0`; the event payload is `null`, `subscribeLive` re-pulls the
  cached snapshot on each event). On disable/unmount: clear the interval and `perfUnsubscribe()`
  (errors ignored). Returns `null` while disabled or before the first snapshot.
- `src/components/PerfOverlay.tsx` — `PerfOverlay({ onClose, onFocusSession })`, mounted only
  while open (so sort and expansion state reset on close); calls `usePerfSnapshot(true)`.
  Local state: `sort` (default `DEFAULT_PERF_SORT`), `collapsedProjects: Set<string>` (empty =
  all projects expanded), `expandedProcs: Set<string>` (empty = no process lists). Layout: header
  (title "Performance", machine CPU %, memory `formatBytes(used) / formatBytes(total)`,
  disk `↓ formatRate(read) ↑ formatRate(write)`, attributed share as `NN%`, close button);
  column header buttons Name / CPU / Mem / IO (sets `nextSort`, `aria-sort`); project rows;
  indented worktree rows; pinned rows at the bottom. Each CPU and Mem cell: value
  (`formatCpu` / `formatBytes`), a proportional bar (CPU bar width = `cpu_percent`% of the cell,
  Mem bar = `mem_bytes / machine.mem_total`) and an inline SVG `<polyline>` sparkline (local
  `Sparkline` component, 64×18, `fill="none"`, `stroke="currentColor"`, stroke-width 1.5,
  points from `sparklinePoints`; CPU scaled to `max(5, max(history.cpu))`, Mem to
  `max(history.mem)`). IO cell: `formatRate(read + write)` with the read/write split in `title`.
  `resolving` rows show "resolving…" in the Name cell. Inferred process rows get a trailing
  `inferred` badge with title "attributed by path, not by pane". Worktree and `other` rows
  toggle their `top_procs` list on click of the chevron; clicking a worktree row's name calls
  `onFocusSession(row.project, row.worktree)`. Fixed panel, not draggable/resizable.
- `src/lib/format.ts` gains `formatRate(bytesPerSec: number): string` = `` `${formatBytes(n)}/s` ``
  (`"—"` for the same inputs `formatBytes` rejects).
- CSS in `src/App.css` (the only app stylesheet besides `index.css`): a `/* ---- perf overlay
  ---- */` block with `.perf-overlay` (`position: absolute; top: 12px; right: 12px; width:
  560px; max-width: calc(100% - 24px); max-height: calc(100% - 24px); z-index: 50; overflow:
  auto`, background `var(--bg-1)`, border `1px solid var(--line)`), row/bar/sparkline classes
  prefixed `perf-`, colours only from existing tokens (`--bg-*`, `--tx-*`, `--line`, `--accent`,
  `--accent-2`, `--warn`) so light theme works with no extra rules. Plus `.rail-perf` added to
  the existing `.rail-settings, .rail-shortcuts` rule pairs (`App.css:216`, `:231`).
- Toggle button: a third button in `.rail-footer` (`Rail.tsx`, inside `.rail-dock`, `:680`),
  between Settings and shortcuts: `<button className="rail-perf" aria-pressed={perfOpen}
  title="Performance (Ctrl+Shift+P)" onClick={onTogglePerf}>` with lucide `Activity` icon (14 px)
  and label `perf`. `Rail` gets props `perfOpen: boolean` and `onTogglePerf: () => void`.
- Shortcut: in `src/lib/keybindings.ts` add `togglePerf: () => void` to `KeyCtx` and a
  `DEFAULT_BINDINGS` entry `{ id: "perf.toggle", keys: "Cmd+Shift+P", label: "Performance
  overlay", group: "Global", global: true, when: () => true, run: (ctx) => ctx.togglePerf() }`
  (`Cmd` is Ctrl on Windows/Linux per `matchesKey`; the chord is currently unbound and is not a
  terminal control code, so `global: true` is safe). The help sheet lists it automatically.
- `App.tsx`: `const [perfOpen, setPerfOpen] = useState(false)` (not persisted — "remembered per
  session" means in memory for the app's lifetime); `togglePerf` added to `keyCtx` (`:875`) and
  its deps; pass `perfOpen` / `onTogglePerf` to `<Rail>` (`:1110`); a `focusPerfSession =
  useCallback((project, worktree) => { const g = railGroups.find((x) => x.resolvedName ===
  project); if (g) selectLane(g, worktree); }, [railGroups, selectLane])` (`selectLane`,
  `:826`, switches project if needed and sets focus view); mount `{perfOpen && <PerfOverlay
  onClose={() => setPerfOpen(false)} onFocusSession={focusPerfSession} />}` as the last child of
  `<div className="body" style={{ position: "relative" }}>` (after the `diffTarget` block), so it
  floats over the stage and never over the rail.
- Visual check through the local mock harness (`.harness/`, mockIPC, gitignored — add a
  `perf_snapshot` / `perf_subscribe` / `perf_unsubscribe` mock returning a canned snapshot built
  from placeholder names like `proj/alpha`); no herdr contact.

### Testing
All tests are `#[cfg(test)]` / vitest; **no test talks to a real herdr or runs the app.**
- `perf/attribute.rs` (exhaustive): tree walk claims descendants; start-time guard rejects a
  child older than its parent and accepts equal start; missing parent not followed; pane tree
  beats herdr/Keitora subtree; herdr subtree beats Keitora when the server is Keitora's child;
  multi-root workspace (foreground pid outside the shell tree); a pid claimed once;
  deterministic root order; `match_root` (longest prefix, sibling-prefix rejection `…/wt2` vs
  `…/wt`, case/separator normalisation, nested worktrees, equal-length tie → `None`);
  `infer_worktree` (UBT `-Project=` incl. quoted path with spaces, non-build `-Mode=` ignored,
  cwd fallback, cmdline `-Foo=path` fallback, disagreeing cmdline paths → `None`); `rollup`
  sums, project = sum of worktrees, inferred counts, `top_procs` order and cap, row-emission
  rules, Other collects the rest.
- `perf/roots.rs` with `FakeHerdr` (`backend/herdr/fake.rs:131`; add `Op::PaneProcessInfo` to
  `ops.rs` `Op`, a scripted per-pane info table + `set_process_info(pane_id, info)`, and the
  `HerdrOps` impl recording the call): fetch only missing panes; refresh after `ROOT_REFRESH`;
  generation change drops entries; PID reuse (different `start`) drops the entry; failure
  backoff; `pane_not_found` removes the entry; one fetch per `fetch_step`; `resolving` set.
- `perf/history.rs`: cap at `HISTORY_LEN`, absent rows get zeros and are dropped after
  `HISTORY_LEN` ticks, returning rows keep their ring.
- `perf/sampler.rs`: a pure `build_snapshot(...)` over hand-made `ProcSample`s yields the exact
  `PerfSnapshot` (machine sums, first-sight zeroing, low-headroom wait selection); `Lease`
  renew / expire / release with injected `Instant`s.
- `perf/mod.rs`: `serde_json::to_value(PerfSnapshot)` pins the wire shape (field names,
  `"kind":"worktree"`, nulls) — the TS mirror is written from it.
- `backend/herdr/cli.rs`: `parse_pane_process_info` — enveloped (`result`), bare, missing shell
  pid → `None`, entries without pid skipped, duplicates and shell pid dropped.
  `backend/herdr/real.rs`: the process-info argv helper.
- `backend/herdr/perf_source.rs`: `panes()` keeps only current-generation workspaces and joins
  sessions (against `FakeHerdr` + a real `KeyMap::new()`).
- TS (`src/lib/perf.test.ts`): sort by each key and direction, name tiebreak, `nextSort`,
  grouping and project ordering, pinned order, `attributedShare` (incl. null), `toggleInSet`,
  `sparklinePoints` (empty, one sample right-aligned, full ring, flat series).
  `src/lib/keybindings.test.ts`: `makeCtx` gets `togglePerf: vi.fn()`; a `perf.toggle` test
  (`matchesKey` with ctrl+shift+p, `run` calls `togglePerf`, `global` is true).
  `src/lib/format.test.ts`: `formatRate`.
- Manual (owed, not automated): real-app smoke with a UBT build + a busy Claude session; measure
  the tick cost (target < 20 ms, log it at `debug` per tick as `perf: tick took {ms} ms`).

## Dependencies
- herdr ≥ 0.7 for `pane.process_info` (Keitora targets 0.9.3).
- `sysinfo` 0.37 (`system` feature, already in `src-tauri/Cargo.toml:53`; `disk_usage`, `cwd`,
  `cmd`, `parent`, `start_time` need no extra feature). No new crates.

## Risks
- **Short-lived processes undercount.** Sampling misses brief `cl.exe` / `link.exe`, so UBT
  CPU reads spiky and low. Mitigation if it matters: Keitora-owned named Job Object per pane
  shell (accounting includes exited children) — a separate follow-up.
- **Detached workers** (MSBuild node reuse, VBCSCompiler, mspdbsrv) may not resolve to a single
  worktree by path → land in Other. Acceptable; labelled honestly.
- **Sampler cost**: one full process refresh every 2 s, only while visible. Measured during the
  manual smoke; target < 20 ms per tick.
- **cwd/cmd reads** touch other processes' memory and fail for elevated ones — fallback only,
  at most `MAX_INFER_PER_TICK` new processes per tick, cached, failures cached as "no match".
- **Root fetch latency**: first open with many panes needs N × ~350 ms of sequential CLI calls;
  rows fill in progressively and show "resolving…".
- **Memory sums** double-count shared pages (see Scope); the header shows machine used memory
  separately so the gap is visible.

## Decisions (refine)
- Network IO: dropped for v1, no header total.
- Toggle: rail-dock button + Ctrl+Shift+P.
- Overlay: fixed panel over the terminal area.
- `Other`: expandable into its top processes.

## Implementation reference (verified against the tree)
Rust, under `src-tauri/src/`:

| What | Where |
|---|---|
| `normalize_cpu(per_core_percent, cores)` | `backend/mod.rs:376` |
| `SessionKey { project, worktree }` (project = resolved name) | `backend/mod.rs:73` |
| `DaemonStats` (serde precedent, no `rename_all`) | `backend/mod.rs:349` |
| `trait PtyBackend`; `daemon_stats` default method (pattern for `perf_source`) | `backend/mod.rs:466`; `:617` |
| `SharedBackend = Arc<dyn PtyBackend>` | `backend/mod.rs:657` |
| `HerdrBackend` fields `ops`, `keymap`, `status_cache` | `backend/herdr/mod.rs:231` |
| `sample_process_stats` (sysinfo pattern) | `backend/herdr/mod.rs:1644` |
| `impl PtyBackend for HerdrBackend` | `backend/herdr/mod.rs:2146` |
| `trait HerdrOps`; `server_pid(&self, &mut System)` | `backend/herdr/ops.rs:143`; `:200` |
| `Generation(pub u64)`; `ServerInstance`; test-only `enum Op` | `ops.rs:268`; `:246`; `:281` |
| `RealHerdr`; `run` (returns the full JSON, errors on `{"error"}`); `impl HerdrOps` | `backend/herdr/real.rs:31`; `:111`; `:218` |
| `pane_list` reads `/result/panes`; `pane_get` reads `/result/pane` | `real.rs:307`; `:319` |
| `spawn_bridge` (`terminal session control <id> --takeover ...`) | `real.rs:450` |
| `RealHerdr::server_pid` (cached, ambiguous-safe) | `real.rs:500` |
| `cmd_of(&Process) -> Vec<String>` (`pub(crate)`) | `real.rs:621` |
| `FakeHerdr` (the existing `HerdrOps` double); `impl HerdrOps`; `fail_next` | `backend/herdr/fake.rs:131`; `:712`; `:473` |
| `PaneIdentity` / `PaneInfo`; `parse_pane_identity` (where `parse_pane_process_info` goes) | `backend/herdr/cli.rs:18`; `:46`; `:132` |
| `HerdrPane { workspace_id, pane_id, server_generation, .. }`; `KeyMap`; `entries()`; `generation()` | `backend/herdr/keymap.rs:47`; `:139`; `:410`; `:272` |
| `normalize_either_separator` | `backend/herdr/keymap.rs:577` |
| `StatusCache` (`low`, `panes`); `SharedStatusCache`; `session_panes`; cadences | `backend/herdr/status_loop.rs:156`; `:168`; `:172`; `:70`-`:80` |
| `trait EmitSink { fn emit(&self, &str, serde_json::Value) }`, `impl for AppHandle` | `backend/emit.rs:9` |
| `trait Headroom` (sampling used by the status loop; the perf sampler reads `StatusCache.low` instead) | `headroom.rs:57` |
| `WtRef`, `ActivityState::worktrees()` (5 s TTL cache; make both `pub(crate)`), `enumerate_worktrees` | `activity/mod.rs:69`; `:104`; `:118` |
| `extract_project_from_args`, `is_build_mode`, `project_to_worktree` (first-wins on ties, so not used), `is_component_prefix` | `activity/progress/ubt.rs:95`; `:106`; `:145`; `:164` |
| targeted `refresh_processes_specifics(Some(&pids), false, with_cmd(OnlyIfNotSet))` pattern | `activity/progress/procs.rs:150` |
| `app.manage(...)` setup block; `generate_handler![...]` | `host.rs:2073`-`:2083`; `:2123` |
| module list | `lib.rs` (add `mod perf;`) |

Frontend, under `src/`:

| What | Where |
|---|---|
| `subscribeLive` (re-pulls `load` on each event; payload only used for `revision`) | `lib/useLiveResource.ts:66` |
| `useLiveResource(load, event, options)` | `lib/useLiveResource.ts:175` |
| `DAEMON_EVENTS_ONLY = { intervalMs: 0 }`; `useDaemonStats` (hook precedent) | `lib/sessions.ts:189`; `:235` |
| `invoke` helper; `DaemonStats` TS mirror | `lib/ipc.ts:11`; `:216` |
| `formatBytes`; `formatCpu` | `lib/format.ts:25`; `:47` |
| `KeyCtx`; `DEFAULT_BINDINGS`; `matchesKey` (`Cmd` = Ctrl/Meta) | `lib/keybindings.ts:14`; `:173`; `:73` |
| test `makeCtx` | `lib/keybindings.test.ts:154` |
| `.rail-dock` markup (`DaemonStatusBadge` then `.rail-footer`); lucide import | `components/Rail.tsx:680`; `:11` |
| `.rail-dock`, `.rail-footer`, `.rail-settings,.rail-shortcuts` CSS | `App.css:203`; `:210`; `:216` |
| theme tokens `--bg-0..3`, `--tx-1..3`, `--line`, `--accent`, `--accent-2`, `--warn` | `App.css:3`-`:55` |
| `GroupModel { key, resolvedName, summary, isActive, .. }` | `lib/railGroups.ts:64` |
| `railGroups` memo; `selectLane(g, w)`; `keyCtx`; `<Rail>`; `.body` wrapper; help mount | `App.tsx:621`; `:826`; `:875`; `:1110`; `:1109`; `:1245` |

sysinfo 0.37.2 (vendored at `~/.cargo/registry/src/*/sysinfo-0.37.2/`), all confirmed:
`System::refresh_processes_specifics(ProcessesToUpdate, remove_dead: bool, ProcessRefreshKind)
-> usize`; `ProcessesToUpdate::{All, Some(&[Pid])}`; `ProcessRefreshKind::nothing()` with
builders `with_cpu()`, `with_memory()`, `with_disk_usage()`, `with_cwd(UpdateKind)`,
`with_cmd(UpdateKind)`, `with_exe(UpdateKind)`; `UpdateKind::{Never, Always, OnlyIfNotSet}`;
`Process::{pid, name() -> &OsStr, cmd() -> &[OsString], cwd() -> Option<&Path>, memory(),
cpu_usage(), start_time() (epoch secs), parent() -> Option<Pid>, disk_usage() -> DiskUsage}`;
`DiskUsage { total_written_bytes, written_bytes, total_read_bytes, read_bytes }` (the
non-`total_` fields are deltas since the previous refresh); `System::{refresh_cpu_usage,
global_cpu_usage, refresh_memory, used_memory, total_memory}`. On Windows `parent()` is
re-read on every process refresh regardless of `ProcessRefreshKind`, `cpu_usage()` is
percent-of-one-core (multiplied by the logical CPU count, so it can exceed 100), and a process's
first disk reading is its lifetime total (hence the first-sight zeroing above).

## Resolved from herdr source (0.9.5, read-only)
1. **Argument form:** `herdr pane process-info --pane <id>` — the leaf uses
   `PaneProcessInfoProjection` with a `--pane` flag (herdr `src/cli/pane/bindings.rs`, the
   characterisation test in `src/cli/pane.rs` passes `["--pane", "explicit"]`). Without `--pane`
   it does **not** default to the caller pane, so the flag is mandatory.
2. **Envelope:** herdr's own client reads `response["result"]["process_info"]`
   (`src/cli/agent.rs`), so `/result/process_info` is the primary shape; keep the `/process_info`
   fallback. The parser test uses a hand-written placeholder sample in that shape.

## Remaining check (manual smoke only)
- Where pane shells sit in the OS tree on Windows (child of the server vs. a ConPTY host). The
  walk handles any depth and pane trees are claimed before the herdr row, so this does not affect
  the design.
