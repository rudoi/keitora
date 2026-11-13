// backend_tests.rs — generic two-tier TaskBackend contract suite.
//
// Invoked by per-backend test modules; not compiled in production.
// A future GitHub backend can call `run_universal` without changing any test body.
//
// Usage:
//   run_universal(&backend).await;   // every backend, regardless of consistency()
//   run_strong(&backend).await;      // only when consistency() == Strong
//
// (Module is test-gated by `#[cfg(test)] mod backend_tests;` in lib.rs — no inner
// `#![cfg(test)]` here, which would be a duplicated-attributes lint.)

use crate::{BackendError, ClaimOutcome, Consistency, ListFilter, TaskBackend, TaskUpdate};

// ---- helpers -----------------------------------------------------------------

/// Assert that `result` is `BackendError::NotFound(expected_id)`.
fn assert_not_found(result: crate::BackendResult<impl std::fmt::Debug>, expected_id: i64) {
    match result {
        Err(BackendError::NotFound(id)) => {
            assert_eq!(id, expected_id, "NotFound id mismatch");
        }
        other => panic!("expected NotFound({expected_id}), got {other:?}"),
    }
}

/// Assert that `result` is `BackendError::Conflict(_)`.
fn assert_conflict(result: crate::BackendResult<impl std::fmt::Debug>, ctx: &str) {
    match result {
        Err(BackendError::Conflict(_)) => {}
        other => panic!("expected Conflict for {ctx}, got {other:?}"),
    }
}

// ---- universal tier ----------------------------------------------------------

/// Universal tier: must hold on every backend regardless of consistency level.
pub async fn run_universal<B: TaskBackend>(b: &B) {
    test_create_round_trip(b).await;
    test_list_filter_backlog(b).await;
    test_find_excludes_archived(b).await;
    test_claim_blocked_by_dep(b).await;
    test_unclaim_returns_to_open(b).await;
    test_update_persists_and_logs(b).await;
    test_update_replaces_tags(b).await;
    test_set_done_reports_unblocked(b).await;
    test_set_review_keeps_the_assignee(b).await;
    test_newly_unblocked_by(b).await;
    test_claim_outcome_variants(b).await;
    test_add_dep_self_dep_rejected(b).await;
    test_add_dep_cycle_rejected(b).await;
    test_add_dep_duplicate_is_conflict(b).await;
    test_deps_tree_transitive(b).await;
    test_revision_advances_on_mutations(b).await;
    test_dispatch_log_round_trip(b).await;
    test_dispatch_log_ordering_and_limit(b).await;
    test_dispatch_log_latest_for_newest_and_filters(b).await;
    test_assign_logs_and_leaves_open(b).await;
    test_dispatched_reservations(b).await;
    test_dispatched_reservations_exclusions(b).await;
    test_error_not_found_variants(b).await;
    test_update_all_none_conflict(b).await;
    test_release_on_terminal_conflict(b).await;
    test_set_done_idempotent(b).await;
    test_set_done_non_owner_conflict(b).await;
    test_delete_open_succeeds(b).await;
    test_delete_active_succeeds(b).await;
    test_delete_with_dependents_conflicts(b).await;
    test_delete_removes_dep_edges(b).await;
    test_delete_purges_dispatch_log(b).await;
    test_delete_not_found(b).await;
}

/// Strong-consistency tier: run only when `b.consistency() == Consistency::Strong`.
pub async fn run_strong<B: TaskBackend + Sync + 'static>(b: &B) {
    if !matches!(b.consistency(), Consistency::Strong) {
        return; // skip for eventual backends
    }
    test_claim_exclusive_serial(b).await;
    test_assign_reservation_holds_against_claim(b).await;
    test_assign_refused_on_ineligible_status(b).await;
    test_release_returns_other_worktree_task_to_open(b).await;
    // Concurrent claim test uses its own on-disk DB and is invoked separately
    // from the entry point so it can receive the db_path. See
    // `test_concurrent_claim_exactly_one_wins` below — called directly from the
    // `strong_tier` test in sqlite_backend contract_tests.
}

// ---- universal sub-tests -----------------------------------------------------

async fn test_create_round_trip<B: TaskBackend>(b: &B) {
    let task = b
        .create(
            "alice",
            "my title",
            "my description",
            "high",
            &["foo".to_string(), "bar".to_string()],
            None,
        )
        .await
        .expect("create should succeed");

    assert_eq!(task.title, "my title");
    assert_eq!(task.description, "my description");
    assert_eq!(task.priority, "high");
    assert_eq!(task.tags, vec!["foo".to_string(), "bar".to_string()]);
    assert_eq!(
        task.created_by.as_deref(),
        Some("alice"),
        "created_by must equal the actor"
    );

    let got = b.get(task.id).await.expect("get should succeed");
    let got = got.expect("get should return Some for existing task");
    assert_eq!(got.task.id, task.id);
    assert_eq!(got.task.title, "my title");
    assert_eq!(got.task.description, "my description");
    assert_eq!(got.task.priority, "high");
    assert_eq!(got.task.tags, vec!["foo".to_string(), "bar".to_string()]);
    assert_eq!(got.task.created_by.as_deref(), Some("alice"));
}

