use super::*;
use std::time::Duration;

/// A temp home with a worktree, Claude's projects dir and Codex's home.
struct Rig {
    dir: tempfile::TempDir,
}

const T: &str = "2026-01-01T00:00:10.000Z";

fn delivered() -> i64 {
    transcript::rfc3339_ms("2026-01-01T00:00:09.500Z").unwrap()
}

impl Rig {
    fn new() -> Self {
        let r = Rig {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(r.worktree()).unwrap();
        std::fs::create_dir_all(r.projects()).unwrap();
        std::fs::create_dir_all(r.codex_home().join("sessions")).unwrap();
        r
    }
    fn worktree(&self) -> PathBuf {
        self.dir.path().join("repos").join("alpha")
    }
    fn projects(&self) -> PathBuf {
        self.dir.path().join("claude").join("projects")
    }
    fn codex_home(&self) -> PathBuf {
        self.dir.path().join("codex")
    }
    fn agy_dir(&self) -> PathBuf {
        self.dir.path().join("gemini").join("conversations")
    }
    fn claude_dir(&self) -> PathBuf {
        let encoded = crate::transfer::encoded_cwd_dir_name(&self.worktree()).unwrap();
        self.projects().join(encoded)
    }
    /// A Claude transcript `name` in the worktree's project dir, modified at `mtime_ms`.
    fn claude_file(&self, name: &str, mtime_ms: i64, body: &str) -> PathBuf {
        let dir = self.claude_dir();
        std::fs::create_dir_all(&dir).unwrap();
        write_at(&dir.join(name), mtime_ms, body)
    }
    /// A rollout for `id` in `sessions/<day>/`, modified at `mtime_ms`.
    fn rollout(&self, day: &str, id: &str, mtime_ms: i64, body: &str) -> PathBuf {
        let dir = self.codex_home().join("sessions").join(day);
        std::fs::create_dir_all(&dir).unwrap();
        write_at(
            &dir.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")),
            mtime_ms,
            body,
        )
    }
    fn source(&self) -> AgentTranscripts {
        let wt = self.worktree();
        AgentTranscripts::new(
            TranscriptDirs {
                claude_projects: Some(self.projects()),
                codex_home: Some(self.codex_home()),
                agy_conversations: Some(self.agy_dir()),
            },
            Box::new(move |k| (k.worktree == "alpha").then(|| wt.clone())),
        )
    }
}

fn write_at(path: &Path, mtime_ms: i64, body: &str) -> PathBuf {
    std::fs::write(path, body).unwrap();
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_millis(mtime_ms as u64))
        .unwrap();
    path.to_path_buf()
}

fn claude_body(reply: &str) -> String {
    format!(
        "{}\n{}\n",
        serde_json::json!({"type": "user", "timestamp": T, "message": {"role": "user", "content": "say it"}}),
        serde_json::json!({"type": "assistant", "timestamp": T,
            "message": {"role": "assistant", "content": [{"type": "text", "text": reply}]}}),
    )
}

fn codex_body(cwd: Option<&Path>, reply: &str) -> String {
    let mut meta = serde_json::json!({"id": "x", "originator": "codex-tui"});
    if let Some(cwd) = cwd {
        meta["cwd"] = cwd.to_string_lossy().into();
    }
    [
        serde_json::json!({"timestamp": "2026-01-01T00:00:00.000Z", "type": "session_meta", "payload": meta}),
        serde_json::json!({"timestamp": T, "type": "event_msg", "payload": {"type": "task_started"}}),
        serde_json::json!({"timestamp": T, "type": "event_msg",
            "payload": {"type": "task_complete", "last_agent_message": reply}}),
    ]
    .map(|v| v.to_string())
    .join("\n")
}

#[test]
fn claude_locator_tries_herdr_path_then_id_then_the_fresh_newest() {
    let r = Rig::new();
    let at = delivered();
    let by_path = r.claude_file("p-1.jsonl", at, &claude_body("by path"));
    let by_id = r.claude_file("s-1.jsonl", at, &claude_body("by id"));
    let newest = r.claude_file("s-2.jsonl", at + 1_000, &claude_body("newest"));
    let wt = r.worktree();
    let p = r.projects();
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Path(&by_path)), at),
        vec![by_path.clone(), newest.clone()]
    );
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Id("s-1")), at),
        vec![by_id, newest.clone()]
    );
    // An id without a file falls through to the newest fresh transcript.
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Id("gone")), at),
        vec![newest.clone()]
    );
    // herdr's file is the newest: tried once.
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Id("s-2")), at),
        vec![newest.clone()]
    );
    assert_eq!(locate_claude(&p, &wt, None, at), vec![newest]);
}

