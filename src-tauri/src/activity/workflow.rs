//! Tolerant reader for a worktree's `WORKFLOW_STATE.json` (the `/workflow` pipeline
//! state). The file's schema drifts across projects and over time, so this extracts a
//! small normalized core and degrades gracefully on anything unexpected. **Strictly
//! read-only: Keitora never creates, writes, renames, or deletes the file.**

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// Canonical `/workflow` stages, in order (index + 1 = ordinal). Sourced from the
/// `/workflow` command's state schema.
const CANONICAL_STAGES: [&str; 12] = [
    "plan",
    "refine",
    "fracture",
    "singletonize",
    "loss_check",
    "sequencing",
    "implementing",
    "testing",
    "staging",
    "review_gate",
    "finalization",
    "complete",
];

/// Age past which a non-`complete` stage renders as "stale" (muted). Staleness is a
/// display hint only — it never implies the worktree is idle.
const STALE_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
/// Largest state file we will read.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// The normalized core of a `WORKFLOW_STATE.json`, resilient to schema drift.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct WorkflowState {
    /// Trimmed; a canonical alias is applied, otherwise kept verbatim.
    pub stage: String,
    /// `1..=12` when the stage is canonical, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_ordinal: Option<u32>,
    /// The canonical stage count (12) — for a "7/12"-style fallback display.
    pub stage_total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `branch` is present AND differs from the worktree's current branch (false if the
    /// current branch is unknown).
    pub branch_mismatch: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<String>,
    /// Epoch ms parsed from the file's `last_updated`, if any. NOT used for staleness
    /// (timezones are inconsistent across projects); `file_mtime` is authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<i64>,
    /// Epoch ms of the file's modification time — the authoritative age signal.
    pub file_mtime: i64,
    /// Older than [`STALE_AFTER_MS`] and not `complete`. Display hint only: **stale does
    /// NOT mean idle** — liveness comes from process/agent activity, never from this.
    pub stale: bool,
    /// Serving a last-good value because the most recent read failed (malformed/partial).
    pub unreadable: bool,
}

/// Normalize a raw stage string to `(display_stage, ordinal)`. A canonical or aliased
/// value yields the canonical name + its ordinal; anything else is kept verbatim (the
/// original trimmed string) with no ordinal.
pub fn normalize_stage(raw: &str) -> (String, Option<u32>) {
    let trimmed = raw.trim();
    let key = trimmed.to_lowercase().replace(['-', ' '], "_");
    let canonical = match key.as_str() {
        "implement" | "implementation" | "in_progress" => "implementing",
        "testing_and_staging" | "test_and_stage" | "test" => "testing",
        "stage" => "staging",
        "loss" | "losscheck" => "loss_check",
        "sequence" | "sequenced" => "sequencing",
        "review" | "reviewgate" => "review_gate",
        "finalize" | "finalisation" => "finalization",
        "done" | "completed" => "complete",
        "singletonized" => "singletonize",
        "fractured" => "fracture",
        "refined" | "refinement" => "refine",
        "planning" | "planned" => "plan",
        other => other,
    };
    if let Some(idx) = CANONICAL_STAGES.iter().position(|s| *s == canonical) {
        (canonical.to_string(), Some(idx as u32 + 1))
    } else {
        (trimmed.to_string(), None)
    }
}

/// `true` when a non-`complete` stage's file is older than [`STALE_AFTER_MS`].
pub fn is_stale(stage: &str, file_mtime_ms: i64, now_ms: i64) -> bool {
    stage != "complete" && now_ms.saturating_sub(file_mtime_ms) > STALE_AFTER_MS
}

/// Parse `last_updated` from a value: RFC3339 string, epoch ms, or epoch seconds
/// (< 10^11 is treated as seconds → ×1000). Anything else → `None`.
fn parse_last_updated(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|dt| dt.timestamp_millis()),
        Value::Number(n) => n
            .as_i64()
            .map(|i| if i < 100_000_000_000 { i * 1000 } else { i }),
        _ => None,
    }
}

/// Zero-pad a numeric work id to 4 digits (`37` → `"0037"`).
fn pad4(n: i64) -> String {
    format!("{n:04}")
}