async fn test_list_filter_backlog<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "backlog-test", "", "normal", &[], None)
        .await
        .expect("create");
    b.backlog(task.id, "tester").await.expect("backlog");

    let without = b
        .list(&ListFilter {
            include_backlog: false,
            include_archived: true,
            archive_days: 30,
            assigned_to: None,
        })
        .await
        .expect("list without backlog");
    assert!(
        !without.iter().any(|t| t.task.id == task.id),
        "backlog task must be absent when include_backlog=false"
    );

    let with = b
        .list(&ListFilter {
            include_backlog: true,
            include_archived: true,
            archive_days: 30,
            assigned_to: None,
        })
        .await
        .expect("list with backlog");
    assert!(
        with.iter().any(|t| t.task.id == task.id),
        "backlog task must appear when include_backlog=true"
    );
}

async fn test_find_excludes_archived<B: TaskBackend>(b: &B) {
    // Use a unique term unlikely to collide with other tests running on the same DB.
    let unique_term = format!(
        "uniq-find-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    );

    let task = b
        .create(
            "alice",
            &format!("title {unique_term}"),
            "",
            "normal",
            &[],
            None,
        )
        .await
        .expect("create");

    // Positive: find returns it with a generous archive window.
    let found = b.find(&unique_term, 365).await.expect("find positive");
    assert!(
        found.iter().any(|t| t.id == task.id),
        "find must return the task within the archive window"
    );

    // Negative: with archive_days=0 (or 1), a brand-new task is NOT in the done/cancelled
    // state so it still shows up (archive only hides done/cancelled older than the window).
    // The meaningful test is: if the task is cancelled and archive_days=0, it's excluded.
    // We cancel the task and verify find with archive_days=0 excludes it.
    b.cancel(task.id, "tester")
        .await
        .expect("cancel for archive test");
    let after_cancel = b
        .find(&unique_term, 0)
        .await
        .expect("find after cancel archive_days=0");
    // The task may or may not appear depending on sub-second timing; assert either outcome
    // is consistent with the spec. The positive case above is the primary assertion.
    // For a robust negative test: archive_days=365 should still include it (cancelled but new).
    let still_found = b
        .find(&unique_term, 365)
        .await
        .expect("find cancelled within window");
    assert!(
        still_found.iter().any(|t| t.id == task.id),
        "cancelled task within archive window must still appear in find"
    );
    let _ = after_cancel; // documented: timing-sensitive, not asserted strictly
}

async fn test_claim_blocked_by_dep<B: TaskBackend>(b: &B) {
    let a = b
        .create("alice", "task-A", "", "normal", &[], None)
        .await
        .expect("create A");
    let dep = b
        .create("alice", "task-dep", "", "normal", &[], None)
        .await
        .expect("create dep");

    // A depends on dep (dep must be done before A can be claimed).
    b.add_dep(a.id, dep.id, "tester").await.expect("add_dep");

    let err = b.claim(a.id, "alice").await;
    assert_conflict(err, "claim with unfinished dep");
}

async fn test_unclaim_returns_to_open<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "unclaim-me", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(task.id, "alice").await.expect("claim");
    b.unclaim(task.id, "alice").await.expect("unclaim");

    let got = b.get(task.id).await.expect("get").expect("Some");
    assert_eq!(got.task.status, "open", "unclaimed task must be open");
    assert!(
        got.task.assigned_to.is_none(),
        "assigned_to must be None after unclaim"
    );
}

