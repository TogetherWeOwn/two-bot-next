//! Protected acceptance follow-up: poll quotas, input fences and delivery recovery.
//! The implementation owner must not edit this file to make a regression green.
//! All persistence is real, isolated Postgres; only the Discord HTTP boundary is doubled.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{MockRest, ResponseGate, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use tokio::sync::Barrier;
use tokio::time::timeout;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1660102030405060701";
const FOREIGN: &str = "1660102030405060799";
const CHANNEL: &str = "1660102030405060702";
const AUTHOR: &str = "1660102030405060721";
const VOTER: &str = "1660102030405060722";
const WAIT: Duration = Duration::from_secs(10);

async fn fixture() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure poll delivery acceptance: TWO_TEST_DATABASE_URL not set"
            );
            eprintln!("skipped poll delivery acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable database"),
    )
}

fn runtime(pool: &PgPool, mock: &MockRest, guild: &str, enabled: bool) -> InteractionRuntime {
    with_executor(
        pool,
        ActionExecutor::with_proxy("synthetic-poll-token".to_owned(), Some(mock.origin())).unwrap(),
        guild,
        enabled,
    )
}

fn with_executor(
    pool: &PgPool,
    executor: ActionExecutor,
    guild: &str,
    enabled: bool,
) -> InteractionRuntime {
    InteractionRuntime::with_router(
        InteractionRouter::new(RouterGates {
            configured_guild: Some(guild.parse().unwrap()),
            announcements: enabled,
            scorecard: false,
            automations: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        pool.clone(),
        executor,
        0,
        ClassifierConfig::default(),
    )
}

fn slash(id: u64, guild: &str, author: &str, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":guild},
        "id":id.to_string(), "token":"synthetic-poll-callback", "type":2, "version":1,
        "guild_id":guild, "channel":{"id":CHANNEL,"type":0},
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":author,"username":"fixture","discriminator":"0","bot":false}},
        "data":{"id":"4444","name":"poll","type":1,"options":options}
    }))
    .expect("synthetic slash packet")
}

fn create_args(question: &str) -> Value {
    json!([{"name":"create","type":1,"options":[
        {"name":"question","type":3,"value":question},
        {"name":"option1","type":3,"value":"one"},
        {"name":"option2","type":3,"value":"two"},
        {"name":"duration-minutes","type":4,"value":1}
    ]}])
}

fn close_args(id: &str) -> Value {
    json!([{"name":"close","type":1,"options":[{"name":"id","type":3,"value":id}]}])
}

fn button(id: u64, guild: &str, poll: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":guild},
        "id":id.to_string(), "token":"synthetic-poll-callback", "type":3,"version":1,
        "guild_id":guild,"channel":{"id":CHANNEL,"type":0},
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":VOTER,"username":"fixture","discriminator":"0","bot":false}},
        "data":{"custom_id":format!("poll:{poll}:0"),"component_type":2}
    }))
    .expect("synthetic button packet")
}

fn channel_post(request: &RestRequest) -> bool {
    request.method == "POST" && request.path == format!("/api/v10/channels/{CHANNEL}/messages")
}

fn reply(request: &RestRequest) -> ScriptedResponse {
    if request.path.contains("/interactions/") {
        ScriptedResponse::status(204)
    } else {
        ScriptedResponse::json(200, json!({"id":"99"}))
    }
}

async fn mock() -> MockRest {
    MockRest::with_responder(reply).await
}

async fn create(pool: &PgPool, mock: &MockRest, id: u64, question: &str) -> String {
    assert!(runtime(pool, mock, GUILD, true)
        .handle(&slash(id, GUILD, AUTHOR, create_args(question)))
        .await
        .expect("create executes"));
    sqlx::query_scalar("SELECT id FROM polls WHERE guild_id=$1 AND question=$2")
        .bind(GUILD)
        .bind(question)
        .fetch_one(pool)
        .await
        .expect("created row")
}

async fn expire(pool: &PgPool, id: &str) {
    sqlx::query("UPDATE polls SET ends_at=now()-interval '1 minute' WHERE guild_id=$1 AND id=$2")
        .bind(GUILD)
        .bind(id)
        .execute(pool)
        .await
        .expect("expire fixture poll");
}

async fn votes(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE guild_id=$1 AND poll_id=$2")
        .bind(GUILD)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("durable vote count")
}

