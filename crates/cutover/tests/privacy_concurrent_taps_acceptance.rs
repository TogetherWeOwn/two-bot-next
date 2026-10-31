//! Concurrent-tap acceptance for the privacy service against a migrated
//! disposable database.
//!
//! Failing-first contract for serializing concurrent `/privacy opt-out` /
//! `opt-in` taps per (guild, member). The Builder implements on this branch
//! and must not edit this file.
//!
//! Failing command (controller):
//! ```sh
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
//!   python3 scripts/cargo_cache.py run -- test -p two-bot-cutover \
//!     --test privacy_concurrent_taps_acceptance
//! ```
//! Expected pre-implementation failure: the service documents that every
//! opt-out invocation writes its own receipt, so N parallel opt-out taps for
//! one pair commit N `privacy_opt_out_audit` rows (and may interleave with a
//! racing opt-in), instead of exactly one purging tap and N-1 no-op taps.
//! The two control tests (distinct pairs, parallel opt-ins) pass today and
//! must keep passing.
//!
//! What these tests pin, at the `two_bot_cutover::privacy` boundary that both
//! member self-serve and assisted operator calls share:
//! 1. Opt-out and opt-in for one (guild, member) are mutually exclusive: any
//!    concurrent herd fits a serial order respecting non-overlapping calls.
//! 2. N parallel opt-out taps: exactly one receipt, suppression present,
//!    exactly one tap reports the purge and every tap completes without an
//!    error (the service-level shape of one "opted out" reply and N-1
//!    "already" replies; the command layer must not turn a raced tap into a
//!    store-failure reply).
//! 3. A mixed opt-out/opt-in herd always ends in the state matching the
//!    last-serialized tap, writes a receipt only on a transition into
//!    suppression, and every opt-in reports whether it actually removed a
//!    suppression row.
//!
//! Coverage limits (stated, not silent): these tests drive the service
//! functions directly with barrier-synchronized tasks; they do not dispatch
//! Discord interactions, so the ephemeral reply text each outcome maps to
//! stays covered by the command-runtime fixtures. Last-serialized equals
//! last-completed for non-overlapping taps. Overlapping taps may return in
//! either order after committing, so the oracle searches legal serial orders
//! using invocation/return stamps, not channel receive order. No timing sleeps
//! coordinate the herds: a `Barrier` releases every tap together.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Barrier};
use two_bot_core::database_tls::TlsPolicy;
use two_bot_cutover::db::{record_event, CutoverDb, FunnelWrite};
use two_bot_cutover::privacy::{is_opted_out, opt_back_in, opt_out_member};
use two_bot_testsupport::TestDatabase;