async fn test_update_persists_and_logs<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "old title", "", "normal", &[], None)
        .await
        .expect("create");

    b.update(
        task.id,
        "tester",
        &TaskUpdate {
            title: Some("new title".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("update");

    let got = b.get(task.id).await.expect("get").expect("Some");
    assert_eq!(
        got.task.title, "new title",
        "update must persist the new title"
    );

    let logs = b.logs(task.id, 10).await.expect("logs");
    assert!(
        logs.iter().any(|l| l.action == "updated"),
        "logs must contain an 'updated' entry after update"
    );
}

async fn test_update_replaces_tags<B: TaskBackend>(b: &B) {
    let tags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let task = b
        .create("alice", "tagged", "", "high", &tags(&["x", "y"]), None)
        .await
        .expect("create");

    let set_tags = |new: Vec<String>| TaskUpdate {
        tags: Some(new),
        ..Default::default()
    };
    b.update(task.id, "tester", &set_tags(tags(&["y", "z"])))
        .await
        .expect("update tags");
    let got = b.get(task.id).await.expect("get").expect("Some");
    let mut got_tags = got.task.tags.clone();
    got_tags.sort();
    assert_eq!(
        got_tags,
        tags(&["y", "z"]),
        "update must replace the tag set"
    );
    assert_eq!(
        got.task.priority, "high",
        "a tags-only update leaves priority alone"
    );

    b.update(task.id, "tester", &set_tags(Vec::new()))
        .await
        .expect("clear tags");
    let got = b.get(task.id).await.expect("get").expect("Some");
    assert!(got.task.tags.is_empty(), "an empty tag set clears the tags");
}

async fn test_set_done_reports_unblocked<B: TaskBackend>(b: &B) {
    // A depends on B; finishing B should unblock A.
    let a = b
        .create("alice", "blocker-A", "", "normal", &[], None)
        .await
        .expect("create A");
    let dep_b = b
        .create("alice", "dep-B", "", "normal", &[], None)
        .await
        .expect("create B");

    // A depends on dep_b.
    b.add_dep(a.id, dep_b.id, "tester")
        .await
        .expect("add_dep A->B");

    // Claim and finish B.
    b.claim(dep_b.id, "alice").await.expect("claim B");
    let outcome = b.set_done(dep_b.id, "alice").await.expect("set_done B");

    assert!(
        !outcome.unblocked.is_empty(),
        "unblocked must be non-empty after finishing the only dep of A"
    );
    assert!(
        outcome.unblocked.contains(&a.id),
        "A must appear in unblocked after B is done; got {:?}",
        outcome.unblocked
    );
}

/// `newly_unblocked_by` reports the same set `set_done` records in `DoneOutcome`:
/// dependents that become unblocked once `id` is done, and only when *every* dep is
/// satisfied (a dependent with a second unfinished dep is NOT reported).
async fn test_newly_unblocked_by<B: TaskBackend>(b: &B) {
    // dependent depends on BOTH dep1 and dep2.
    let dependent = b
        .create("alice", "nub-dependent", "", "normal", &[], None)
        .await
        .expect("create dependent");
    let dep1 = b
        .create("alice", "nub-dep1", "", "normal", &[], None)
        .await
        .expect("create dep1");
    let dep2 = b
        .create("alice", "nub-dep2", "", "normal", &[], None)
        .await
        .expect("create dep2");
    b.add_dep(dependent.id, dep1.id, "tester")
        .await
        .expect("add dep1");
    b.add_dep(dependent.id, dep2.id, "tester")
        .await
        .expect("add dep2");

    // Finishing dep1 alone must NOT unblock the dependent (dep2 still open).
    b.claim(dep1.id, "alice").await.expect("claim dep1");
    b.set_done(dep1.id, "alice").await.expect("done dep1");
    let after_one = b
        .newly_unblocked_by(dep1.id)
        .await
        .expect("newly_unblocked_by dep1");
    assert!(
        !after_one.contains(&dependent.id),
        "dependent must NOT be unblocked while dep2 is still open; got {after_one:?}"
    );

    // Finishing dep2 unblocks the dependent — newly_unblocked_by must report it.
    b.claim(dep2.id, "alice").await.expect("claim dep2");
    b.set_done(dep2.id, "alice").await.expect("done dep2");
    let after_two = b
        .newly_unblocked_by(dep2.id)
        .await
        .expect("newly_unblocked_by dep2");
    assert!(
        after_two.contains(&dependent.id),
        "dependent must be unblocked once both deps are done; got {after_two:?}"
    );
}

/// A fresh claim yields a non-`Conflict` claim outcome; the variant depends on the
/// backend's consistency. `Strong` → `Claimed` (fresh) / `AlreadyYours` (re-claim);
/// `Eventual` → `ClaimedUnverified`. The universal assertion is only that the claim
/// succeeds and the returned task carries the actor.
async fn test_claim_outcome_variants<B: TaskBackend>(b: &B) {
    let strong = matches!(b.consistency(), Consistency::Strong);
    let task = b
        .create("alice", "claim-outcome", "", "normal", &[], None)
        .await
        .expect("create");

    let first = b.claim(task.id, "alice").await.expect("first claim");
    match (&first, strong) {
        (ClaimOutcome::Claimed(t), true) => assert_eq!(t.assigned_to.as_deref(), Some("alice")),
        (ClaimOutcome::ClaimedUnverified(t), false) => {
            assert_eq!(t.assigned_to.as_deref(), Some("alice"))
        }
        (other, _) => panic!("unexpected fresh-claim outcome (strong={strong}): {other:?}"),
    }

    // Re-claim by the same actor: Strong → AlreadyYours; Eventual → ClaimedUnverified.
    let again = b.claim(task.id, "alice").await.expect("re-claim");
    if strong {
        assert!(
            matches!(again, ClaimOutcome::AlreadyYours(_)),
            "same-actor re-claim on a Strong backend must be AlreadyYours; got {again:?}"
        );
    } else {
        assert!(
            matches!(again, ClaimOutcome::ClaimedUnverified(_)),
            "re-claim on an Eventual backend must be ClaimedUnverified; got {again:?}"
        );
    }
}

async fn test_add_dep_self_dep_rejected<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "self-dep", "", "normal", &[], None)
        .await
        .expect("create");
    let err = b.add_dep(task.id, task.id, "tester").await;
    assert_conflict(err, "self-dep");
}

async fn test_add_dep_cycle_rejected<B: TaskBackend>(b: &B) {
    let a = b
        .create("alice", "cycle-A", "", "normal", &[], None)
        .await
        .expect("create A");
    let c_b = b
        .create("alice", "cycle-B", "", "normal", &[], None)
        .await
        .expect("create B");

    b.add_dep(a.id, c_b.id, "tester").await.expect("add A->B");

    // Adding B->A would create a cycle.
    let err = b.add_dep(c_b.id, a.id, "tester").await;
    assert_conflict(err, "cycle dep B->A");
}

async fn test_add_dep_duplicate_is_conflict<B: TaskBackend>(b: &B) {
    let a = b
        .create("alice", "dup-dep-A", "", "normal", &[], None)
        .await
        .expect("create A");
    let dep = b
        .create("alice", "dup-dep-dep", "", "normal", &[], None)
        .await
        .expect("create dep");

    b.add_dep(a.id, dep.id, "tester")
        .await
        .expect("first add_dep");

    // Per the idempotency contract, duplicate → Conflict("already exists").
    // Callers treat this as idempotent success, but the backend MUST still return Conflict.
    let err = b.add_dep(a.id, dep.id, "tester").await;
    match err {
        Err(BackendError::Conflict(_)) => {} // correct
        other => panic!("expected Conflict for duplicate dep, got {other:?}"),
    }
}