async fn open(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM polls WHERE guild_id=$1 AND status='open'")
        .bind(GUILD)
        .fetch_one(pool)
        .await
        .expect("open quota")
}

async fn fence(pool: &PgPool, id: &str) -> (bool, bool, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT results_started_at IS NOT NULL, results_posted_at IS NOT NULL,
        results_message_id, counts_json::text FROM polls WHERE guild_id=$1 AND id=$2",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("durable result fence")
}

// Removing the shared creation lock must over-admit concurrent independent handlers.
#[tokio::test]
async fn concurrent_creates_respect_author_and_guild_open_quotas() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    for same_author in [true, false] {
        let barrier = Arc::new(Barrier::new(16));
        let mut tasks = Vec::new();
        for n in 0..16 {
            let rt = runtime(db.pool(), &mock, GUILD, true);
            let barrier = barrier.clone();
            let author = if same_author {
                AUTHOR.to_owned()
            } else {
                (1660102030405060800u64 + n).to_string()
            };
            let packet = slash(
                7000 + n + if same_author { 0 } else { 100 },
                GUILD,
                &author,
                create_args(&format!("race {same_author} {n}")),
            );
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                rt.handle(&packet).await
            }));
        }
        for task in tasks {
            assert!(timeout(WAIT, task)
                .await
                .expect("bounded create race")
                .expect("task")
                .is_ok());
        }
        let count = open(db.pool()).await;
        let want = if same_author { 3 } else { 10 };
        assert_eq!(
            count, want,
            "concurrent requests must fill, never exceed, the quota"
        );
        if same_author {
            sqlx::query(
                "UPDATE polls SET status='closed', closed_at=now(), results_posted_at=now()",
            )
            .execute(db.pool())
            .await
            .expect("release first race's quota");
        }
    }
    db.close().await.expect("drop fixture");
}

// Removing either configured-guild or announcements admission must create/vote/close here.
#[tokio::test]
async fn foreign_guild_and_disabled_gate_create_vote_and_tick_are_inert() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let id = create(db.pool(), &mock, 7200, "fenced").await;
    for (guild, enabled, incoming) in [(GUILD, false, GUILD), (GUILD, true, FOREIGN)] {
        let rt = runtime(db.pool(), &mock, guild, enabled);
        for packet in [
            slash(7201, incoming, AUTHOR, create_args("not admitted")),
            button(7202, incoming, &id),
            slash(7203, incoming, AUTHOR, close_args(&id)),
        ] {
            assert!(rt.handle(&packet).await.is_ok());
        }
    }
    expire(db.pool(), &id).await;
    let before = mock.requests().len();
    runtime(db.pool(), &mock, GUILD, false)
        .tick_polls()
        .await
        .expect("disabled tick");
    runtime(db.pool(), &mock, FOREIGN, true)
        .tick_polls()
        .await
        .expect("foreign tick");
    let after = mock.requests().len();
    let count = open(db.pool()).await;
    let durable = votes(db.pool(), &id).await;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM polls")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await.expect("drop fixture");
    assert_eq!((total, count, durable, after - before), (1, 1, 0, 0));
}

// Reject malformed argument sets rather than accepting the first matching field.
#[tokio::test]
async fn unknown_duplicate_sparse_and_mistyped_create_arguments_refuse_without_reservation() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let valid = create_args("bad input");
    let mut cases = Vec::new();
    let mut unknown = valid.clone();
    unknown[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"surprise","type":3,"value":"x"}));
    cases.push(unknown);
    let mut duplicate = valid.clone();
    duplicate[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"question","type":3,"value":"duplicate"}));
    cases.push(duplicate);
    let mut sparse = valid.clone();
    sparse[0]["options"][2]["name"] = json!("option3");
    cases.push(sparse);
    let mut mistyped = valid.clone();
    mistyped[0]["options"][0] = json!({"name":"question","type":4,"value":7});
    cases.push(mistyped);
    let mut duration = valid.clone();
    duration[0]["options"][3] = json!({"name":"duration-minutes","type":3,"value":"1"});
    cases.push(duration);
    let mut missing = valid.clone();
    missing[0]["options"].as_array_mut().unwrap().remove(3);
    cases.push(missing);
    let mut twice = valid.clone();
    twice.as_array_mut().unwrap().push(valid[0].clone());
    cases.push(twice);
    let mut zero = valid.clone();
    zero[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"option0","type":3,"value":"extra"}));
    cases.push(zero);
    let mut eleven = valid.clone();
    eleven[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"option11","type":3,"value":"extra"}));
    cases.push(eleven);
    for (n, args) in cases.into_iter().enumerate() {
        assert!(runtime(db.pool(), &mock, GUILD, true)
            .handle(&slash(7300 + n as u64, GUILD, AUTHOR, args))
            .await
            .is_ok());
        assert_eq!(
            open(db.pool()).await,
            0,
            "malformed create case {n} reserved quota"
        );
    }
    assert!(!mock.requests().iter().any(channel_post));
    db.close().await.expect("drop fixture");
}