const GUILD_A: &str = "1545644954272137297";
const GUILD_B: &str = "1545644954272137298";
const ACTOR: &str = "member:self-serve";

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the privacy concurrent-taps database suite"
            );
            eprintln!("SKIP privacy concurrent-taps integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    // A wider pool than the sequential suites: herd taps must be able to hold
    // separate connections at the same time, or the pool itself serializes
    // them and the race under test never happens.
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 16, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

fn event(guild: &str, member: &str) -> FunnelWrite {
    FunnelWrite {
        member_id: Some(member.to_owned()),
        guild_id: guild.to_owned(),
        event_type: "first_message".to_owned(),
        occurred_at: "2026-10-12T00:00:00Z".to_owned(),
        source: "privacy-concurrent-taps-fixture".to_owned(),
        metadata: Some("{}".to_owned()),
    }
}

/// Seed one persisted event so the winning opt-out has rows to purge (one
/// `events` row plus its `members` projection row, as in `privacy_service`).
async fn seed(db: &CutoverDb, guild: &str, member: &str) {
    let (inserted, _) = record_event(db, &event(guild, member))
        .await
        .expect("seed write");
    assert!(inserted, "seed event persists for {guild}/{member}");
}

async fn receipts(db: &CutoverDb, guild: &str, member: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_out_audit WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(guild)
    .bind(member)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

async fn event_count(db: &CutoverDb, guild: &str, member: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
        .bind(guild)
        .bind(member)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tap {
    OptOut,
    OptIn,
}

/// For an opt-out, `purged` is the summed rows the call reports having
/// deleted; an already-suppressed tap must report zero. For an opt-in,
/// `removed` reports whether it deleted a suppression row.
#[derive(Debug)]
enum Outcome {
    OptOut(Result<i64, String>),
    OptIn(Result<bool, String>),
}

#[derive(Debug)]
struct Completed {
    invoked: u64,
    returned: u64,
    outcome: Outcome,
}

/// Release `taps` against one pair from a single barrier. The shared clock
/// bounds each service call; channel receive order has no semantic meaning.
async fn herd(db: &CutoverDb, guild: &str, member: &str, taps: &[Tap]) -> Vec<Completed> {
    let barrier = Arc::new(Barrier::new(taps.len()));
    let clock = Arc::new(AtomicU64::new(0));
    let (tx, mut rx) = mpsc::unbounded_channel::<Completed>();
    let mut handles = Vec::with_capacity(taps.len());
    for tap in taps {
        let tap = *tap;
        let db = db.clone();
        let guild = guild.to_owned();
        let member = member.to_owned();
        let barrier = Arc::clone(&barrier);
        let tx = tx.clone();
        let clock = Arc::clone(&clock);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let invoked = clock.fetch_add(1, Ordering::SeqCst);
            let outcome = match tap {
                Tap::OptOut => Outcome::OptOut(
                    opt_out_member(&db, &guild, &member, ACTOR)
                        .await
                        .map(|counts| counts.iter().map(|count| count.rows).sum())
                        .map_err(|err| err.to_string()),
                ),
                Tap::OptIn => Outcome::OptIn(
                    opt_back_in(&db, &guild, &member)
                        .await
                        .map_err(|err| err.to_string()),
                ),
            };
            let returned = clock.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(Completed {
                invoked,
                returned,
                outcome,
            });
        }));
    }
    drop(tx);
    let mut completed = Vec::with_capacity(taps.len());
    while let Some(done) = rx.recv().await {
        completed.push(done);
    }
    for handle in handles {
        handle.await.expect("tap task runs to completion");
    }
    assert_eq!(completed.len(), taps.len(), "every tap reports an outcome");
    completed
}

/// Assert every tap succeeded and return `(purge sums, opt-in removals)`.
/// A raced tap must end as a clean no-op outcome, never as a store error the
/// command layer could only render as a failure reply.
fn outcomes(completed: &[Completed]) -> (Vec<i64>, Vec<bool>) {
    let mut purges = Vec::new();
    let mut removals = Vec::new();
    for done in completed {
        match &done.outcome {
            Outcome::OptOut(Ok(purged)) => purges.push(*purged),
            Outcome::OptOut(Err(err)) => {
                panic!("every opt-out tap must complete cleanly, got error: {err}")
            }
            Outcome::OptIn(Ok(removed)) => removals.push(*removed),
            Outcome::OptIn(Err(err)) => {
                panic!("every opt-in tap must complete cleanly, got error: {err}")
            }
        }
    }
    (purges, removals)
}

/// Wing & Gong search over legal serial histories, pruning prefixes that
/// contradict real-time precedence or a reported outcome. The mixed herd
/// has six taps, so even fully overlapping calls admit at most 6! orders.
/// Initially the pair is unsuppressed with two seeded rows and no receipts.
fn serial_ends(completed: &[Completed]) -> Vec<(i64, bool)> {
    fn visit(
        completed: &[Completed],
        used: &mut [bool],
        suppressed: bool,
        rows: i64,
        receipts: i64,
        ends: &mut Vec<(i64, bool)>,
    ) {
        if used.iter().all(|used| *used) {
            let end = (receipts, suppressed);
            if !ends.contains(&end) {
                ends.push(end);
            }
            return;
        }
        for (index, done) in completed.iter().enumerate() {
            if used[index]
                || completed
                    .iter()
                    .enumerate()
                    .any(|(other, predecessor)| !used[other] && predecessor.returned < done.invoked)
            {
                continue;
            }
            let (next_suppressed, next_rows, next_receipts) = match &done.outcome {
                Outcome::OptOut(Ok(purged)) => {
                    let expected_purge = if suppressed { 0 } else { rows };
                    if *purged != expected_purge {
                        continue;
                    }
                    (true, 0, receipts + i64::from(!suppressed))
                }
                Outcome::OptIn(Ok(removed)) if *removed == suppressed => (false, rows, receipts),
                _ => continue,
            };
            used[index] = true;
            visit(
                completed,
                used,
                next_suppressed,
                next_rows,
                next_receipts,
                ends,
            );
            used[index] = false;
        }
    }

    let mut ends = Vec::new();
    visit(
        completed,
        &mut vec![false; completed.len()],
        false,
        2,
        0,
        &mut ends,
    );
    ends
}