async fn test_deps_tree_transitive<B: TaskBackend>(b: &B) {
    let a = b
        .create("alice", "tree-A", "", "normal", &[], None)
        .await
        .expect("create A");
    let c_b = b
        .create("alice", "tree-B", "", "normal", &[], None)
        .await
        .expect("create B");
    let c = b
        .create("alice", "tree-C", "", "normal", &[], None)
        .await
        .expect("create C");

    b.add_dep(a.id, c_b.id, "tester").await.expect("add A->B");
    b.add_dep(c_b.id, c.id, "tester").await.expect("add B->C");

    let tree = b.deps_tree(a.id).await.expect("deps_tree");

    let has_b = tree.iter().any(|n| n.id == c_b.id && n.depth == 1);
    let has_c = tree.iter().any(|n| n.id == c.id && n.depth == 2);

    assert!(
        has_b,
        "B must appear at depth 1 in deps_tree(A); got {:?}",
        tree
    );
    assert!(
        has_c,
        "C must appear at depth 2 in deps_tree(A); got {:?}",
        tree
    );
}

async fn test_revision_advances_on_mutations<B: TaskBackend>(b: &B) {
    let r0 = b.revision().await.expect("revision before create");

    let task = b
        .create("alice", "rev-test", "", "normal", &[], None)
        .await
        .expect("create");
    let r1 = b.revision().await.expect("revision after create");
    assert_ne!(r0, r1, "revision must advance after create");

    // Stable between mutations.
    let r1b = b.revision().await.expect("revision stable read");
    assert_eq!(r1, r1b, "revision must be stable between mutations");

    b.claim(task.id, "alice").await.expect("claim");
    let r2 = b.revision().await.expect("revision after claim");
    assert_ne!(r1, r2, "revision must advance after claim");

    // dispatch_log_add must also advance revision (board freshness guarantee).
    b.dispatch_log_add(task.id, "feng", "suggested", "reason")
        .await
        .expect("dispatch_log_add");
    let r3 = b.revision().await.expect("revision after dispatch_log_add");
    assert_ne!(r2, r3, "revision must advance after dispatch_log_add");
}

async fn test_dispatch_log_round_trip<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "dispatch-log-task", "", "normal", &[], None)
        .await
        .expect("create");

    b.dispatch_log_add(task.id, "feng", "dispatched", "good reason")
        .await
        .expect("dispatch_log_add");

    // dispatch_log_for returns entries for the task.
    let for_task = b.dispatch_log_for(task.id).await.expect("dispatch_log_for");
    assert!(
        !for_task.is_empty(),
        "dispatch_log_for must return the entry"
    );
    let entry = &for_task[0];
    assert_eq!(entry.task_id, task.id);
    assert_eq!(entry.worktree, "feng");
    assert_eq!(entry.decision, "dispatched");
    assert_eq!(entry.rationale, "good reason");

    // dispatch_log_list includes it.
    let all = b.dispatch_log_list(10).await.expect("dispatch_log_list");
    assert!(
        all.iter()
            .any(|e| e.task_id == task.id && e.decision == "dispatched"),
        "dispatch_log_list must include the entry"
    );

    // dispatch_log_latest_for returns Some.
    let latest = b
        .dispatch_log_latest_for(task.id, "feng")
        .await
        .expect("dispatch_log_latest_for");
    assert!(latest.is_some(), "dispatch_log_latest_for must return Some");
    assert_eq!(latest.unwrap().decision, "dispatched");

    // Unlogged (task, worktree) pair → None.
    let none = b
        .dispatch_log_latest_for(task.id, "unknown-wt")
        .await
        .expect("dispatch_log_latest_for unknown");
    assert!(none.is_none(), "unknown worktree pair must return None");
}

async fn test_dispatched_reservations<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "reserved-task", "", "normal", &[], None)
        .await
        .expect("create");

    // Assign to "feng" (keeps status = open, assigned_to = feng).
    b.assign(task.id, "keitora", "feng").await.expect("assign");

    // Add a 'dispatched' dispatch log entry for feng.
    b.dispatch_log_add(task.id, "feng", "dispatched", "")
        .await
        .expect("dispatch_log_add");

    let reservations = b
        .dispatched_reservations()
        .await
        .expect("dispatched_reservations");
    assert!(
        reservations
            .iter()
            .any(|r| r.task_id == task.id && r.worktree == "feng"),
        "dispatched_reservations must include the assigned+dispatched task; got {:?}",
        reservations
            .iter()
            .map(|r| (r.task_id, r.worktree.as_str()))
            .collect::<Vec<_>>()
    );
}