/// Resolve `work_number`: explicit `work_number`, else `task_id`, else the leading 4
/// digits of `plan_directory`'s last path segment.
fn work_number(obj: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(v) = obj.get("work_number") {
        match v {
            Value::String(s) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    return Some(pad4(i));
                }
            }
            _ => {}
        }
    }
    if let Some(v) = obj.get("task_id") {
        match v {
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    return Some(pad4(i));
                }
            }
            Value::String(s) => {
                if let Ok(i) = s.trim().parse::<i64>() {
                    return Some(pad4(i));
                }
            }
            _ => {}
        }
    }
    if let Some(Value::String(dir)) = obj.get("plan_directory") {
        let seg = dir.trim_end_matches(['/', '\\']);
        let seg = seg.rsplit(['/', '\\']).next().unwrap_or(seg);
        let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            return Some(digits[..4].to_string());
        }
    }
    None
}

/// Length of an array field, preferring `implementation_progress.<key>` then top-level.
fn nested_or_top_len(
    obj: &serde_json::Map<String, Value>,
    nested_parent: &str,
    key: &str,
) -> Option<u32> {
    if let Some(Value::Object(ip)) = obj.get(nested_parent) {
        if let Some(Value::Array(a)) = ip.get(key) {
            return Some(a.len() as u32);
        }
    }
    if let Some(Value::Array(a)) = obj.get(key) {
        return Some(a.len() as u32);
    }
    None
}

/// Parse the normalized core from already-read file text. `None` when the document is not
/// a JSON object, or the required `stage` field is missing/empty. Never panics.
pub fn parse_workflow(
    text: &str,
    file_mtime_ms: i64,
    _now_ms: i64,
    current_branch: Option<&str>,
) -> Option<WorkflowState> {
    // Strip a leading UTF-8 BOM.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let value: Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;

    // `stage` is required.
    let raw_stage = obj.get("stage").and_then(Value::as_str)?.trim();
    if raw_stage.is_empty() {
        return None;
    }
    let (stage, stage_ordinal) = normalize_stage(raw_stage);

    let stage_detail = obj
        .get("stage_detail")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let branch = match obj.get("branch") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    };
    let branch_mismatch = match (&branch, current_branch) {
        (Some(b), Some(cur)) => b != cur,
        _ => false,
    };

    let done = nested_or_top_len(obj, "implementation_progress", "completed_stages");
    let total = match obj.get("sequenced_stages") {
        Some(Value::Array(a)) => Some(a.len() as u32),
        _ => None,
    };
    let (done, total) = match total {
        Some(0) => (None, None),
        Some(t) => (done.map(|d| d.min(t)), Some(t)),
        None => (done, None),
    };

    let str_field = |k: &str| obj.get(k).and_then(Value::as_str).map(|s| s.to_string());

    Some(WorkflowState {
        stage,
        stage_ordinal,
        stage_total: CANONICAL_STAGES.len() as u32,
        stage_detail,
        work_number: work_number(obj),
        branch,
        branch_mismatch,
        done,
        total,
        pr_url: str_field("pr_url"),
        complexity: str_field("complexity"),
        last_updated: obj.get("last_updated").and_then(parse_last_updated),
        file_mtime: file_mtime_ms,
        stale: false, // set by the caller via is_stale (needs the normalized stage)
        unreadable: false,
    })
}

/// File mtime as epoch ms, or `None`.
fn mtime_ms(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_millis() as i64)
}

const STATE_FILENAME: &str = "WORKFLOW_STATE.json";

/// Max directory depth below the worktree root for discovery recursion.
const MAX_DISCOVERY_DEPTH: usize = 4;
/// Hard cap on directories visited during one discovery pass.
const MAX_DIRS_VISITED: usize = 2000;
/// Directory names skipped at every depth during discovery.
const EXCLUDED_DIRS: [&str; 5] = ["node_modules", "target", ".git", "Saved", "Intermediate"];
/// Re-run discovery at most this often while no file matches the current branch.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// Re-run discovery at most this often once a file matches — only to pick up a newer
/// workflow started on the same branch. Keeps the directory walk rare when settled.
const MATCHED_PROBE_INTERVAL: Duration = Duration::from_secs(300);

/// Read a file's recorded `branch` + mtime via [`parse_workflow`], so selection and display
/// agree. `None` when the file is unreadable, has no usable `stage`, or records no branch.
fn candidate_branch(path: &Path) -> Option<(String, i64)> {
    let mtime = mtime_ms(path)?;
    let st = parse_workflow(&read_capped(path)?, mtime, 0, None)?;
    Some((st.branch?, mtime))
}