#[test]
fn serial_history_oracle_checks_overlap_precedence_and_outcomes() {
    let opt_out = |invoked, returned, purged| Completed {
        invoked,
        returned,
        outcome: Outcome::OptOut(Ok(purged)),
    };
    let opt_in = |invoked, returned, removed| Completed {
        invoked,
        returned,
        outcome: Outcome::OptIn(Ok(removed)),
    };
    let cases = [
        (
            "overlapping calls may return in reverse serialization order",
            vec![opt_in(1, 2, true), opt_out(0, 3, 2)],
            vec![(1, false)],
        ),
        (
            "non-overlapping calls cannot be reordered to explain a removal",
            vec![opt_in(0, 1, true), opt_out(2, 3, 2)],
            vec![],
        ),
        (
            "a later suppression transition receipts without purging again",
            vec![opt_out(0, 1, 2), opt_in(2, 3, true), opt_out(4, 5, 0)],
            vec![(2, true)],
        ),
        (
            "the first opt-out must purge both seeded rows",
            vec![opt_out(0, 1, 0)],
            vec![],
        ),
        (
            "overlapping opt-outs cannot both purge the seeded rows",
            vec![opt_out(0, 3, 2), opt_out(1, 2, 2)],
            vec![],
        ),
        (
            "opt-in must report an existing suppression",
            vec![opt_out(0, 1, 2), opt_in(2, 3, false)],
            vec![],
        ),
        (
            "receipt counts and suppression stay paired across legal orders",
            vec![opt_out(0, 5, 2), opt_out(1, 4, 0), opt_in(2, 3, true)],
            vec![(1, false), (2, true)],
        ),
        (
            "a later transition cannot purge the seeded rows again",
            vec![opt_out(0, 1, 2), opt_in(2, 3, true), opt_out(4, 5, 2)],
            vec![],
        ),
        (
            "an already-suppressed opt-out is a no-op",
            vec![opt_out(0, 1, 2), opt_out(2, 3, 0)],
            vec![(1, true)],
        ),
    ];
    for (case, completed, expected) in cases {
        let mut actual = serial_ends(&completed);
        actual.sort_unstable();
        assert_eq!(actual, expected, "{case}");
    }
}

/// N parallel opt-out taps for one pair: exactly one purge, exactly one
/// receipt, suppression installed, and every tap completes cleanly — one
/// tap reports the purge, the remaining N-1 report that nothing was left
/// to purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_opt_out_taps_commit_exactly_one_purge_and_receipt() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    const MEMBER: &str = "123456789012345678";
    seed(&db, GUILD_A, MEMBER).await;

    let completed = herd(&db, GUILD_A, MEMBER, &[Tap::OptOut; 8]).await;
    let (purges, _) = outcomes(&completed);

    let purged_total: i64 = purges.iter().sum();
    assert_eq!(
        purged_total, 2,
        "the seeded events+members rows are purged exactly once across the herd: {purges:?}"
    );
    assert_eq!(
        purges.iter().filter(|purged| **purged > 0).count(),
        1,
        "exactly one tap performs the purge; the rest are no-ops: {purges:?}"
    );
    assert_eq!(
        receipts(&db, GUILD_A, MEMBER).await,
        1,
        "parallel taps write exactly one audit receipt"
    );
    assert!(
        is_opted_out(&db, GUILD_A, MEMBER).await.unwrap(),
        "suppression is installed after the herd"
    );
    assert_eq!(event_count(&db, GUILD_A, MEMBER).await, 0);

    db.close().await;
    test_db.close().await.unwrap();
}

