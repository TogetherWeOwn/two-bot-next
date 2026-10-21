#![cfg(test)]
//! Protected acceptance tests: interaction backpressure on the RSVP path.
//!
//! PROTECTED FILE: the paired Builder implements the production bound
//! without editing this file. The only wiring outside it is the
//! `mod backpressure_acceptance;` line in `gateway_tests.rs`, plus
//! `pub(super)` visibility on the four `rsvp.rs` harness helpers reused
//! here (`connect`, `global_series`, `interaction`, `lane_holding_rest`;
//! no behavior change to that file).
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

use super::rsvp::{connect, global_series, interaction, lane_holding_rest};
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
    let (runner, mut ws) = connect(&db, &rest).await;
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
    // Survival: ending reception drains admitted work and exits cleanly;
    // refusals must never wedge the funnel, checkpoint, or worker.
    drop(ws);
    tokio::time::timeout(std::time::Duration::from_secs(20), runner)
        .await
        .expect("runner exits after the burst")
        .unwrap()
        .unwrap();
    rest.shutdown().await;
    db.close().await;
}