#[tokio::test]
async fn unknown_duplicate_and_mistyped_close_arguments_do_not_close() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let id = create(db.pool(), &mock, 7400, "remain open").await;
    let valid = close_args(&id);
    let mut unknown = valid.clone();
    unknown[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"extra","type":3,"value":"x"}));
    let mut duplicate = valid.clone();
    duplicate[0]["options"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"id","type":3,"value":id}));
    let wrong = json!([{"name":"close","type":1,"options":[{"name":"id","type":4,"value":42}]}]);
    let before = mock.requests().len();
    for (n, args) in [
        unknown,
        duplicate,
        wrong,
        json!([{"name":"close","type":1,"options":[]}]),
    ]
    .into_iter()
    .enumerate()
    {
        runtime(db.pool(), &mock, GUILD, true)
            .handle(&slash(7401 + n as u64, GUILD, AUTHOR, args))
            .await
            .expect("invalid close executes");
        assert_eq!(
            open(db.pool()).await,
            1,
            "invalid close case {n} changed state"
        );
    }
    assert!(!mock.requests()[before..].iter().any(channel_post));
    db.close().await.expect("drop fixture");
}

// Count Unicode scalar values, not bytes; trim before checking the bounds.
#[tokio::test]
async fn unicode_boundaries_accept_exact_limits_and_refuse_overflow() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let mut exact = create_args(&format!("  {}  ", "🦀".repeat(256)));
    exact[0]["options"][1]["value"] = json!(format!(" {} ", "界".repeat(80)));
    let rt = runtime(db.pool(), &mock, GUILD, true);
    rt.handle(&slash(7500, GUILD, AUTHOR, exact))
        .await
        .expect("Unicode boundary create");
    let row = sqlx::query("SELECT question, options_json::text AS choices FROM polls")
        .fetch_one(db.pool())
        .await
        .expect("Unicode at bounds accepted");
    assert_eq!(row.get::<String, _>("question"), "🦀".repeat(256));
    let choices: Value = serde_json::from_str(row.get::<&str, _>("choices")).unwrap();
    assert_eq!(choices[0], "界".repeat(80));
    let mut over_choice = create_args("option overflow");
    over_choice[0]["options"][1]["value"] = json!("界".repeat(81));
    for (n, args) in [
        create_args(&"🦀".repeat(257)),
        over_choice,
        create_args("\u{2003}\n\t"),
    ]
    .into_iter()
    .enumerate()
    {
        rt.handle(&slash(7501 + n as u64, GUILD, AUTHOR, args))
            .await
            .expect("overflow refusal");
    }
    assert_eq!(open(db.pool()).await, 1);
    db.close().await.expect("drop fixture");
}

// A confirmed HTTP receipt must settle durable success, not leave a replayable fence.
#[tokio::test]
async fn confirmed_result_clears_only_inflight_fence_and_is_durable_across_restart() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let id = create(db.pool(), &mock, 7600, "confirmed receipt").await;
    expire(db.pool(), &id).await;
    runtime(db.pool(), &mock, GUILD, true)
        .tick_polls()
        .await
        .expect("close");
    let before = fence(db.pool(), &id).await;
    let sent = mock.requests().len();
    runtime(db.pool(), &mock, GUILD, true)
        .tick_polls()
        .await
        .expect("restart");
    let after = fence(db.pool(), &id).await;
    let requests = mock.requests().len();
    db.close().await.expect("drop fixture");
    assert_eq!(
        before,
        (
            false,
            true,
            Some("99".to_owned()),
            Some("[0, 0]".to_owned())
        )
    );
    assert_eq!(after, before);
    assert_eq!(requests, sent, "durable posted marker must prevent replay");
}

