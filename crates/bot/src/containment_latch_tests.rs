#![cfg(test)]
//! Builder-owned behavioral pins for the latched containment plan: a latched
//! shutdown starts no further Discord step, an armed interrupt is uncertain
//! with the confirmed removals kept, and a disarmed interrupt still ends as a
//! dry run. Offline only: a mock Discord double and a closed database pool.

use super::*;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::tuning_test_support as support;
use serde_json::json;

const ROLES: &str = "GET /guilds/22/roles";
const EXECUTOR_READ: &str = "GET /guilds/22/members/5001";
const BOT_READ: &str = "GET /guilds/22/members/999";
const FIRST_REMOVAL: &str = "DELETE /guilds/22/members/5001/roles/77";
const SECOND_REMOVAL: &str = "DELETE /guilds/22/members/5001/roles/79";

struct Fixed;

impl SettingsSource for Fixed {
    async fn current(&mut self) -> ContainmentSettings {
        ContainmentSettings {
            window_ms: 60_000,
            max_age_ms: 3_600_000,
            heat_threshold: 5,
            staff_channel: None,
        }
    }
}

/// The plan's request trail as `METHOD /path`, in arrival order.
fn trail(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .into_iter()
        .map(|request| {
            let path = request
                .path
                .strip_prefix("/api/v10")
                .unwrap_or(&request.path);
            format!("{} {path}", request.method)
        })
        .collect()
}

/// Mock Discord for one executor (5001) holding two dangerous roles below the
/// bot's role. `latch_on` names the request whose arrival latches the shutdown
/// while it is being served, so the plan sees the latch the moment it returns.
async fn discord(shutdown: Arc<crate::raid_runtime::WorkerShutdown>, latch_on: &str) -> MockRest {
    let latch_on = latch_on.to_owned();
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request
            .path
            .strip_prefix("/api/v10")
            .unwrap_or(&request.path);
        let key = format!("{} {path}", request.method);
        if key == latch_on {
            shutdown.latch();
        }
        match key.as_str() {
            ROLES => ScriptedResponse::json(
                200,
                json!([
                    {"id":"22","permissions":"3072","position":0,"managed":false},
                    {"id":"77","permissions":"8","position":1,"managed":false},
                    {"id":"79","permissions":"8","position":2,"managed":false},
                    {"id":"88","permissions":"8","position":5,"managed":false},
                ]),
            ),
            BOT_READ => ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":["88"]})),
            EXECUTOR_READ => {
                ScriptedResponse::json(200, json!({"user":{"id":"5001"},"roles":["77","79"]}))
            }
            FIRST_REMOVAL | SECOND_REMOVAL => ScriptedResponse::status(204),
            // Unscripted routes fail the plan; each test asserts the exact
            // trail afterwards, so a stray request cannot pass silently.
            _ => ScriptedResponse::status(404),
        }
    })
    .await
}

async fn worker(
    mock: &MockRest,
    shutdown: &Arc<crate::raid_runtime::WorkerShutdown>,
    armed: bool,
) -> ContainmentWorker<Fixed> {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("latch-test-only-token".into(), Some(mock.origin())).unwrap();
    let (_, entries) = mpsc::channel(1);
    ContainmentWorker {
        guild_id: 22,
        settings: Fixed,
        store: ContainmentStore::from_pool(support::closed_pool().await),
        delivery: RaidDelivery::new(executor.clone(), 22),
        executor,
        trusted_user_ids: HashSet::new(),
        protected_user_ids: HashSet::new(),
        bot_user_id: "999".into(),
        armed,
        entries,
        shutdown: Arc::clone(shutdown),
    }
}