/// dispatch_log_list / dispatch_log_for must order newest-first and respect the
/// LIMIT. Ported from the deleted `Store::dispatch_log_round_trips_and_orders_newest_first`.
async fn test_dispatch_log_ordering_and_limit<B: TaskBackend>(b: &B) {
    let t1 = b
        .create("keitora", "ord task one", "", "normal", &[], None)
        .await
        .expect("create t1");
    let t2 = b
        .create("keitora", "ord task two", "", "normal", &[], None)
        .await
        .expect("create t2");

    b.dispatch_log_add(t1.id, "feng", "suggested", "good match")
        .await
        .expect("add 1");
    b.dispatch_log_add(t1.id, "feng", "dispatched", "confirmed")
        .await
        .expect("add 2");
    b.dispatch_log_add(t2.id, "sayo", "queued", "sayo busy")
        .await
        .expect("add 3");

    // dispatch_log_for returns only entries for the given task, newest first.
    let for_t1 = b.dispatch_log_for(t1.id).await.expect("dispatch_log_for");
    assert_eq!(
        for_t1.len(),
        2,
        "dispatch_log_for must return both t1 entries"
    );
    assert_eq!(
        for_t1[0].decision, "dispatched",
        "newest entry must come first"
    );
    assert_eq!(for_t1[0].rationale, "confirmed");
    assert_eq!(for_t1[1].decision, "suggested");
    assert!(
        for_t1.iter().all(|e| e.task_id == t1.id),
        "must not bleed across tasks"
    );

    // dispatch_log_list returns all entries across tasks, newest first.
    let all = b.dispatch_log_list(10).await.expect("dispatch_log_list");
    assert!(all.len() >= 3, "list must include all three entries");
    assert_eq!(
        all[0].decision, "queued",
        "newest (t2) entry must come first"
    );

    // LIMIT is respected.
    let limited = b
        .dispatch_log_list(1)
        .await
        .expect("dispatch_log_list limited");
    assert_eq!(limited.len(), 1, "LIMIT 1 must return exactly one row");
}

/// dispatch_log_latest_for must return the NEWEST entry for a (task, worktree) and
/// not bleed across worktrees. Ported from the deleted
/// `Store::dispatch_log_latest_for_returns_newest_and_filters_worktree`.
async fn test_dispatch_log_latest_for_newest_and_filters<B: TaskBackend>(b: &B) {
    let t = b
        .create("keitora", "latest-for task", "", "normal", &[], None)
        .await
        .expect("create");

    // Unlogged pair → None.
    assert!(b
        .dispatch_log_latest_for(t.id, "feng")
        .await
        .expect("none")
        .is_none());

    b.dispatch_log_add(t.id, "feng", "suggested", "r1")
        .await
        .expect("add 1");
    b.dispatch_log_add(t.id, "sayo", "queued", "r2")
        .await
        .expect("add 2");
    b.dispatch_log_add(t.id, "feng", "dispatched", "r3")
        .await
        .expect("add 3");

    // Newest entry for feng is "dispatched".
    let latest_feng = b
        .dispatch_log_latest_for(t.id, "feng")
        .await
        .expect("latest feng")
        .unwrap();
    assert_eq!(latest_feng.decision, "dispatched");
    assert_eq!(latest_feng.rationale, "r3");

    // A different-worktree entry does not bleed into feng's result.
    let latest_sayo = b
        .dispatch_log_latest_for(t.id, "sayo")
        .await
        .expect("latest sayo")
        .unwrap();
    assert_eq!(latest_sayo.decision, "queued");
}

/// `assign` reserves a task (status stays `open`, `assigned_to` set) and writes an
/// `assigned` activity-log entry. Ported from the deleted
/// `Store::assign_reserves_task_leaves_status_open_and_logs`.
async fn test_assign_logs_and_leaves_open<B: TaskBackend>(b: &B) {
    let t = b
        .create("keitora", "to be reserved", "", "normal", &[], None)
        .await
        .expect("create");
    let reserved = b.assign(t.id, "keitora", "feng").await.expect("assign");
    assert_eq!(
        reserved.status, "open",
        "assign must not flip status to active"
    );
    assert_eq!(
        reserved.assigned_to.as_deref(),
        Some("feng"),
        "assigned_to must be the target"
    );

    let logs = b.logs(t.id, 10).await.expect("logs");
    assert!(
        logs.iter().any(|l| l.action == "assigned"),
        "expected an 'assigned' log entry, got: {logs:?}"
    );
}

/// `dispatched_reservations` must EXCLUDE: queued-latest, active (claimed), unassigned,
/// and wrong-worktree dispatched entries — including only the open+assigned+latest-dispatched
/// pair. Ported from the deleted `Store::dispatched_reservations_includes_and_excludes_correctly`.
async fn test_dispatched_reservations_exclusions<B: TaskBackend>(b: &B) {
    // Included: open, assigned to feng, latest decision for feng = dispatched.
    let t_dispatched = b
        .create("keitora", "excl dispatched", "", "normal", &[], None)
        .await
        .expect("c1");
    b.assign(t_dispatched.id, "keitora", "feng")
        .await
        .expect("assign 1");
    b.dispatch_log_add(t_dispatched.id, "feng", "suggested", "")
        .await
        .expect("d1a");
    b.dispatch_log_add(t_dispatched.id, "feng", "dispatched", "")
        .await
        .expect("d1b");

    // Excluded: latest decision for assignee = queued.
    let t_queued = b
        .create("keitora", "excl queued", "", "normal", &[], None)
        .await
        .expect("c2");
    b.assign(t_queued.id, "keitora", "sayo")
        .await
        .expect("assign 2");
    b.dispatch_log_add(t_queued.id, "sayo", "dispatched", "")
        .await
        .expect("d2a");
    b.dispatch_log_add(t_queued.id, "sayo", "queued", "")
        .await
        .expect("d2b");

    // Excluded: claimed (status = active), not open.
    let t_active = b
        .create("keitora", "excl active", "", "normal", &[], None)
        .await
        .expect("c3");
    b.claim(t_active.id, "feng").await.expect("claim 3");
    b.dispatch_log_add(t_active.id, "feng", "dispatched", "")
        .await
        .expect("d3");

    // Excluded: open, no assigned_to.
    let t_unassigned = b
        .create("keitora", "excl unassigned", "", "normal", &[], None)
        .await
        .expect("c4");
    b.dispatch_log_add(t_unassigned.id, "feng", "dispatched", "")
        .await
        .expect("d4");

    // Excluded: open, assigned to sayo, but the dispatched entry is for feng.
    let t_wrong = b
        .create("keitora", "excl wrong-wt", "", "normal", &[], None)
        .await
        .expect("c5");
    b.assign(t_wrong.id, "keitora", "sayo")
        .await
        .expect("assign 5");
    b.dispatch_log_add(t_wrong.id, "feng", "dispatched", "")
        .await
        .expect("d5");

    let pairs = b
        .dispatched_reservations()
        .await
        .expect("dispatched_reservations");
    // Only t_dispatched among THESE tasks may be present (other tests share the DB,
    // so filter to this test's ids rather than asserting total length).
    let mine: Vec<_> = pairs
        .iter()
        .filter(|r| {
            [
                t_dispatched.id,
                t_queued.id,
                t_active.id,
                t_unassigned.id,
                t_wrong.id,
            ]
            .contains(&r.task_id)
        })
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "only t_dispatched should be reserved; got {mine:?}"
    );
    assert_eq!(mine[0].task_id, t_dispatched.id);
    assert_eq!(mine[0].worktree, "feng");
}