// Losing the receipt after Discord accepted results must never replay automatically.
#[tokio::test]
async fn ambiguous_results_receipt_keeps_fence_and_concurrent_recoverers_skip() {
    let Some(db) = fixture().await else { return };
    let good = mock().await;
    let id = create(db.pool(), &good, 7700, "ambiguous results").await;
    expire(db.pool(), &id).await;
    let ambiguous = MockRest::with_responder(|r| {
        if channel_post(r) {
            ScriptedResponse::json(200, json!({"id":"101"})).truncated_body()
        } else {
            reply(r)
        }
    })
    .await;
    let _ = runtime(db.pool(), &ambiguous, GUILD, true)
        .tick_polls()
        .await;
    let armed = fence(db.pool(), &id).await;
    let before = good.requests().len();
    let a = runtime(db.pool(), &good, GUILD, true);
    let b = runtime(db.pool(), &good, GUILD, true);
    let (a, b) = tokio::join!(a.tick_polls(), b.tick_polls());
    assert!(a.is_ok() && b.is_ok());
    let after = fence(db.pool(), &id).await;
    let posts = ambiguous
        .requests()
        .iter()
        .filter(|r| channel_post(r))
        .count();
    let replay = good.requests().len() - before;
    db.close().await.expect("drop fixture");
    assert_eq!(
        posts, 1,
        "ambiguous POST must not retry inside the executor"
    );
    assert!(armed.0 && !armed.1);
    assert_eq!(after, armed);
    assert_eq!(replay, 0, "recoverers must skip a durable ambiguous fence");
}

// Both an active sender and a crash before its response must retain the same fence.
#[tokio::test]
async fn cancellation_after_durable_results_started_never_replays() {
    let Some(db) = fixture().await else { return };
    let good = mock().await;
    let id = create(db.pool(), &good, 7800, "cancelled results").await;
    expire(db.pool(), &id).await;
    let gate = Arc::new(ResponseGate::default());
    let selected = gate.clone();
    let held = MockRest::with_gated_responder(move |r| {
        (reply(r), channel_post(r).then(|| selected.clone()))
    })
    .await;
    let rt = runtime(db.pool(), &held, GUILD, true);
    let task = tokio::spawn(async move { rt.tick_polls().await });
    timeout(WAIT, gate.wait_for_request())
        .await
        .expect("results POST reached");
    let armed = fence(db.pool(), &id).await;
    let before = good.requests().len();
    runtime(db.pool(), &good, GUILD, true)
        .tick_polls()
        .await
        .expect("active competitor skips");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    gate.release();
    runtime(db.pool(), &good, GUILD, true)
        .tick_polls()
        .await
        .expect("restart skips");
    let after = fence(db.pool(), &id).await;
    let replay = good.requests().len() - before;
    db.close().await.expect("drop fixture");
    assert!(
        armed.0 && !armed.1,
        "send starts only after the durable fence commits"
    );
    assert_eq!(after, armed);
    assert_eq!(replay, 0);
}

// A committed pre-send fence is not proof of absence even when no HTTP was made.
#[tokio::test]
async fn concurrent_recovery_of_presend_fence_is_inert() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let id = create(db.pool(), &mock, 7900, "pre-send crash").await;
    sqlx::query("UPDATE polls SET status='closed',closed_at=now(),counts_json='[0,0]'::jsonb,results_started_at=now() WHERE id=$1")
        .bind(&id).execute(db.pool()).await.unwrap();
    let before = mock.requests().len();
    let a = runtime(db.pool(), &mock, GUILD, true);
    let b = runtime(db.pool(), &mock, GUILD, true);
    let (a, b) = tokio::join!(a.tick_polls(), b.tick_polls());
    assert!(a.is_ok() && b.is_ok());
    let after = fence(db.pool(), &id).await;
    let calls = mock.requests().len() - before;
    db.close().await.expect("drop fixture");
    assert!(after.0 && !after.1);
    assert_eq!(calls, 0);
}

