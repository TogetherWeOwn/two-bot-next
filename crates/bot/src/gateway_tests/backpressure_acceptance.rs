#![cfg(test)]
//! Protected acceptance tests: interaction backpressure on the RSVP path.
//!
//! PROTECTED FILE: the paired Builder implements the production bound
//! without editing this file. The only wiring outside it is the
//! `mod backpressure_acceptance;` line in `gateway_tests.rs`, plus
//! `pub(super)` visibility on the four `rsvp.rs` harness helpers reused
//! here (`connect_with_shutdown`, `global_series`, `interaction`,
//! `lane_holding_rest`; no behavior change to that file).
//!
//! Contract under test:
//!
//! - A burst of [`BURST`] interactions against a stalled RSVP drain is
//!   refused past [`MAX_RSVP_IN_FLIGHT`]: the
//!   `two_bot_gateway_rsvp_refused_total` counter grows by at least the
//!   excess. Today nothing refuses, so the burst test fails on the
//!   counted-refusals assertion (behavioral RED, not a compile failure).
//! - The refused-counter series is rendered: a fresh `Metrics` exposition
//!   contains `two_bot_gateway_rsvp_refused_total`. This unignored,
//!   offline pin fails today (series absent) and passes once the Builder
//!   adds the counter, independent of the database-backed burst test.
//! - The runner survives the burst and the healthy path is unchanged: the
//!   first interaction still completes with its exact user-facing content.
//!
//! Builder instructions:
//!
//! - Gate the per-interaction prepare spawn with a permit cap of at most
//!   [`MAX_RSVP_IN_FLIGHT`] (the `AckPermit` fail-closed shape already used
//!   for detached onboarding ACKs), and/or bound the RSVP drain channel
//!   with `try_send` refusal. Either gate satisfies these tests, but every
//!   refusal from either gate must bump the counter exactly once; the
//!   burst test pins the COUNT, not the mechanism. Spawn-gating is
//!   preferred (it also bounds spawned tasks, not just buffered handles).
//! - Add `Metrics::gateway_rsvp_refused(&self)` rendering label-free
//!   `two_bot_gateway_rsvp_refused_total` (u64, default 0), following the
//!   existing `gateway_disconnect` counter shape.
//! - Refusal must advance reception: a refused interaction never wedges
//!   the funnel, the checkpoint, or the dispatch worker. The survival
//!   assertion pins a clean runner exit after the burst.
//! - Healthy path unchanged: a lone interaction still defers promptly and
//!   completes with identical content (pinned below for the burst head).
//!
//! Sizing: [`BURST`] (40) sits above the ceiling (32) and well below the
//! fatal `DISPATCH_BACKLOG` (64), so the burst exercises the RSVP bound
//! without tripping the unrelated fatal funnel overflow. The 3.2 s REST
//! hold stalls the serial drain while the burst arrives; the refusal poll
//! resolves in about a second, inside the hold window.

use super::rsvp::{
    connect_with_shutdown, connect_with_shutdown_and_heartbeat, global_series, interaction,
    lane_holding_rest,
};
use super::*;
use crate::discord_test_common::MockRest;
use std::sync::atomic::AtomicBool;
use two_bot_core::metrics::Metrics;

/// Ceiling on admitted in-flight RSVP work per burst. Matches the existing
/// `ACK_LIMIT` (32) for detached acknowledgements; the Builder may choose a
/// smaller cap. The burst test asserts a lower bound on refusals, so any
/// smaller cap still passes.
const MAX_RSVP_IN_FLIGHT: usize = 32;
/// Burst sized to overflow any compliant cap while staying below the fatal
/// `DISPATCH_BACKLOG` (64).
const BURST: u64 = 40;
const REFUSED_SERIES: &str = "two_bot_gateway_rsvp_refused_total ";

#[test]
fn rsvp_refused_counter_series_is_rendered() {
    // Offline pin for the counted-refusals half of the contract: the series
    // must exist in a fresh exposition (at zero). Today it is absent, so
    // this fails; it passes once the Builder adds the counter, with no
    // database and no shard involved.
    let text = Metrics::default().render(None);
    assert!(
        text.contains(REFUSED_SERIES),
        "fresh exposition must render the RSVP refused counter:\n{text}"
    );
}