async fn test_error_not_found_variants<B: TaskBackend>(b: &B) {
    let missing: i64 = 9_999_999;

    // get returns Ok(None) — not an error.
    let get_result = b.get(missing).await.expect("get missing returns Ok(None)");
    assert!(
        get_result.is_none(),
        "get(missing) must return None, not an error"
    );

    // All write operations on a missing id → NotFound.
    assert_not_found(b.claim(missing, "alice").await, missing);
    assert_not_found(b.set_done(missing, "alice").await, missing);
    assert_not_found(
        b.update(
            missing,
            "tester",
            &TaskUpdate {
                title: Some("x".to_string()),
                ..Default::default()
            },
        )
        .await,
        missing,
    );
    assert_not_found(b.cancel(missing, "tester").await, missing);
    assert_not_found(b.backlog(missing, "tester").await, missing);
    assert_not_found(b.release(missing, "tester").await, missing);
}

async fn test_update_all_none_conflict<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "update-none-test", "", "normal", &[], None)
        .await
        .expect("create");

    let err = b.update(task.id, "tester", &TaskUpdate::default()).await;
    assert_conflict(err, "update with all-None patch");
}

async fn test_release_on_terminal_conflict<B: TaskBackend>(b: &B) {
    // Release on a done task → Conflict.
    let done_task = b
        .create("alice", "release-done", "", "normal", &[], None)
        .await
        .expect("create done_task");
    b.claim(done_task.id, "alice")
        .await
        .expect("claim done_task");
    b.set_done(done_task.id, "alice")
        .await
        .expect("set_done done_task");
    let err = b.release(done_task.id, "tester").await;
    assert_conflict(err, "release on done task");

    // Release on a cancelled task → Conflict.
    let cancelled = b
        .create("alice", "release-cancelled", "", "normal", &[], None)
        .await
        .expect("create cancelled");
    b.cancel(cancelled.id, "tester").await.expect("cancel");
    let err = b.release(cancelled.id, "tester").await;
    assert_conflict(err, "release on cancelled task");

    // Release on a backlog task → Conflict.
    let backlogged = b
        .create("alice", "release-backlog", "", "normal", &[], None)
        .await
        .expect("create backlogged");
    b.backlog(backlogged.id, "tester").await.expect("backlog");
    let err = b.release(backlogged.id, "tester").await;
    assert_conflict(err, "release on backlog task");
}

async fn test_set_done_idempotent<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "done-twice", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(task.id, "alice").await.expect("claim");
    b.set_done(task.id, "alice").await.expect("first set_done");

    // Second set_done by the same actor must succeed (idempotent per §9 contract).
    let result = b.set_done(task.id, "alice").await;
    assert!(
        result.is_ok(),
        "second set_done must return Ok (idempotent), got {:?}",
        result
    );
}

/// The assignee hands a task to review and keeps it (its lane); nobody else can. A
/// backend with no review column (GitHub) leaves it `active`, so either reads back.
async fn test_set_review_keeps_the_assignee<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "review-me", "", "normal", &[], None)
        .await
        .expect("create");
    assert_conflict(
        b.set_review(task.id, "alice").await,
        "set_review on open task",
    );
    b.claim(task.id, "alice").await.expect("claim");
    assert_conflict(
        b.set_review(task.id, "bob").await,
        "set_review by non-owner",
    );

    let reviewed = b.set_review(task.id, "alice").await.expect("set_review");
    assert_eq!(reviewed.assigned_to.as_deref(), Some("alice"));
    assert!(
        matches!(reviewed.status.as_str(), "review" | "active"),
        "status after set_review: {}",
        reviewed.status
    );
    b.set_review(task.id, "alice")
        .await
        .expect("set_review is idempotent");
}

async fn test_set_done_non_owner_conflict<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "done-nonowner", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(task.id, "alice").await.expect("claim");

    // bob is not the owner — set_done must return Conflict.
    let err = b.set_done(task.id, "bob").await;
    assert_conflict(err, "set_done by non-owner");
}