// Frozen aggregates survive erasure of the underlying voter row and any later tick.
#[tokio::test]
async fn frozen_final_counts_survive_voter_erasure() {
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let id = create(db.pool(), &mock, 8000, "freeze counts").await;
    runtime(db.pool(), &mock, GUILD, true)
        .handle(&button(8001, GUILD, &id))
        .await
        .expect("vote");
    assert_eq!(votes(db.pool(), &id).await, 1);
    expire(db.pool(), &id).await;
    runtime(db.pool(), &mock, GUILD, true)
        .tick_polls()
        .await
        .expect("close freezes aggregate");
    let before = fence(db.pool(), &id).await;
    sqlx::query("DELETE FROM poll_votes WHERE guild_id=$1 AND user_id=$2")
        .bind(GUILD)
        .bind(VOTER)
        .execute(db.pool())
        .await
        .expect("erase fixture voter rows");
    runtime(db.pool(), &mock, GUILD, true)
        .tick_polls()
        .await
        .expect("post-erasure tick");
    let after = fence(db.pool(), &id).await;
    let remaining = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert_eq!(remaining, 0);
    assert_eq!(before.3, Some("[1, 0]".to_owned()));
    assert_eq!(
        after, before,
        "erasure must not rewrite frozen counts or replay results"
    );
}

fn displayed_first_count(body: &Value) -> i64 {
    body["embeds"][0]["description"]
        .as_str()
        .expect("poll description")
        .lines()
        .find_map(|line| line.strip_prefix("1. one — "))
        .expect("first choice's displayed count")
        .parse()
        .expect("integer count")
}

// RED regression: an accepted live edit cannot evaporate with its uncommitted vote.
// Keep persistence real: a mock accepting PATCH alone cannot make this pass.
#[tokio::test]
async fn accepted_vote_edit_then_cancellation_preserves_durable_count_display_reconciliation() {
    let Some(db) = fixture().await else { return };
    let good = mock().await;
    let id = create(db.pool(), &good, 8100, "cancelled vote").await;
    let gate = Arc::new(ResponseGate::default());
    let selected = gate.clone();
    let displayed = Arc::new(Mutex::new(None::<Value>));
    let display = displayed.clone();
    let held = MockRest::with_gated_responder(move |r| {
        if r.method == "PATCH" && r.path.contains("/channels/") {
            *display.lock().unwrap() =
                Some(serde_json::from_slice(&r.body).expect("live count edit"));
            (reply(r), Some(selected.clone()))
        } else {
            (reply(r), None)
        }
    })
    .await;
    let rt = runtime(db.pool(), &held, GUILD, true);
    let packet = button(8101, GUILD, &id);
    let task = tokio::spawn(async move { rt.handle(&packet).await });
    timeout(WAIT, gate.wait_for_request())
        .await
        .expect("Discord accepted the live count edit");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    gate.release();
    // Give the real restart/reconciliation tick the opportunity to repair the display.
    runtime(db.pool(), &good, GUILD, true)
        .tick_polls()
        .await
        .expect("repair tick");
    let durable = votes(db.pool(), &id).await;
    let repair: Vec<_> = good
        .requests()
        .into_iter()
        .filter(|r| {
            r.method == "PATCH"
                && r.path
                    .starts_with(&format!("/api/v10/channels/{CHANNEL}/messages/"))
        })
        .collect();
    let accepted = displayed
        .lock()
        .unwrap()
        .clone()
        .expect("server accepted count display");
    db.close().await.expect("drop fixture");
    assert!(
        accepted.get("embeds").is_some(),
        "must exercise a real poll live-count edit"
    );
    assert_eq!(
        displayed_first_count(&accepted),
        1,
        "the accepted edit displays the cast vote"
    );
    let latest = repair
        .last()
        .map(|r| serde_json::from_slice(&r.body).expect("repair JSON"))
        .unwrap_or(accepted);
    assert_eq!(
        displayed_first_count(&latest),
        durable,
        "after cancellation and repair tick, displayed count must equal durable votes"
    );
}

#[derive(Debug)]
struct SettlementFailure(two_bot_core::send_admission::TokenKey);

struct FailedCompletion;

impl two_bot_core::send_admission::SendCompletion for FailedCompletion {
    fn complete(
        self: Box<Self>,
        _: Option<two_bot_core::send_admission::SendCooldown>,
    ) -> two_bot_core::send_admission::AdmissionFuture<
        'static,
        Result<(), two_bot_core::send_admission::AdmissionError>,
    > {
        Box::pin(async { Err(two_bot_core::send_admission::AdmissionError::Storage) })
    }
}