fn callback_attempts(rest: &MockRest) -> usize {
    rest.requests()
        .iter()
        .filter(|request| request.path.contains("/callback"))
        .count()
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn rsvp_burst_against_stalled_drain_refuses_and_counts() {
    // Stall the serial RSVP drain with a held REST read, burst 40
    // interactions, and require counted refusals for the excess over the
    // 32 cap. Reception-time refusal is synchronous, so the poll below
    // observes every defer-or-refuse decision while the hold is still up.
    let db = TestDb::new().await;
    let rest = lane_holding_rest(
        std::sync::Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_millis(3200),
    )
    .await;
    let refused_before = global_series(REFUSED_SERIES);
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) = connect_with_shutdown(&db, &rest, Some(receiver)).await;
    for sequence in 2..2 + BURST {
        ws.send(Message::text(interaction(sequence, "going").to_string()))
            .await
            .unwrap();
    }
    // Every burst member is either deferred (a /callback attempt) or
    // refused (counter bump). Poll until all 40 decisions are visible.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let decided = callback_attempts(&rest)
                + (global_series(REFUSED_SERIES) - refused_before) as usize;
            if decided >= BURST as usize {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("burst decisions visible while the drain is stalled");
    let refused = global_series(REFUSED_SERIES) - refused_before;
    let minimum = (BURST as usize - MAX_RSVP_IN_FLIGHT) as u64;
    assert!(
        refused >= minimum,
        "burst of {BURST} against a stalled drain with a cap of at most {MAX_RSVP_IN_FLIGHT} \
         must count at least {minimum} refusals, counted {refused}"
    );
    // Healthy head: the first interaction still completes with its exact
    // user-facing content once the hold releases.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let done = rest
                .requests()
                .iter()
                .any(|request| request.method == "PATCH" && request.path.contains("mock-rsvp-2"));
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("first interaction completes after the hold releases");
    let requests = rest.requests();
    let edits: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.contains("mock-rsvp-2"))
        .collect();
    assert_eq!(edits.len(), 1, "one completion edit for the burst head");
    assert_eq!(
        serde_json::from_slice::<Value>(&edits[0].body).unwrap()["content"],
        "RSVP saved: going."
    );
    // Survival: cooperative shutdown drains admitted work and exits cleanly;
    // refusals must never wedge the funnel, checkpoint, or worker. A bare
    // `drop(ws)` cannot end reception (the shard maps a server-side drop to
    // `Err(Reconnect)` and retries forever), so signal shutdown first and
    // drop the socket after, like every `rsvp.rs` sibling. The 60 s bound
    // covers the ~32 admitted dispatches draining under lane contention.
    shutdown.send_replace(true);
    tokio::time::timeout(std::time::Duration::from_secs(60), runner)
        .await
        .expect("runner exits after the burst")
        .unwrap()
        .unwrap();
    drop(ws);
    rest.shutdown().await;
    db.close().await;
}

fn mixed_rsvp(sequence: u64, status: &str) -> Value {
    let mut packet = interaction(sequence, status);
    // Keep the per-member write-rate budget out of the ingress-capacity test.
    packet["d"]["member"]["user"]["id"] = json!((70_000 + sequence).to_string());
    packet
}

fn mixed_slash(sequence: u64, name: &str) -> Value {
    let mut packet = interaction(sequence, "going");
    packet["d"]["token"] = json!(format!("mock-other-{sequence}"));
    packet["d"]["member"]["permissions"] = json!("32");
    packet["d"]["data"]["name"] = json!(name);
    packet["d"]["data"]["options"] = json!([]);
    packet
}

fn mixed_component(sequence: u64) -> Value {
    let mut packet = mixed_slash(sequence, "not-rsvp");
    packet["d"]["type"] = json!(3);
    packet["d"]["data"] = json!({
        "custom_id": "not-rsvp-acceptance-probe", "component_type": 2
    });
    packet
}

fn token_requests(rest: &MockRest, method: &str, suffix: &str) -> usize {
    rest.requests()
        .iter()
        .filter(|request| request.method == method && request.path.ends_with(suffix))
        .count()
}