/// Locate the worktree's active `WORKFLOW_STATE.json`: the newest file (by mtime; ties
/// broken by path) whose recorded `branch` equals `current_branch`. A worktree accumulates
/// state files from finished and other work, so a file that doesn't record the current
/// branch is never shown — not even at the root.
///
/// The walk is bounded and symlink-free: depth ≤ [`MAX_DISCOVERY_DEPTH`], ≤
/// [`MAX_DIRS_VISITED`] dirs, skipping [`EXCLUDED_DIRS`] and dot-dirs. Only files named
/// exactly `WORKFLOW_STATE.json` count.
pub fn discover(worktree: &Path, current_branch: &str) -> Option<PathBuf> {
    let mut best: Option<(i64, PathBuf)> = None;
    let mut visited = 0usize;
    let mut queue: Vec<(PathBuf, usize)> = vec![(worktree.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        if visited >= MAX_DIRS_VISITED {
            break;
        }
        visited += 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if ft.is_file() {
                if name != STATE_FILENAME {
                    continue;
                }
                let path = entry.path();
                if let Some((branch, mtime)) = candidate_branch(&path) {
                    let key = (mtime, path);
                    if branch == current_branch && best.as_ref().is_none_or(|b| key > *b) {
                        best = Some(key);
                    }
                }
            } else if ft.is_dir()
                && depth < MAX_DISCOVERY_DEPTH
                && !name.starts_with('.')
                && !EXCLUDED_DIRS.iter().any(|e| *e == name)
            {
                queue.push((entry.path(), depth + 1));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Per-worktree cached discovery + last-good state.
#[derive(Default)]
pub struct WorkflowSlot {
    path: Option<PathBuf>,
    /// The branch seen on the last tick; a change forces re-discovery.
    branch: Option<String>,
    last_good: Option<WorkflowState>,
    last_mtime: i64,
    /// When discovery last ran; `None` until the first probe, which is therefore immediate.
    /// Never backdated: `Instant` counts from boot on Windows, so `now - interval` panics
    /// when the machine has been up for less than `interval`.
    last_probe: Option<Instant>,
}

impl WorkflowSlot {
    /// Forget the selection (it no longer matches, or vanished).
    fn clear(&mut self) {
        self.path = None;
        self.last_good = None;
        self.last_mtime = 0;
    }
}

/// Read (and cache) one worktree's workflow state for this tick. Returns `None` when no
/// file records the current branch; a last-good value (with `unreadable=true`) when the
/// latest read failed; the fresh value on success. Never panics, never retries in a loop.
///
/// `branch` is the worktree's current git branch, supplied by the caller (the activity
/// engine's pure-fs branch reader). Only a file recording that branch is shown (see
/// [`discover`]). While the branch is unknown or HEAD is detached (e.g. mid-rebase) the
/// current selection is held rather than re-selected. This reader never spawns a
/// subprocess.
pub fn read_workflow(
    slot: &mut WorkflowSlot,
    worktree: &Path,
    branch: Option<&str>,
    now_ms: i64,
) -> Option<WorkflowState> {
    let known_branch = branch.filter(|b| !super::branch::is_detached(b));
    let branch_changed = slot.branch.as_deref() != branch;
    if branch_changed {
        slot.branch = branch.map(str::to_string);
        slot.last_mtime = 0;
    }

    if let Some(cur) = known_branch {
        let vanished = slot.path.as_ref().is_some_and(|p| !p.is_file());
        let interval = if slot.path.is_some() {
            MATCHED_PROBE_INTERVAL
        } else {
            PROBE_INTERVAL
        };
        let due = slot.last_probe.is_none_or(|at| at.elapsed() >= interval);
        if branch_changed || vanished || due {
            slot.last_probe = Some(Instant::now());
            let found = discover(worktree, cur);
            if found != slot.path {
                slot.clear();
                slot.path = found;
            }
        }
    }

    let path = slot.path.clone()?;
    let Some(mtime) = mtime_ms(&path) else {
        // File vanished between discovery and read.
        slot.clear();
        return None;
    };

    // Unchanged since the last good read → reuse, only recomputing staleness.
    if mtime == slot.last_mtime {
        if let Some(mut good) = slot.last_good.clone() {
            good.stale = is_stale(&good.stage, good.file_mtime, now_ms);
            good.unreadable = false;
            return Some(good);
        }
    }

    let parsed = read_capped(&path)
        .and_then(|text| parse_workflow(&text, mtime, now_ms, known_branch))
        .map(|mut st| {
            st.stale = is_stale(&st.stage, st.file_mtime, now_ms);
            st
        });

    match parsed {
        // Rewritten to record another branch (or none): no longer ours.
        Some(st) if known_branch.is_some() && st.branch.as_deref() != known_branch => {
            slot.clear();
            None
        }
        Some(st) => {
            slot.last_mtime = mtime;
            slot.last_good = Some(st.clone());
            Some(st)
        }
        None => {
            // Keep serving the last good value, flagged unreadable; next tick retries.
            slot.last_good.clone().map(|mut good| {
                good.stale = is_stale(&good.stage, good.file_mtime, now_ms);
                good.unreadable = true;
                good
            })
        }
    }
}

/// Read at most [`MAX_FILE_BYTES`] of a file as UTF-8 (lossy); `None` on I/O error or an
/// over-size file.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `now` as epoch ms.
pub fn now_ms() -> i64 {
    keitora_tasks::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value, mtime: i64, now: i64) -> Option<WorkflowState> {
        parse_workflow(&v.to_string(), mtime, now, None).map(|mut s| {
            s.stale = is_stale(&s.stage, s.file_mtime, now);
            s
        })
    }

    #[test]
    fn normalizes_a_full_realistic_object() {
        let v = json!({
            "version": 1,
            "stage": "implementing",
            "stage_detail": "doing the thing",
            "work_number": "0037",
            "branch": "llm/x/0037-foo",
            "plan_directory": "plans/0037-foo",
            "pr_url": "https://github.com/o/r/pull/126",
            "complexity": "large",
            "last_updated": "2026-10-03T12:00:00Z",
            "sequenced_stages": ["a", "b", "c", "d"],
            "implementation_progress": { "completed_stages": ["a", "b"] }
        });
        let s = parse(v, 1_000, 1_000).unwrap();
        assert_eq!(s.stage, "implementing");
        assert_eq!(s.stage_ordinal, Some(7));
        assert_eq!(s.stage_total, 12);
        assert_eq!(s.done, Some(2));
        assert_eq!(s.total, Some(4));
        assert_eq!(s.work_number.as_deref(), Some("0037"));
        assert_eq!(s.complexity.as_deref(), Some("large"));
        assert!(s.last_updated.is_some());
        assert!(!s.stale);
    }

    #[test]
    fn done_prefers_nested_then_top_level_then_none() {
        let nested = json!({"stage":"x","sequenced_stages":[1,2,3],"implementation_progress":{"completed_stages":[1]}});
        assert_eq!(parse(nested, 0, 0).unwrap().done, Some(1));
        let top = json!({"stage":"x","sequenced_stages":[1,2],"completed_stages":[1,2,3]});
        // done clamped to total
        assert_eq!(parse(top, 0, 0).unwrap().done, Some(2));
        let none = json!({"stage":"x"});
        assert_eq!(parse(none, 0, 0).unwrap().done, None);
        let zero = json!({"stage":"x","sequenced_stages":[],"completed_stages":[1]});
        let z = parse(zero, 0, 0).unwrap();
        assert_eq!((z.done, z.total), (None, None));
    }

    #[test]
    fn wrong_typed_fields_drop_only_themselves() {
        let v = json!({"stage":"refine","stage_detail":5,"branch":null,"done":"nope","pr_url":[1]});
        let s = parse(v, 0, 0).unwrap();
        assert_eq!(s.stage, "refine");
        assert_eq!(s.stage_detail, None);
        assert_eq!(s.branch, None);
        assert_eq!(s.pr_url, None);
    }

    #[test]
    fn missing_or_bad_stage_or_non_object_yields_none() {
        assert!(parse(json!({"stage":""}), 0, 0).is_none());
        assert!(parse(json!({"stage":5}), 0, 0).is_none());
        assert!(parse(json!({"nostage":true}), 0, 0).is_none());
        assert!(parse(json!([1, 2, 3]), 0, 0).is_none());
        assert!(parse(json!("a string"), 0, 0).is_none());
        assert!(parse(json!(42), 0, 0).is_none());
    }

    #[test]
    fn bom_and_truncated() {
        let with_bom = format!("\u{FEFF}{}", json!({"stage":"plan"}));
        assert_eq!(parse_workflow(&with_bom, 0, 0, None).unwrap().stage, "plan");
        assert!(parse_workflow("{\"stage\":\"impl", 0, 0, None).is_none());
    }

    #[test]
    fn stage_alias_table_and_ordinals() {
        for (input, canon, ord) in [
            ("implement", "implementing", Some(7)),
            ("Implement", "implementing", Some(7)),
            ("in progress", "implementing", Some(7)),
            ("testing-and-staging", "testing", Some(8)),
            ("review gate", "review_gate", Some(10)),
            ("done", "complete", Some(12)),
            ("plan", "plan", Some(1)),
            ("complete", "complete", Some(12)),
        ] {
            let (s, o) = normalize_stage(input);
            assert_eq!((s.as_str(), o), (canon, ord), "input={input}");
        }
        // Unknown / custom stages keep verbatim, no ordinal.
        assert_eq!(normalize_stage("rescoped"), ("rescoped".to_string(), None));
        assert_eq!(
            normalize_stage("ux_mockup"),
            ("ux_mockup".to_string(), None)
        );
        assert_eq!(
            normalize_stage("  Rescoped  "),
            ("Rescoped".to_string(), None)
        );
    }

    #[test]
    fn work_number_fallback_chain() {
        assert_eq!(
            parse(json!({"stage":"x","work_number":"0037"}), 0, 0)
                .unwrap()
                .work_number
                .as_deref(),
            Some("0037")
        );
        assert_eq!(
            parse(json!({"stage":"x","task_id":37}), 0, 0)
                .unwrap()
                .work_number
                .as_deref(),
            Some("0037")
        );
        assert_eq!(
            parse(json!({"stage":"x","task_id":"37"}), 0, 0)
                .unwrap()
                .work_number
                .as_deref(),
            Some("0037")
        );
        assert_eq!(
            parse(json!({"stage":"x","plan_directory":"plans/0037-foo"}), 0, 0)
                .unwrap()
                .work_number
                .as_deref(),
            Some("0037")
        );
        assert_eq!(parse(json!({"stage":"x"}), 0, 0).unwrap().work_number, None);
    }

    #[test]
    fn last_updated_parsing() {
        let p = |v: Value| parse_last_updated(&v);
        assert!(p(json!("2026-10-03T12:00:00Z")).is_some());
        assert!(p(json!("2026-10-03T12:00:00+02:00")).is_some());
        assert_eq!(p(json!(1_700_000_000_000i64)), Some(1_700_000_000_000));
        assert_eq!(p(json!(1_700_000_000i64)), Some(1_700_000_000_000));
        assert_eq!(p(json!("garbage")), None);
        assert_eq!(p(json!(true)), None);
    }

    #[test]
    fn staleness_rules() {
        let hour = 3_600_000i64;
        assert!(!is_stale("implementing", 100 * hour, 100 * hour + 5 * hour)); // 5h fresh
        assert!(is_stale("implementing", 100 * hour, 100 * hour + 7 * hour)); // 7h stale
        assert!(!is_stale("complete", 0, 1000 * hour)); // complete never stale
        assert!(!is_stale("implementing", 200 * hour, 100 * hour)); // mtime in future
    }

    #[test]
    fn branch_mismatch_only_when_both_known() {
        let v = json!({"stage":"x","branch":"feature"});
        // No current branch → no mismatch.
        assert!(
            !parse_workflow(&v.to_string(), 0, 0, None)
                .unwrap()
                .branch_mismatch
        );
        assert!(
            parse_workflow(&v.to_string(), 0, 0, Some("main"))
                .unwrap()
                .branch_mismatch
        );
        assert!(
            !parse_workflow(&v.to_string(), 0, 0, Some("feature"))
                .unwrap()
                .branch_mismatch
        );
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let inputs = [
            json!({}),
            json!({"stage":"x","implementation_progress":"notobject"}),
            json!({"stage":"x","implementation_progress":{"completed_stages":"nope"}}),
            json!({"stage":"x","sequenced_stages":{}}),
            json!({"stage":"   "}),
            json!({"stage":"x","last_updated":{}}),
            json!({"stage":"x","work_number":{}}),
            json!({"stage":"x","task_id":9_999_999_999i64}),
            json!({"stage":"\u{1F600}"}),
            json!(null),
        ];
        for v in inputs {
            let _ = parse_workflow(&v.to_string(), 0, 0, None); // must not panic
        }
    }

    const HOUR: i64 = 60 * 60 * 1000;

    /// Pretend the last probe happened just past `interval` ago. `checked_sub` because
    /// `Instant` can't go before boot; `None` (never probed) is due for every interval.
    fn age_probe(slot: &mut WorkflowSlot, interval: Duration) {
        slot.last_probe = Instant::now().checked_sub(interval + Duration::from_secs(1));
    }

    #[test]
    fn discovery_finds_a_nested_match_and_skips_excluded_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sub = root.join("plans").join("0001-a");
        write_state(&sub, "plan", "me/feat");
        // Excluded dirs, dot-dirs and backup names never count, even on a match.
        write_state(&root.join("node_modules").join("pkg"), "plan", "me/feat");
        write_state(&root.join(".hidden"), "plan", "me/feat");
        std::fs::write(
            root.join("WORKFLOW_STATE-before-rebase.json"),
            r#"{"stage":"plan","branch":"me/feat"}"#,
        )
        .unwrap();
        assert_eq!(discover(root, "me/feat"), Some(sub.join(STATE_FILENAME)));
    }

    #[test]
    fn discovery_respects_the_depth_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c").join("d").join("e");
        write_state(&deep, "plan", "me/feat");
        assert_eq!(discover(tmp.path(), "me/feat"), None);
    }

    #[test]
    fn a_root_file_for_another_branch_is_not_shown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(root, "implementing", "me/finished");
        let mut slot = WorkflowSlot::default();
        assert!(read_workflow(&mut slot, root, Some("me/parking"), now_ms()).is_none());
        // A file that records no branch never matches either.
        std::fs::write(root.join(STATE_FILENAME), r#"{"stage":"plan"}"#).unwrap();
        assert_eq!(discover(root, "me/parking"), None);
    }

    #[test]
    fn a_nested_match_beats_a_root_file_for_other_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(root, "review_gate", "old/work");
        write_state(&root.join("FEATURE"), "implementing", "me/feature");
        let mut slot = WorkflowSlot::default();
        let st = read_workflow(&mut slot, root, Some("me/feature"), now_ms()).unwrap();
        assert_eq!(st.stage, "implementing");
        assert!(!st.branch_mismatch);
    }

    #[test]
    fn the_newest_match_wins_and_ties_break_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(&root.join("A"), "complete", "feat");
        std::thread::sleep(Duration::from_millis(20));
        write_state(&root.join("B"), "plan", "feat");
        let mut slot = WorkflowSlot::default();
        let st = read_workflow(&mut slot, root, Some("feat"), now_ms()).unwrap();
        assert_eq!(st.stage, "plan");
        // Equal mtimes resolve deterministically (largest path).
        let a = root.join("A").join(STATE_FILENAME);
        let b = root.join("B").join(STATE_FILENAME);
        let t = std::fs::metadata(&b).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(t)
            .unwrap();
        assert_eq!(discover(root, "feat"), Some(b));
    }

    #[test]
    fn a_branch_change_rediscovers_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(&root.join("A"), "implementing", "br/a");
        write_state(&root.join("B"), "testing", "br/b");
        let mut slot = WorkflowSlot::default();
        let now = now_ms();
        let a = read_workflow(&mut slot, root, Some("br/a"), now).unwrap();
        assert_eq!(a.stage, "implementing");
        let b = read_workflow(&mut slot, root, Some("br/b"), now).unwrap();
        assert_eq!(b.stage, "testing");
        assert!(read_workflow(&mut slot, root, Some("me/parking"), now).is_none());
    }

    #[test]
    fn detached_head_holds_the_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(&root.join("F"), "finalization", "me/feat");
        let mut slot = WorkflowSlot::default();
        let now = now_ms() + 100 * HOUR;
        assert!(read_workflow(&mut slot, root, Some("me/feat"), now).is_some());
        // Mid-rebase: detached, and well past every probe interval.
        age_probe(&mut slot, MATCHED_PROBE_INTERVAL);
        let st = read_workflow(&mut slot, root, Some("0123456 (detached)"), now).unwrap();
        assert_eq!(st.stage, "finalization");
        assert!(!st.branch_mismatch);
        // Never matched and detached → nothing.
        let mut fresh = WorkflowSlot::default();
        assert!(read_workflow(&mut fresh, root, Some("0123456 (detached)"), now).is_none());
    }

    #[test]
    fn a_file_rewritten_for_another_branch_drops_out() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(root, "implementing", "me/feat");
        let mut slot = WorkflowSlot::default();
        let now = now_ms();
        assert!(read_workflow(&mut slot, root, Some("me/feat"), now).is_some());
        std::thread::sleep(Duration::from_millis(20));
        write_state(root, "plan", "me/next");
        assert!(read_workflow(&mut slot, root, Some("me/feat"), now).is_none());
    }

    #[test]
    fn a_matched_selection_rewalks_only_on_the_slow_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_state(&root.join("OLD"), "complete", "feat");
        let mut slot = WorkflowSlot::default();
        let now = now_ms();
        let old = read_workflow(&mut slot, root, Some("feat"), now).unwrap();
        assert_eq!(old.stage, "complete");
        // A new workflow starts on the same branch.
        std::thread::sleep(Duration::from_millis(20));
        write_state(&root.join("NEW"), "plan", "feat");
        age_probe(&mut slot, PROBE_INTERVAL);
        let held = read_workflow(&mut slot, root, Some("feat"), now).unwrap();
        assert_eq!(
            held.stage, "complete",
            "no re-walk at the fast interval once matched"
        );
        age_probe(&mut slot, MATCHED_PROBE_INTERVAL);
        let new = read_workflow(&mut slot, root, Some("feat"), now).unwrap();
        assert_eq!(new.stage, "plan");
    }

    #[test]
    fn a_fresh_slot_probes_on_its_first_tick_then_waits_for_the_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut slot = WorkflowSlot::default();
        assert!(slot.last_probe.is_none(), "a new slot is never backdated");
        let now = now_ms();
        // Nothing yet: the first tick still probes (and records when).
        assert!(read_workflow(&mut slot, root, Some("feat"), now).is_none());
        assert!(slot.last_probe.is_some());
        // A matching file appears, but the unmatched interval has not elapsed.
        write_state(root, "plan", "feat");
        assert!(read_workflow(&mut slot, root, Some("feat"), now).is_none());
        age_probe(&mut slot, PROBE_INTERVAL);
        assert_eq!(
            read_workflow(&mut slot, root, Some("feat"), now)
                .unwrap()
                .stage,
            "plan"
        );
    }

    fn write_state(dir: &Path, stage: &str, branch: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let state = format!(r#"{{"stage":"{stage}","branch":"{branch}"}}"#);
        std::fs::write(dir.join(STATE_FILENAME), state).unwrap();
    }

    #[test]
    fn read_workflow_serves_last_good_on_corruption_then_none_on_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = root.join(STATE_FILENAME);
        std::fs::write(
            &file,
            r#"{"stage":"implementing","branch":"me/feat","sequenced_stages":[1,2,3]}"#,
        )
        .unwrap();
        let mut slot = WorkflowSlot::default();
        let now = now_ms();
        let first = read_workflow(&mut slot, root, Some("me/feat"), now).unwrap();
        assert_eq!(first.stage, "implementing");
        assert!(!first.unreadable);

        // Corrupt it (and bump mtime by rewriting) → last-good with unreadable=true.
        std::fs::write(&file, "{ broken").unwrap();
        // ensure mtime differs
        filetime_bump(&file);
        let second = read_workflow(&mut slot, root, Some("me/feat"), now);
        let second = second.expect("serves last good");
        assert_eq!(second.stage, "implementing");
        assert!(second.unreadable);

        // Delete → None.
        std::fs::remove_file(&file).unwrap();
        assert!(read_workflow(&mut slot, root, Some("me/feat"), now).is_none());
    }

    // Nudge mtime forward so the cache re-reads (resolution can be coarse).
    fn filetime_bump(path: &Path) {
        let _ = std::fs::OpenOptions::new().write(true).open(path);
        // Re-write to force a modified-time change on platforms with coarse resolution.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let cur = std::fs::read(path).unwrap_or_default();
        std::fs::write(path, cur).unwrap();
    }
}