/// A mixed herd must fit one serial order respecting non-overlapping taps,
/// including its per-tap outcomes, receipt count and final suppression.
/// Three rounds on distinct members; four opt-outs and two opt-ins demand
/// at most three receipts, so receipting every opt-out fails every round.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_mixed_taps_fit_a_real_time_serial_history() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    const TAPS: [Tap; 6] = [
        Tap::OptOut,
        Tap::OptIn,
        Tap::OptOut,
        Tap::OptOut,
        Tap::OptIn,
        Tap::OptOut,
    ];

    for round in 0..3_u64 {
        let member = (123_456_789_012_345_680_u64 + round).to_string();
        seed(&db, GUILD_A, &member).await;

        let completed = herd(&db, GUILD_A, &member, &TAPS).await;
        outcomes(&completed);

        let ends = serial_ends(&completed);
        assert!(
            !ends.is_empty(),
            "round {round}: every purge/removal must fit a serial order respecting \
             real-time precedence: {completed:?}"
        );
        let actual_receipts = receipts(&db, GUILD_A, &member).await;
        assert!(
            ends.iter().any(|(count, _)| *count == actual_receipts),
            "round {round}: {actual_receipts} receipt(s) must fit a serial order \
             with one receipt per suppression transition; serial ends {ends:?}"
        );
        let suppressed = is_opted_out(&db, GUILD_A, &member).await.unwrap();
        assert!(
            ends.contains(&(actual_receipts, suppressed)),
            "round {round}: receipt count and last-serialized suppression must \
             fit the same order; observed ({actual_receipts}, {suppressed}), \
             serial ends {ends:?}"
        );
        if suppressed {
            assert_eq!(
                event_count(&db, GUILD_A, &member).await,
                0,
                "round {round}: a herd ending opted out leaves no seeded rows"
            );
        }
    }

    db.close().await;
    test_db.close().await.unwrap();
}

/// Control: serialization is per (guild, member), never global. Parallel
/// first opt-outs for distinct pairs each purge and receipt; no pair is
/// turned into a no-op by another pair's tap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_opt_outs_for_distinct_pairs_each_purge_and_receipt() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    const MEMBER_A: &str = "123456789012345690";
    const MEMBER_B: &str = "123456789012345691";
    let pairs = [
        (GUILD_A, MEMBER_A),
        (GUILD_A, MEMBER_B),
        (GUILD_B, MEMBER_A),
    ];
    for (guild, member) in pairs {
        seed(&db, guild, member).await;
    }

    let barrier = Arc::new(Barrier::new(pairs.len()));
    let mut handles = Vec::with_capacity(pairs.len());
    for (guild, member) in pairs {
        let db = db.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            opt_out_member(&db, guild, member, ACTOR)
                .await
                .map(|counts| counts.iter().map(|count| count.rows).sum::<i64>())
        }));
    }
    for handle in handles {
        let purged = handle
            .await
            .expect("pair tap runs to completion")
            .expect("each distinct pair purges cleanly");
        assert_eq!(purged, 2, "each pair's seeded rows are purged");
    }
    for (guild, member) in pairs {
        assert_eq!(receipts(&db, guild, member).await, 1);
        assert!(is_opted_out(&db, guild, member).await.unwrap());
        assert_eq!(event_count(&db, guild, member).await, 0);
    }

    db.close().await;
    test_db.close().await.unwrap();
}

/// Control: parallel opt-in taps on a suppressed pair remove the suppression
/// exactly once — one tap reports the removal, the rest report nothing to
/// remove — and write no receipts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_opt_in_taps_remove_suppression_exactly_once() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    const MEMBER: &str = "123456789012345692";
    seed(&db, GUILD_A, MEMBER).await;
    opt_out_member(&db, GUILD_A, MEMBER, ACTOR)
        .await
        .expect("setup opt-out commits");
    assert!(is_opted_out(&db, GUILD_A, MEMBER).await.unwrap());
    assert_eq!(receipts(&db, GUILD_A, MEMBER).await, 1);

    let completed = herd(&db, GUILD_A, MEMBER, &[Tap::OptIn; 6]).await;
    let (_, removals) = outcomes(&completed);

    assert_eq!(
        removals.iter().filter(|removed| **removed).count(),
        1,
        "exactly one opt-in tap removes the suppression: {removals:?}"
    );
    assert!(
        !is_opted_out(&db, GUILD_A, MEMBER).await.unwrap(),
        "the herd ends opted in"
    );
    assert_eq!(
        receipts(&db, GUILD_A, MEMBER).await,
        1,
        "opt-in taps write no receipts"
    );

    db.close().await;
    test_db.close().await.unwrap();
}