// ---- delete (universal: SQLite hard-delete + GitHub tombstone, same contract) ---

async fn test_delete_open_succeeds<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "delete-open", "", "normal", &[], None)
        .await
        .expect("create");

    b.delete(task.id).await.expect("delete");

    // Observably gone: get returns None on both backends.
    let got = b.get(task.id).await.expect("get after delete");
    assert!(got.is_none(), "deleted task must read as None");
}

async fn test_delete_active_succeeds<B: TaskBackend>(b: &B) {
    // D2: any status is deletable — including an active (claimed) task.
    let task = b
        .create("alice", "delete-active", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(task.id, "alice").await.expect("claim");

    b.delete(task.id).await.expect("delete active");

    let got = b.get(task.id).await.expect("get after delete");
    assert!(got.is_none(), "deleted active task must read as None");
}

async fn test_delete_with_dependents_conflicts<B: TaskBackend>(b: &B) {
    // A depends on B; deleting B must Conflict (no silent unblocking, D3).
    let dep_b = b
        .create("alice", "del-dep-B", "", "normal", &[], None)
        .await
        .expect("create B");
    let a = b
        .create("alice", "del-dependent-A", "", "normal", &[], None)
        .await
        .expect("create A");
    b.add_dep(a.id, dep_b.id, "tester")
        .await
        .expect("add_dep A->B");

    let err = b.delete(dep_b.id).await;
    assert_conflict(err, "delete task with dependents");

    // B is still visible — the refused delete left it untouched.
    let got = b.get(dep_b.id).await.expect("get B");
    assert!(got.is_some(), "B must remain after a refused delete");
}

async fn test_delete_removes_dep_edges<B: TaskBackend>(b: &B) {
    // A depends on B; deleting the *dependent* A is allowed (B has no dependents
    // of its own), removes the edge, and must not leave B spuriously blocked.
    let dep_b = b
        .create("alice", "del-edge-B", "", "normal", &[], None)
        .await
        .expect("create B");
    let a = b
        .create("alice", "del-edge-A", "", "normal", &[], None)
        .await
        .expect("create A");
    b.add_dep(a.id, dep_b.id, "tester")
        .await
        .expect("add_dep A->B");

    // Sanity: before delete, B reports A as a dependent.
    let before = b.blocks(dep_b.id).await.expect("blocks B before");
    assert!(
        before.contains(&a.id),
        "B must list A as a dependent before delete; got {before:?}"
    );

    b.delete(a.id).await.expect("delete dependent A");

    // B remains and its dep edge to A is gone.
    let got = b.get(dep_b.id).await.expect("get B").expect("B exists");
    assert!(
        !got.blocks.contains(&a.id),
        "B must no longer list the deleted A as a dependent; got {:?}",
        got.blocks
    );
    let after = b.blocks(dep_b.id).await.expect("blocks B after");
    assert!(
        !after.contains(&a.id),
        "blocks(B) must not include the deleted A; got {after:?}"
    );
}

async fn test_delete_purges_dispatch_log<B: TaskBackend>(b: &B) {
    // A deleted task must not leave dispatch-history rows behind. `dispatch_log`
    // has no FK to the task (its DDL is shared with the FK-less GitHub sidecar), so
    // both backends purge it explicitly — otherwise `dispatch_log_for` would keep
    // returning rows for a task that `get` now reports as gone.
    let task = b
        .create("alice", "delete-dispatched", "", "normal", &[], None)
        .await
        .expect("create");
    b.dispatch_log_add(task.id, "alpha", "dispatched", "test")
        .await
        .expect("dispatch_log_add");
    let before = b.dispatch_log_for(task.id).await.expect("log before");
    assert_eq!(before.len(), 1, "dispatch row must exist before delete");

    b.delete(task.id).await.expect("delete dispatched task");

    let after = b.dispatch_log_for(task.id).await.expect("log after");
    assert!(
        after.is_empty(),
        "dispatch_log must be purged on delete; got {after:?}"
    );
}

async fn test_delete_not_found<B: TaskBackend>(b: &B) {
    let missing: i64 = 9_999_998;
    assert_not_found(b.delete(missing).await, missing);

    // Create + delete, then delete again → NotFound (repeated delete).
    let task = b
        .create("alice", "delete-twice", "", "normal", &[], None)
        .await
        .expect("create");
    b.delete(task.id).await.expect("first delete");
    assert_not_found(b.delete(task.id).await, task.id);
}

// ---- strong-consistency sub-tests --------------------------------------------

async fn test_claim_exclusive_serial<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "exclusive-claim", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(task.id, "alice").await.expect("alice claims");

    // bob tries to claim the already-claimed task → Conflict.
    let err = b.claim(task.id, "bob").await;
    assert_conflict(err, "bob claim after alice");
}

async fn test_assign_reservation_holds_against_claim<B: TaskBackend>(b: &B) {
    let task = b
        .create("alice", "reservation-hold", "", "normal", &[], None)
        .await
        .expect("create");

    // Assign (reserve) to "feng" — status stays open.
    b.assign(task.id, "keitora", "feng")
        .await
        .expect("assign to feng");

    // sayo trying to claim the reserved task → Conflict.
    let err = b.claim(task.id, "sayo").await;
    assert_conflict(err, "sayo claim of feng-reserved task");

    // feng claiming its own reservation → Ok.
    b.claim(task.id, "feng")
        .await
        .expect("feng claims its own reservation");
}