#[test]
fn claude_locator_puts_a_stale_herdr_file_after_the_fresh_newest() {
    let r = Rig::new();
    let at = delivered();
    let old = r.claude_file("s-old.jsonl", at - 60_000, &claude_body("old"));
    let (p, wt) = (r.projects(), r.worktree());
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Id("s-old")), at),
        vec![old.clone()]
    );
    let new = r.claude_file("s-new.jsonl", at + 1_000, &claude_body("new"));
    assert_eq!(
        locate_claude(&p, &wt, Some(SessionRef::Id("s-old")), at),
        vec![new, old]
    );
}

#[test]
fn source_falls_back_from_herdrs_file_when_it_has_no_reply_for_the_turn() {
    let r = Rig::new();
    let at = delivered();
    // After `/clear`, herdr still names the previous session's file: it was written
    // around the delivery (the command's echo) but holds no prompt since then.
    let cleared = format!(
        "{}\n",
        serde_json::json!({"type": "user", "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {"role": "user", "content": "before"}})
    );
    r.claude_file("s-old.jsonl", at + 500, &cleared);
    r.claude_file("s-new.jsonl", at + 1_000, &claude_body("after clear"));
    let src = r.source();
    assert_eq!(
        src.reply(&query("claude", Some("id"), Some("s-old")))
            .map(|r| r.text),
        Ok("after clear".to_string())
    );
    // herdr's stale file with no fresh one: herdr's file says why.
    let r = Rig::new();
    r.claude_file("s-old.jsonl", at - 60_000, &cleared);
    assert_eq!(
        r.source()
            .reply(&query("claude", Some("id"), Some("s-old"))),
        Err(miss::NOT_THIS_TURN)
    );
}

#[test]
fn claude_locator_rejects_a_path_outside_the_project_dir() {
    let r = Rig::new();
    let at = delivered();
    let outside = r.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let stray = write_at(&outside.join("s-9.jsonl"), at, &claude_body("stray"));
    // Another worktree's project dir under the same projects root is outside too.
    let other = r.projects().join("other-proj");
    std::fs::create_dir_all(&other).unwrap();
    let sibling = write_at(&other.join("s-8.jsonl"), at, &claude_body("sibling"));
    for p in [&stray, &sibling] {
        assert_eq!(
            locate_claude(&r.projects(), &r.worktree(), Some(SessionRef::Path(p)), at),
            Vec::<PathBuf>::new(),
            "{}",
            p.display()
        );
    }
    // An id that tries to walk out is never joined into a path.
    assert_eq!(
        locate_claude(
            &r.projects(),
            &r.worktree(),
            Some(SessionRef::Id("../other-proj/s-8")),
            at
        ),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn claude_fallback_skips_agent_transcripts_and_stale_files() {
    let r = Rig::new();
    let at = delivered();
    r.claude_file("agent-1.jsonl", at + 5_000, &claude_body("sub-agent"));
    assert_eq!(
        locate_claude(&r.projects(), &r.worktree(), None, at),
        Vec::<PathBuf>::new()
    );
    let main = r.claude_file(
        "s-1.jsonl",
        at - FRESH_SLACK_MS - 1_000,
        &claude_body("old"),
    );
    assert_eq!(
        locate_claude(&r.projects(), &r.worktree(), None, at),
        Vec::<PathBuf>::new(),
        "older than the delivery less the slack"
    );
    write_at(&main, at - FRESH_SLACK_MS, &claude_body("fresh"));
    assert_eq!(
        locate_claude(&r.projects(), &r.worktree(), None, at),
        vec![main]
    );
}

#[test]
fn codex_id_is_found_today_yesterday_and_in_older_dated_dirs() {
    let r = Rig::new();
    let at = delivered(); // 2026-01-01 (UTC); local may be 2025-12-31.
    let index = TranscriptIndex::default();
    let home = r.codex_home();
    let wt = r.worktree();
    let today = r.rollout("2026/01/01", "t-today", at, "");
    let yesterday = r.rollout("2025/12/31", "t-yday", at, "");
    let older = r.rollout("2025/06/15", "t-old", at, "");
    for (id, want) in [("t-today", today), ("t-yday", yesterday), ("t-old", older)] {
        assert_eq!(
            locate_codex(&home, &wt, Some(SessionRef::Id(id)), at, &index),
            Some(want),
            "{id}"
        );
    }
    assert_eq!(cached_ids(&index), 3);
    assert_eq!(
        locate_codex(&home, &wt, Some(SessionRef::Id("t-none")), at, &index),
        None
    );
}

/// How many thread ids `index` remembers.
fn cached_ids(index: &TranscriptIndex) -> usize {
    index.codex.lock().unwrap().entries.len()
}

/// What `index` remembers for `id`.
fn cached(index: &TranscriptIndex, id: &str) -> Option<IndexHit> {
    let map = index.codex.lock().unwrap();
    map.entries.get(id).map(|e| e.hit.clone())
}

#[test]
fn codex_index_drops_a_rollout_that_disappeared() {
    let r = Rig::new();
    let at = delivered();
    let index = TranscriptIndex::default();
    let first = r.rollout("2025/06/15", "t-1", at, "");
    let (home, wt) = (r.codex_home(), r.worktree());
    let id = Some(SessionRef::Id("t-1"));
    assert_eq!(
        locate_codex(&home, &wt, id, at, &index),
        Some(first.clone())
    );
    std::fs::remove_file(&first).unwrap();
    let moved = r.rollout("2025/07/01", "t-1", at, "");
    assert_eq!(
        locate_codex(&home, &wt, id, at, &index),
        Some(moved.clone())
    );
    assert_eq!(cached(&index, "t-1"), Some(IndexHit::Found(moved.clone())));
    std::fs::remove_file(&moved).unwrap();
    assert_eq!(locate_codex(&home, &wt, id, at, &index), None);
    assert!(matches!(
        cached(&index, "t-1"),
        Some(IndexHit::Missing { .. })
    ));
}

#[test]
fn codex_index_remembers_a_miss_until_it_expires() {
    let r = Rig::new();
    let at = delivered();
    let index = TranscriptIndex::default();
    let (home, wt) = (r.codex_home(), r.worktree());
    let id = Some(SessionRef::Id("t-late"));
    assert_eq!(locate_codex(&home, &wt, id, at, &index), None);
    // The rollout appears: the remembered miss skips the walk until it expires.
    let late = r.rollout("2025/06/15", "t-late", at, "");
    assert_eq!(locate_codex(&home, &wt, id, at, &index), None);
    if let Some(e) = index.codex.lock().unwrap().entries.get_mut("t-late") {
        e.hit = IndexHit::Missing {
            until: Instant::now(),
        };
    }
    assert_eq!(locate_codex(&home, &wt, id, at, &index), Some(late));
}

#[test]
fn a_stale_index_entry_is_dropped_only_if_still_unchanged() {
    let r = Rig::new();
    let index = TranscriptIndex::default();
    let now = Instant::now();
    let gone = IndexHit::Found(r.dir.path().join("gone.jsonl"));
    // Another lookup stored a fresh answer after this one read the stale entry.
    let fresh = r.rollout("2025/06/15", "t-1", delivered(), "");
    index.store("t-1", Some(fresh.clone()), now);
    index.drop_if_unchanged("t-1", &gone);
    assert_eq!(cached(&index, "t-1"), Some(IndexHit::Found(fresh.clone())));
    index.drop_if_unchanged("t-1", &IndexHit::Found(fresh));
    assert_eq!(cached(&index, "t-1"), None);
    // A lookup of a stale entry drops it.
    index.store("t-2", Some(r.dir.path().join("gone.jsonl")), now);
    assert_eq!(index.lookup("t-2", now), None);
    assert_eq!(cached(&index, "t-2"), None);
}

#[test]
fn codex_index_evicts_the_least_recently_used_id() {
    let index = TranscriptIndex::default();
    let now = Instant::now();
    let r = Rig::new();
    let kept = r.rollout("2025/06/15", "t-kept", delivered(), "");
    index.store("t-kept", Some(kept.clone()), now);
    for i in 0..INDEX_MAX {
        index.store(&format!("t-{i}"), None, now);
        // Using an id keeps it from being the one evicted.
        assert_eq!(index.lookup("t-kept", now), Some(Some(kept.clone())));
    }
    assert_eq!(cached_ids(&index), INDEX_MAX);
    assert!(cached(&index, "t-kept").is_some());
    assert!(cached(&index, "t-0").is_none(), "the oldest unused id went");
}

#[test]
fn codex_id_with_path_separators_is_rejected() {
    let r = Rig::new();
    let at = delivered();
    r.rollout("2026/01/01", "t-1", at, "");
    let index = TranscriptIndex::default();
    for bad in ["../t-1", "a/t-1", r"a\t-1", "", "-t-1"] {
        assert_eq!(
            locate_codex(
                &r.codex_home(),
                &r.worktree(),
                Some(SessionRef::Id(bad)),
                at - 3_600_000, // too old for the fallback
                &index
            ),
            None,
            "{bad:?}"
        );
    }
}

#[test]
fn codex_herdr_path_must_be_a_rollout_under_sessions() {
    let r = Rig::new();
    let at = delivered();
    let index = TranscriptIndex::default();
    let (home, wt) = (r.codex_home(), r.worktree());
    let good = r.rollout("2025/06/15", "t-1", at - 3_600_000, "");
    assert_eq!(
        locate_codex(&home, &wt, Some(SessionRef::Path(&good)), at, &index),
        Some(good)
    );
    let outside = write_at(&r.dir.path().join("rollout-x-t-2.jsonl"), at, "");
    let misnamed = write_at(&home.join("sessions").join("notes.jsonl"), at, "");
    for p in [&outside, &misnamed] {
        assert_eq!(
            locate_codex(
                &home,
                &wt,
                Some(SessionRef::Path(p)),
                at - 3_600_000,
                &index
            ),
            None,
            "{}",
            p.display()
        );
    }
}

#[test]
fn codex_fallback_needs_this_worktree_in_session_meta() {
    let r = Rig::new();
    let at = delivered();
    let index = TranscriptIndex::default();
    let (home, wt) = (r.codex_home(), r.worktree());
    let other = r.dir.path().join("repos").join("bravo");
    r.rollout(
        "2026/01/01",
        "t-other",
        at + 2_000,
        &codex_body(Some(&other), "other"),
    );
    r.rollout(
        "2026/01/01",
        "t-nocwd",
        at + 3_000,
        &codex_body(None, "no cwd"),
    );
    assert_eq!(locate_codex(&home, &wt, None, at, &index), None);
    let mine = r.rollout(
        "2026/01/01",
        "t-mine",
        at + 1_000,
        &codex_body(Some(&wt), "mine"),
    );
    assert_eq!(
        locate_codex(&home, &wt, None, at, &index),
        Some(mine.clone())
    );
    // Too old for the delivery.
    write_at(
        &mine,
        at - FRESH_SLACK_MS - 1,
        &codex_body(Some(&wt), "mine"),
    );
    assert_eq!(locate_codex(&home, &wt, None, at, &index), None);
}

#[test]
fn codex_fallback_finds_a_session_started_days_before_the_delivery() {
    let r = Rig::new();
    let at = delivered(); // 2026-01-01
    let index = TranscriptIndex::default();
    let (home, wt) = (r.codex_home(), r.worktree());
    // A long-running session's rollout stays in its start date's dir.
    let mine = r.rollout(
        "2025/12/29",
        "t-long",
        at + 1_000,
        &codex_body(Some(&wt), "mine"),
    );
    // An older session of this worktree, written before the delivery.
    r.rollout(
        "2025/12/28",
        "t-older",
        at - 3_600_000,
        &codex_body(Some(&wt), "older"),
    );
    assert_eq!(
        locate_codex(&home, &wt, None, at, &index),
        Some(mine.clone())
    );
    // A fresher rollout of this worktree in an even older dir still wins on mtime.
    let fresher = r.rollout(
        "2025/11/02",
        "t-fresher",
        at + 2_000,
        &codex_body(Some(&wt), "fresher"),
    );
    assert_eq!(locate_codex(&home, &wt, None, at, &index), Some(fresher));
}

fn query(agent: &str, kind: Option<&str>, session: Option<&str>) -> ReplyQuery {
    ReplyQuery {
        key: SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        },
        agent: Some(agent.into()),
        agent_session: session.map(str::to_string),
        agent_session_kind: kind.map(str::to_string),
        delivered_at_ms: delivered(),
        detected_at_ms: delivered() + 3_000,
    }
}

#[test]
fn source_reads_the_turn_reply_or_says_why_not() {
    let r = Rig::new();
    let at = delivered();
    let src = r.source();
    r.rollout(
        "2026/01/01",
        "t-1",
        at,
        &codex_body(Some(&r.worktree()), "from the rollout"),
    );
    assert_eq!(
        src.reply(&query("codex", Some("id"), Some("t-1"))),
        Ok(TurnReply {
            text: "from the rollout".into()
        })
    );
    r.claude_file("s-1.jsonl", at, &claude_body("from claude"));
    assert_eq!(
        src.reply(&query("claude", Some("id"), Some("s-1")))
            .map(|r| r.text),
        Ok("from claude".to_string())
    );
    // A later delivery: the transcript has no turn for it.
    let mut late = query("claude", Some("id"), Some("s-1"));
    late.delivered_at_ms += 60_000;
    assert_eq!(src.reply(&late), Err(miss::NOT_THIS_TURN));
    let mut unknown = query("codex", Some("id"), Some("t-1"));
    unknown.key.worktree = "charlie".into();
    assert_eq!(src.reply(&unknown), Err(miss::NOT_FOUND));
    // An id without a rollout falls back to this worktree's fresh rollout...
    assert_eq!(
        src.reply(&query("codex", Some("id"), Some("t-missing")))
            .map(|r| r.text),
        Ok("from the rollout".to_string())
    );
    // ...and finds none once that rollout predates the delivery.
    let mut stale = query("codex", Some("id"), Some("t-missing"));
    stale.delivered_at_ms += 3_600_000;
    assert_eq!(src.reply(&stale), Err(miss::NOT_FOUND));
    assert_eq!(
        src.reply(&query("shell", None, None)),
        Err(miss::NO_PROVIDER_SUPPORT)
    );
}

#[test]
fn source_ignores_a_prompt_typed_after_the_detection() {
    let r = Rig::new();
    let at = delivered();
    // The turn's reply, then the human's next prompt, logged after the detection.
    let later = serde_json::json!({"type": "user", "timestamp": "2026-01-01T00:00:20.000Z",
        "message": {"role": "user", "content": "and now this"}});
    let body = format!("{}{later}\n", claude_body("the turn's reply"));
    r.claude_file("s-1.jsonl", at + 11_000, &body);
    let q = query("claude", Some("id"), Some("s-1"));
    assert!(q.detected_at_ms < at + 10_000);
    assert_eq!(
        r.source().reply(&q).map(|r| r.text),
        Ok("the turn's reply".to_string())
    );
}

#[test]
fn codex_days_cover_the_delivery_day_and_the_one_before() {
    let days = codex_days(delivered());
    let utc = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    assert!(days.contains(&utc));
    assert!(days.contains(&NaiveDate::from_ymd_opt(2025, 12, 31).unwrap()));
    assert!(days.len() <= 4);
    assert!(codex_days(i64::MAX).is_empty());
}

/// An agy conversation file `<id>.db`: the user's input at `at_ms`, then a done
/// planner response with `reply`.
fn agy_conversation(dir: &Path, id: &str, at_ms: i64, reply: &str) -> PathBuf {
    use keitora_tasks::transcript::agy::fixture::{metadata, response, write_conversation};
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.db"));
    let rows = [
        (0, 14, 3, metadata(at_ms), Vec::new()),
        (1, 15, 3, metadata(at_ms), response(reply.as_bytes(), 1)),
    ];
    write_conversation(&path, &rows).unwrap();
    path
}

#[test]
fn agy_locator_maps_agent_session_id_to_one_file() {
    let r = Rig::new();
    let at = delivered();
    let mine = agy_conversation(&r.agy_dir(), "c-1", at, "from agy");
    agy_conversation(&r.agy_dir(), "c-2", at, "another workspace");
    assert_eq!(
        locate_agy(&r.agy_dir(), Some(SessionRef::Id("c-1"))),
        Some(mine)
    );
    // No id, a path, or an id without a file: no guess.
    assert_eq!(locate_agy(&r.agy_dir(), None), None);
    assert_eq!(
        locate_agy(
            &r.agy_dir(),
            Some(SessionRef::Path(&r.agy_dir().join("c-1.db")))
        ),
        None
    );
    assert_eq!(locate_agy(&r.agy_dir(), Some(SessionRef::Id("c-3"))), None);
    let src = r.source();
    assert_eq!(
        src.reply(&query("agy", Some("id"), Some("c-1")))
            .map(|r| r.text),
        Ok("from agy".to_string())
    );
    let mut later = query("agy", Some("id"), Some("c-1"));
    later.delivered_at_ms += 60_000;
    assert_eq!(src.reply(&later), Err(miss::NOT_THIS_TURN));
    assert_eq!(src.reply(&query("agy", None, None)), Err(miss::NOT_FOUND));
}

#[test]
fn agy_locator_rejects_unsafe_session_id() {
    let r = Rig::new();
    let at = delivered();
    agy_conversation(&r.agy_dir(), "c-1", at, "from agy");
    // A file the id would reach by walking out of the dir.
    agy_conversation(&r.dir.path().join("gemini"), "c-9", at, "outside");
    for bad in ["../c-9", "sub/c-1", r"sub\c-1", "", "-c-1", "c-1.db"] {
        assert_eq!(
            locate_agy(&r.agy_dir(), Some(SessionRef::Id(bad))),
            None,
            "{bad:?}"
        );
    }
}