/// Plan once with the latch landing on `latch_on` (`""` latches up front).
async fn planned(
    armed: bool,
    latch_on: &str,
) -> (
    PlanOutcome,
    Vec<String>,
    Arc<crate::raid_runtime::WorkerShutdown>,
) {
    let shutdown = Arc::new(crate::raid_runtime::WorkerShutdown::default());
    let mock = discord(Arc::clone(&shutdown), latch_on).await;
    if latch_on.is_empty() {
        shutdown.latch();
    }
    let worker = worker(&mock, &shutdown, armed).await;
    let outcome = worker.plan("22", "5001", !armed).await;
    let requests = trail(&mock);
    mock.shutdown().await;
    (outcome, requests, shutdown)
}

/// An armed interrupt: nothing more is removed, the failure is `Shutdown`, and
/// the persisted state is uncertain whatever was confirmed removed so far.
fn assert_armed_interrupt(outcome: PlanOutcome, confirmed: &[&str]) {
    let PlanOutcome::Removed { removed, failure } = outcome else {
        unreachable!("an armed latched plan must report removals, not another outcome");
    };
    assert_eq!(removed, confirmed, "confirmed removals persist");
    assert!(
        matches!(failure, Some(QuarantineFailure::Shutdown)),
        "the interrupt must be the Shutdown failure"
    );
    assert_eq!(
        quarantine_outcome(&removed, failure),
        ContainmentIncidentState::Uncertain,
        "a latched armed incident is held uncertain, never refused or released"
    );
}

#[tokio::test]
async fn armed_latch_before_the_plan_starts_no_discord_step_and_is_uncertain() {
    let (outcome, requests, _) = planned(true, "").await;
    assert!(requests.is_empty(), "no read after the latch: {requests:?}");
    assert_armed_interrupt(outcome, &[]);
}

#[tokio::test]
async fn disarmed_latch_before_the_plan_ends_as_a_dry_run_not_uncertain() {
    let (outcome, requests, _) = planned(false, "").await;
    assert!(requests.is_empty(), "no read after the latch: {requests:?}");
    assert!(
        matches!(outcome, PlanOutcome::DryRun),
        "nothing is ever removed disarmed, so the end state is known"
    );
}

#[tokio::test]
async fn latch_during_the_executor_read_starts_no_bot_or_snapshot_read() {
    for armed in [true, false] {
        let (outcome, requests, _) = planned(armed, EXECUTOR_READ).await;
        assert_eq!(requests, [EXECUTOR_READ], "armed={armed}: {requests:?}");
        if armed {
            assert_armed_interrupt(outcome, &[]);
        } else {
            assert!(matches!(outcome, PlanOutcome::DryRun));
        }
    }
}

#[tokio::test]
async fn latch_during_the_bot_read_starts_no_snapshot_read() {
    for armed in [true, false] {
        let (outcome, requests, _) = planned(armed, BOT_READ).await;
        assert_eq!(
            requests,
            [EXECUTOR_READ, BOT_READ],
            "armed={armed}: {requests:?}"
        );
        if armed {
            assert_armed_interrupt(outcome, &[]);
        } else {
            assert!(matches!(outcome, PlanOutcome::DryRun));
        }
    }
}

#[tokio::test]
async fn latch_after_the_first_removal_keeps_it_and_stops_before_the_second() {
    let (outcome, requests, _) = planned(true, FIRST_REMOVAL).await;
    assert_eq!(
        requests,
        [EXECUTOR_READ, BOT_READ, ROLES, FIRST_REMOVAL],
        "the second removal must not start: {requests:?}"
    );
    assert_armed_interrupt(outcome, &["77"]);
}

#[tokio::test]
async fn unlatched_armed_plan_removes_every_role_as_the_control() {
    let (outcome, requests, shutdown) = planned(true, "never").await;
    assert!(!shutdown.is_latched());
    assert_eq!(
        requests,
        [
            EXECUTOR_READ,
            BOT_READ,
            ROLES,
            FIRST_REMOVAL,
            SECOND_REMOVAL
        ]
    );
    let PlanOutcome::Removed { removed, failure } = outcome else {
        unreachable!("an armed plan with dangerous roles reports removals");
    };
    assert_eq!(removed, ["77", "79"]);
    assert!(failure.is_none());
}