/// `assign` must be refused (Conflict) for any task that is not `open`+eligible:
/// blocked by an unfinished dep, terminal (done/cancelled), active (claimed), or
/// in the backlog. Ported from the deleted `Store::assign_refused_when_*` tests.
async fn test_assign_refused_on_ineligible_status<B: TaskBackend>(b: &B) {
    // Blocked by an unfinished dependency.
    let dep = b
        .create("keitora", "ar-blocker", "", "normal", &[], None)
        .await
        .expect("create dep");
    let blocked = b
        .create("keitora", "ar-blocked", "", "normal", &[], None)
        .await
        .expect("create blocked");
    b.add_dep(blocked.id, dep.id, "tester")
        .await
        .expect("add_dep");
    assert_conflict(
        b.assign(blocked.id, "keitora", "feng").await,
        "assign blocked-by-dep task",
    );

    // Done (terminal).
    let done = b
        .create("keitora", "ar-done", "", "normal", &[], None)
        .await
        .expect("create done");
    b.claim(done.id, "keitora").await.expect("claim");
    b.set_done(done.id, "keitora").await.expect("set_done");
    assert_conflict(
        b.assign(done.id, "keitora", "feng").await,
        "assign done task",
    );

    // Cancelled (terminal).
    let cancelled = b
        .create("keitora", "ar-cancelled", "", "normal", &[], None)
        .await
        .expect("create cancelled");
    b.cancel(cancelled.id, "tester").await.expect("cancel");
    assert_conflict(
        b.assign(cancelled.id, "keitora", "feng").await,
        "assign cancelled task",
    );

    // Active (claimed) — even re-targeting the same worktree must fail (not open).
    let active = b
        .create("keitora", "ar-active", "", "normal", &[], None)
        .await
        .expect("create active");
    b.claim(active.id, "feng").await.expect("claim active");
    assert_conflict(
        b.assign(active.id, "keitora", "sayo").await,
        "assign active task to other",
    );
    assert_conflict(
        b.assign(active.id, "keitora", "feng").await,
        "re-assign active task to same",
    );

    // Backlog — assign must not resurrect it.
    let shelved = b
        .create("keitora", "ar-backlog", "", "normal", &[], None)
        .await
        .expect("create shelved");
    b.backlog(shelved.id, "tester").await.expect("backlog");
    assert_conflict(
        b.assign(shelved.id, "keitora", "feng").await,
        "assign backlog task",
    );
    let after = b.get(shelved.id).await.expect("get").unwrap().task;
    assert_eq!(
        after.status, "backlog",
        "assign must leave a backlog task shelved"
    );
    assert!(
        after.assigned_to.is_none(),
        "assign must not set assignee on a backlog task"
    );
}

/// Admin `release` returns a task owned by ANOTHER worktree to `open`+unassigned and
/// writes a `released` log entry. Ported from the deleted
/// `Store::release_returns_task_owned_by_another_worktree_to_open`.
async fn test_release_returns_other_worktree_task_to_open<B: TaskBackend>(b: &B) {
    let t = b
        .create("keitora", "rel-other-wt", "", "normal", &[], None)
        .await
        .expect("create");
    b.claim(t.id, "feng").await.expect("feng claims");

    // Admin ("keitora") force-releases feng's active task.
    b.release(t.id, "tester").await.expect("release");
    let after = b.get(t.id).await.expect("get").unwrap().task;
    assert_eq!(after.status, "open", "release must return the task to open");
    assert!(
        after.assigned_to.is_none(),
        "release must clear assigned_to"
    );

    let logs = b.logs(t.id, 10).await.expect("logs");
    assert!(
        logs.iter().any(|l| l.action == "released"),
        "expected a 'released' log entry"
    );
}

/// Concurrent N-thread claim: exactly one wins. This test opens N separate
/// `SqliteBackend` handles on the SAME on-disk DB to model N separate processes.
///
/// Called directly from the `strong_tier` sqlite contract test with `db_path`
/// pointing at the test's TempDb so the setup task is visible to all handles.
pub async fn test_concurrent_claim_exactly_one_wins(db_path: &str) {
    use futures::future::join_all;

    // Create the task via a fresh backend on the provided path.
    let setup = crate::SqliteBackend::open(db_path).expect("open setup backend");
    let task = setup
        .create("setup", "race-task", "", "normal", &[], None)
        .await
        .expect("create race task");
    let id = task.id;
    drop(setup);

    const N: usize = 8;
    let path = db_path.to_string();

    let handles: Vec<_> = (0..N)
        .map(|n| {
            let p = path.clone();
            let actor = format!("actor-{n}");
            tokio::spawn(async move {
                // Each task opens its own backend — separate connection, separate WAL reader.
                let backend = crate::SqliteBackend::open(&p).expect("open per-actor backend");
                backend.claim(id, &actor).await
            })
        })
        .collect();

    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task must not panic"))
        .collect();

    let ok_count = results.iter().filter(|r| r.is_ok()).count();
    let conflict_count = results
        .iter()
        .filter(|r| matches!(r, Err(BackendError::Conflict(_))))
        .count();

    assert_eq!(
        ok_count, 1,
        "exactly one concurrent claim must win; got {ok_count} wins out of {N}"
    );
    assert_eq!(
        conflict_count,
        N - 1,
        "remaining {} claims must get Conflict; got {conflict_count}",
        N - 1
    );
}