impl two_bot_core::send_admission::SendAdmission for SettlementFailure {
    fn token_key(&self) -> &two_bot_core::send_admission::TokenKey {
        &self.0
    }
    fn admit(
        &self,
    ) -> two_bot_core::send_admission::AdmissionFuture<
        '_,
        Result<
            two_bot_core::send_admission::AdmissionPermit,
            two_bot_core::send_admission::AdmissionError,
        >,
    > {
        Box::pin(async {
            Ok(two_bot_core::send_admission::AdmissionPermit::new(
                Box::new(FailedCompletion),
            ))
        })
    }
}

// RED regression: HTTP success followed by admission settlement failure cannot
// erase a displayed vote. Only the admission storage boundary is fault-injected.
#[tokio::test]
async fn accepted_vote_edit_then_admission_settlement_failure_reconciles_durable_count() {
    let Some(db) = fixture().await else { return };
    let good = mock().await;
    let id = create(db.pool(), &good, 8150, "settlement-failed vote").await;
    let admission = Arc::new(SettlementFailure(
        two_bot_core::send_admission::TokenKey::for_bot_token("synthetic-poll-token").unwrap(),
    ));
    let executor = ActionExecutor::with_admission(
        "synthetic-poll-token".into(),
        Some(good.origin()),
        admission,
    )
    .unwrap();
    let outcome = with_executor(db.pool(), executor, GUILD, true)
        .handle(&button(8151, GUILD, &id))
        .await;
    let live_edits = good
        .requests()
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.contains("/channels/"))
        .count();
    runtime(db.pool(), &good, GUILD, true)
        .tick_polls()
        .await
        .expect("reconciliation tick");
    let edits: Vec<_> = good
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH" && r.path.contains("/channels/"))
        .map(|r| serde_json::from_slice::<Value>(&r.body).expect("count edit JSON"))
        .collect();
    let durable = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err(), "exercise admission settlement failure");
    assert_eq!(
        live_edits, 1,
        "Discord accepted the live vote edit before settlement failed"
    );
    assert_eq!(
        displayed_first_count(edits.first().expect("accepted vote edit")),
        1
    );
    assert_eq!(displayed_first_count(edits.last().expect("latest display")),durable,
        "after admission settlement failure and repair tick, displayed count must equal durable votes");
}

// RED regression: invisible expired creation reservations must not hold open quota forever.
#[tokio::test]
async fn expired_invisible_reservations_release_quota_without_creation_or_results_replay() {
    let Some(db) = fixture().await else { return };
    let failed = MockRest::with_responder(|r| {
        if channel_post(r) {
            ScriptedResponse::status(403)
        } else {
            reply(r)
        }
    })
    .await;
    for n in 0..3 {
        let _ = runtime(db.pool(), &failed, GUILD, true)
            .handle(&slash(
                8200 + n,
                GUILD,
                AUTHOR,
                create_args(&format!("failed {n}")),
            ))
            .await;
    }
    let reserved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM polls WHERE status='open' AND message_id IS NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        reserved, 3,
        "exercise durable invisible creation reservations"
    );
    sqlx::query("UPDATE polls SET ends_at=now()-interval '1 minute'")
        .execute(db.pool())
        .await
        .unwrap();
    let good = mock().await;
    runtime(db.pool(), &good, GUILD, true)
        .tick_polls()
        .await
        .expect("expire invisible reservations");
    let tick_calls = good.requests().len();
    let old_open = open(db.pool()).await;
    runtime(db.pool(), &good, GUILD, true)
        .handle(&slash(8300, GUILD, AUTHOR, create_args("quota reusable")))
        .await
        .expect("new legitimate creation");
    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM polls WHERE question='quota reusable' AND message_id IS NOT NULL",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let replay: Vec<_> = good.requests().into_iter().filter(channel_post).collect();
    db.close().await.expect("drop fixture");
    assert_eq!(
        tick_calls, 0,
        "cleanup must not replay invisible creation or results"
    );
    assert_eq!(
        old_open, 0,
        "expired invisible reservations must stop consuming open quota"
    );
    assert_eq!(created, 1, "author quota must be reusable after expiry");
    assert_eq!(replay.len(), 1, "only the new legitimate creation may send");
}