async fn mixed_receipt(
    rest: &MockRest,
    runner: &JoinHandle<Result<(), sqlx::Error>>,
    method: &str,
    suffix: &str,
    refused_before: u64,
) -> Result<(), String> {
    // The outer stalled-phase deadline bounds every wait together. In
    // particular a capacity mutant must not wait through 32 separate budgets.
    loop {
        let refused = global_series(REFUSED_SERIES) - refused_before;
        if refused != 0 {
            return Err(format!(
                "unrelated traffic consumed RSVP capacity or counted a refusal: {refused}"
            ));
        }
        if runner.is_finished() {
            return Err("runner stopped before the stalled phase completed".to_owned());
        }
        if token_requests(rest, method, suffix) == 1 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI disposable test database"]
async fn real_gateway_mixed_traffic_does_not_spend_rsvp_capacity_or_refusals() {
    // Breaks: candidate=true at run_shard reception consumes slots for
    // ignored prepares; unconditional refusal counting charges bypasses.
    // Neither a pure classifier assertion nor all-callback counting can
    // detect both mistakes. Hold actual SQL work until both phases finish.
    // Exclusive TestDb fences all gateway peers so exact global deltas are
    // meaningful (including in the serial ignored-runtime CI lane).
    let db = TestDb::exclusive_with_pool_max(8).await;
    let rest = lane_holding_rest(Arc::new(AtomicBool::new(false)), Duration::ZERO).await;
    let refused_before = global_series(REFUSED_SERIES);
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let (runner, mut ws) =
        connect_with_shutdown_and_heartbeat(&db, &rest, Some(receiver), 600_000).await;
    let lock_pool = db.independent_pool(1).await;
    let mut lock = lock_pool.begin().await.unwrap();
    let locker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE event_rsvps IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let admitted: Vec<u64> = std::iter::once(2).chain(6..=36).collect();
    assert_eq!(admitted.len(), MAX_RSVP_IN_FLIGHT);
    // READY=1, 32 admitted RSVPs, six unrelated packets, eight true
    // overflows: 46 dispatches total. Even with NO funnel drain this is
    // below the independent fatal DISPATCH_BACKLOG=64.
    let phase = tokio::time::timeout(Duration::from_secs(15), async {
        ws.send(Message::text(mixed_rsvp(2, "going").to_string()))
            .await
            .map_err(|_| "head packet send failed")?;
        mixed_receipt(
            &rest,
            &runner,
            "POST",
            "/interactions/2/mock-rsvp-2/callback",
            refused_before,
        )
        .await?;
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'relation' \
                 AND relation = 'event_rsvps'::regclass AND NOT granted \
                 AND $1::integer = ANY(pg_blocking_pids(pid)))",
            )
            .bind(locker)
            .fetch_one(&db.pool)
            .await
            .map_err(|_| "lock waiter query failed")?;
            if waiting {
                break;
            }
            if runner.is_finished() {
                return Err("runner stopped before SQL stall".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Both phases run while the head holds the serial RSVP drain. A
        // sticky validation completion is a reception barrier after unknown
        // slash/component packets; those unknown packets need not reply.
        for (slash, component, barrier) in [(3, 4, 5), (37, 38, 39)] {
            ws.send(Message::text(
                mixed_slash(slash, "not-rsvp-acceptance-probe").to_string(),
            ))
            .await
            .map_err(|_| "unrelated slash send failed")?;
            ws.send(Message::text(mixed_component(component).to_string()))
                .await
                .map_err(|_| "unrelated component send failed")?;
            ws.send(Message::text(mixed_slash(barrier, "sticky").to_string()))
                .await
                .map_err(|_| "sticky barrier send failed")?;
            mixed_receipt(
                &rest,
                &runner,
                "POST",
                &format!("/interactions/{barrier}/mock-other-{barrier}/callback"),
                refused_before,
            )
            .await?;
            mixed_receipt(
                &rest,
                &runner,
                "PATCH",
                &format!("/webhooks/1111/mock-other-{barrier}/messages/@original"),
                refused_before,
            )
            .await?;
            if slash == 3 {
                for (index, sequence) in admitted.iter().copied().enumerate().skip(1) {
                    let status = if index % 2 == 0 {
                        "going"
                    } else {
                        "interested"
                    };
                    ws.send(Message::text(mixed_rsvp(sequence, status).to_string()))
                        .await
                        .map_err(|_| "admitted RSVP send failed")?;
                    mixed_receipt(
                        &rest,
                        &runner,
                        "POST",
                        &format!("/interactions/{sequence}/mock-rsvp-{sequence}/callback"),
                        refused_before,
                    )
                    .await?;
                }
            }
            if global_series(REFUSED_SERIES) != refused_before {
                return Err("non-RSVP packets added RSVP refusals".to_owned());
            }
        }
        for sequence in &admitted {
            if token_requests(
                &rest,
                "PATCH",
                &format!("/webhooks/1111/mock-rsvp-{sequence}/messages/@original"),
            ) != 0
            {
                return Err("RSVP drain was not stalled through saturation".to_owned());
            }
        }
        for sequence in 40..=47 {
            ws.send(Message::text(mixed_rsvp(sequence, "going").to_string()))
                .await
                .map_err(|_| "overflow RSVP send failed")?;
        }
        loop {
            let refused = global_series(REFUSED_SERIES) - refused_before;
            if refused >= 8 {
                if refused != 8 {
                    return Err(format!(
                        "expected only eight genuine RSVP refusals, got {refused}"
                    ));
                }
                break;
            }
            if runner.is_finished() {
                return Err("runner stopped on genuine overflow".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok::<(), String>(())
    })
    .await;
    // Always request cooperative shutdown, even when a mutation caused an
    // early phase error. Keep the websocket open until accepted work drains.
    shutdown.send_replace(true);
    lock.rollback().await.unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(60), runner).await;
    drop(ws);
    let phase = phase.expect("whole stalled mixed-traffic phase deadline");
    phase.expect("mixed traffic must not consume RSVP capacity or refusal counts");
    drained
        .expect("cooperative mixed-traffic drain deadline")
        .unwrap()
        .unwrap();
    assert_eq!(global_series(REFUSED_SERIES) - refused_before, 8);
    let requests = rest.requests();
    let mut completed = Vec::new();
    for (index, sequence) in admitted.iter().copied().enumerate() {
        let callback = format!("/interactions/{sequence}/mock-rsvp-{sequence}/callback");
        let callbacks: Vec<_> = requests
            .iter()
            .filter(|r| r.path.ends_with(&callback))
            .collect();
        assert_eq!(callbacks.len(), 1, "one defer per admitted RSVP {sequence}");
        assert_eq!(
            serde_json::from_slice::<Value>(&callbacks[0].body).unwrap()["type"],
            5
        );
        let original = format!("/webhooks/1111/mock-rsvp-{sequence}/messages/@original");
        let edits: Vec<_> = requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.path.ends_with(&original))
            .collect();
        assert_eq!(
            edits.len(),
            1,
            "one completion per admitted RSVP {sequence}"
        );
        let expected = if index % 2 == 0 {
            "RSVP saved: going."
        } else {
            "RSVP saved: interested."
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&edits[0].body).unwrap()["content"],
            expected
        );
        completed.push(
            requests
                .iter()
                .position(|r| std::ptr::eq(r, edits[0]))
                .unwrap(),
        );
    }
    assert!(
        completed.windows(2).all(|pair| pair[0] < pair[1]),
        "admitted RSVP completion order"
    );
    for sequence in 40..=47 {
        assert_eq!(
            token_requests(
                &rest,
                "POST",
                &format!("/interactions/{sequence}/mock-rsvp-{sequence}/callback")
            ),
            0,
            "overflow must not defer"
        );
        assert_eq!(
            token_requests(
                &rest,
                "PATCH",
                &format!("/webhooks/1111/mock-rsvp-{sequence}/messages/@original")
            ),
            0,
            "overflow must not execute"
        );
    }
    for sequence in [5, 39] {
        assert_eq!(
            token_requests(
                &rest,
                "PATCH",
                &format!("/webhooks/1111/mock-other-{sequence}/messages/@original")
            ),
            1,
            "unrelated validation completes even at saturation"
        );
    }
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 47);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id, status FROM event_rsvps ORDER BY user_id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 32, "one durable RSVP per admitted human");
    for (index, sequence) in admitted.iter().copied().enumerate() {
        assert_eq!(rows[index].0, (70_000 + sequence).to_string());
        assert_eq!(
            rows[index].1,
            if index % 2 == 0 {
                "going"
            } else {
                "interested"
            }
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        32
    );
    rest.shutdown().await;
    lock_pool.close().await;
    db.close().await;
}
