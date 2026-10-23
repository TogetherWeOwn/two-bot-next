//! Protected test-only handoff: refresh failures must not lie, lose recovery or starve peers.
//! Keep the existing delivery/admission controls unchanged. Only Discord HTTP and
//! send-admission storage are doubled; votes, receipts and settlement use Postgres.
#![cfg(feature = "db")]

pub mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor, DiscordError};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1660102030405060701";
const CHANNEL: &str = "1660102030405060702";
const AUTHOR: &str = "1660102030405060721";
const VOTER: &str = "1660102030405060722";
const WAIT: Duration = Duration::from_secs(20);

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable agent-testdb required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database")
}

fn runtime(pool: &PgPool, mock: &MockRest) -> InteractionRuntime {
    with_executor(
        pool,
        ActionExecutor::with_proxy("synthetic-poll-token".into(), Some(mock.origin())).unwrap(),
    )
}

fn with_executor(pool: &PgPool, executor: ActionExecutor) -> InteractionRuntime {
    InteractionRuntime::with_router(
        InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse().unwrap()),
            announcements: true,
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

fn packet(id: u64, kind: u8, user: &str, data: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":GUILD},
        "id":id.to_string(), "token":"synthetic-poll-callback", "type":kind, "version":1,
        "guild_id":GUILD, "channel":{"id":CHANNEL,"type":0},
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":user,"username":"fixture","discriminator":"0","bot":false}},
        "data":data
    }))
    .expect("synthetic poll interaction")
}

fn vote(id: u64, poll: &str) -> Interaction {
    packet(
        id,
        3,
        VOTER,
        json!({"custom_id":format!("poll:{poll}:0"),"component_type":2}),
    )
}

fn channel_post(request: &RestRequest) -> bool {
    request.method == "POST" && request.path == format!("/api/v10/channels/{CHANNEL}/messages")
}

fn live_patch(request: &RestRequest) -> bool {
    request.method == "PATCH"
        && request
            .path
            .starts_with(&format!("/api/v10/channels/{CHANNEL}/messages/"))
}

fn success(request: &RestRequest) -> ScriptedResponse {
    if request.path.contains("/interactions/") {
        ScriptedResponse::status(204)
    } else {
        ScriptedResponse::json(200, json!({"id":"9999"}))
    }
}

async fn good() -> MockRest {
    let messages = AtomicU64::new(100);
    MockRest::with_responder(move |request| {
        if channel_post(request) {
            ScriptedResponse::json(
                200,
                json!({"id":messages.fetch_add(1, Ordering::SeqCst).to_string()}),
            )
        } else {
            success(request)
        }
    })
    .await
}

async fn failing_patch(response: ScriptedResponse) -> MockRest {
    MockRest::with_responder(move |request| {
        if live_patch(request) {
            response.clone()
        } else {
            success(request)
        }
    })
    .await
}

async fn create(pool: &PgPool, mock: &MockRest, id: u64, question: &str) -> String {
    let request = packet(
        id,
        2,
        AUTHOR,
        json!({"id":"4444","name":"poll","type":1,"options":[
            {"name":"create","type":1,"options":[
                {"name":"question","type":3,"value":question},
                {"name":"option1","type":3,"value":"one"},
                {"name":"option2","type":3,"value":"two"},
                {"name":"duration-minutes","type":4,"value":60}
            ]}
        ]}),
    );
    assert!(runtime(pool, mock)
        .handle(&request)
        .await
        .expect("create handled"));
    sqlx::query_scalar(
        "SELECT id FROM polls WHERE guild_id=$1 AND question=$2 AND message_id IS NOT NULL",
    )
    .bind(GUILD)
    .bind(question)
    .fetch_one(pool)
    .await
    .expect("published poll")
}

async fn dirty(pool: &PgPool, id: &str) -> bool {
    sqlx::query_scalar("SELECT display_dirty FROM polls WHERE guild_id=$1 AND id=$2")
        .bind(GUILD)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn votes(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE guild_id=$1 AND poll_id=$2")
        .bind(GUILD)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn fence(pool: &PgPool, id: &str) -> (String, bool, bool, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, results_started_at IS NOT NULL, results_posted_at IS NOT NULL,
        results_message_id, counts_json::text FROM polls WHERE guild_id=$1 AND id=$2",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn expire(pool: &PgPool, id: &str, minutes: i32) {
    sqlx::query(
        "UPDATE polls SET ends_at=now()-make_interval(mins => $3) WHERE guild_id=$1 AND id=$2",
    )
    .bind(GUILD)
    .bind(id)
    .bind(minutes)
    .execute(pool)
    .await
    .unwrap();
}

fn final_reply(mock: &MockRest) -> Value {
    let replies: Vec<Value> = mock
        .requests()
        .iter()
        .filter(|r| {
            r.method == "PATCH"
                && r.path == "/api/v10/webhooks/1111/synthetic-poll-callback/messages/@original"
        })
        .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 1, "one final ephemeral reply after deferral");
    replies.into_iter().next().unwrap()
}

fn claims_vote_saved(content: &str) -> bool {
    let content = content.to_lowercase();
    [
        "vote saved",
        "vote was saved",
        "vote is saved",
        "vote has been saved",
        "saved your vote",
    ]
    .iter()
    .any(|phrase| content.contains(phrase))
}

fn assert_saved_vote_reply(body: &Value) {
    let content = body["content"]
        .as_str()
        .expect("vote receipt content")
        .to_lowercase();
    assert!(
        claims_vote_saved(&content),
        "durable vote needs a vote-specific SAVED receipt, got {content:?}"
    );
    assert!(
        !content.contains("poll is saved"),
        "a creation receipt is not a vote receipt"
    );
}

async fn pending_vote(pool: &PgPool, id: &str, interaction: u64) {
    let failed = failing_patch(ScriptedResponse::status(503)).await;
    assert!(runtime(pool, &failed)
        .handle(&vote(interaction, id))
        .await
        .is_err());
    assert_eq!(
        votes(pool, id).await,
        1,
        "vote commits before the failing PATCH"
    );
    assert!(
        dirty(pool, id).await,
        "uncertain display remains recoverable"
    );
}

// Clearing only display_dirty without advancing the measured timestamp loses the
// change from a rollback window that begins after the vote, before reconciliation.
#[tokio::test]
async fn successful_dirty_clear_is_measurable_after_post_vote_rollback_fence() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9400, "measured recovery").await;
    pending_vote(db.pool(), &id, 9401).await;
    let before_votes = votes(db.pool(), &id).await;
    let before_receipts: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE message_id IS NOT NULL),
         count(*) FILTER (WHERE results_message_id IS NOT NULL) FROM polls",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let before_fence = fence(db.pool(), &id).await;
    let rollback_fence: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let recovery = good().await;
    runtime(db.pool(), &recovery)
        .tick_polls()
        .await
        .expect("refresh succeeds");
    let cleared = !dirty(db.pool(), &id).await;
    let measured: bool =
        sqlx::query_scalar("SELECT updated_at > $3 FROM polls WHERE guild_id=$1 AND id=$2")
            .bind(GUILD)
            .bind(&id)
            .bind(rollback_fence)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let after_votes = votes(db.pool(), &id).await;
    let patches: Vec<_> = recovery.requests().into_iter().filter(live_patch).collect();
    let body: Value =
        serde_json::from_slice(&patches.first().expect("recovery PATCH").body).unwrap();
    runtime(db.pool(), &recovery)
        .tick_polls()
        .await
        .expect("settled restart");
    let total_patches = recovery.requests().iter().filter(|r| live_patch(r)).count();
    let after_receipts: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE message_id IS NOT NULL),
         count(*) FILTER (WHERE results_message_id IS NOT NULL) FROM polls",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let after_fence = fence(db.pool(), &id).await;
    let total_requests = recovery.requests().len();
    db.close().await.expect("drop fixture");
    assert!(cleared);
    assert_eq!(before_receipts, (1, 0));
    assert_eq!(
        after_receipts, before_receipts,
        "refresh must not invent or lose message receipts"
    );
    assert_eq!(
        after_fence, before_fence,
        "refresh must not alter results lifecycle"
    );
    assert_eq!(
        total_requests, 1,
        "repair is only a PATCH, not a second vote reply or POST"
    );
    assert_eq!((before_votes, after_votes), (1, 1));
    assert!(body["embeds"][0]["description"]
        .as_str()
        .unwrap()
        .contains("1. one — 1"));
    assert_eq!(
        total_patches, 1,
        "successful settlement must not stay dirty"
    );
    assert!(
        measured,
        "dirty clearing must advance updated_at beyond the post-vote rollback fence"
    );
}

async fn saved_vote_on_http_failure(response: ScriptedResponse, interaction: u64) {
    let db = fixture().await;
    let good = good().await;
    let id = create(db.pool(), &good, interaction, "truthful vote receipt").await;
    let failed = failing_patch(response).await;
    let outcome = runtime(db.pool(), &failed)
        .handle(&vote(interaction + 1, &id))
        .await;
    let durable = votes(db.pool(), &id).await;
    let pending = dirty(db.pool(), &id).await;
    let reply = final_reply(&failed);
    let edits = failed.requests().iter().filter(|r| live_patch(r)).count();
    runtime(db.pool(), &good)
        .tick_polls()
        .await
        .expect("repair");
    let repaired = !dirty(db.pool(), &id).await;
    let after_votes = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(
        outcome.is_err(),
        "refresh failure must still surface to the caller"
    );
    assert_eq!((durable, after_votes), (1, 1));
    assert_eq!(
        edits, 1,
        "uncertain PATCH is not replayed inside the handler"
    );
    assert!(pending && repaired);
    assert_saved_vote_reply(&reply);
}

#[tokio::test]
async fn unavailable_vote_refresh_keeps_dirty_but_replies_vote_saved() {
    saved_vote_on_http_failure(ScriptedResponse::status(503), 9410).await;
}

#[tokio::test]
async fn ambiguous_vote_refresh_keeps_dirty_but_replies_vote_saved() {
    saved_vote_on_http_failure(
        ScriptedResponse::json(200, json!({"id":"100"})).truncated_body(),
        9420,
    )
    .await;
}

struct FailedCompletion;
impl SendCompletion for FailedCompletion {
    fn complete(
        self: Box<Self>,
        _: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Storage) })
    }
}

#[derive(Debug)]
struct SettlementFailure(TokenKey);
impl SendAdmission for SettlementFailure {
    fn token_key(&self) -> &TokenKey {
        &self.0
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Ok(AdmissionPermit::new(Box::new(FailedCompletion))) })
    }
}

fn settlement_executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_admission(
        "synthetic-poll-token".into(),
        Some(mock.origin()),
        Arc::new(SettlementFailure(
            TokenKey::for_bot_token("synthetic-poll-token").unwrap(),
        )),
    )
    .unwrap()
}

// Extends, never replaces, the existing accepted-edit/admission-settlement control:
// the durable count is not enough if the member receives a creation warning.
#[tokio::test]
async fn admission_settlement_failure_still_replies_vote_saved_and_keeps_recovery() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9430, "admission receipt").await;
    let failing = good().await;
    let outcome = with_executor(db.pool(), settlement_executor(&failing))
        .handle(&vote(9431, &id))
        .await;
    let durable = votes(db.pool(), &id).await;
    let pending = dirty(db.pool(), &id).await;
    let edits = failing.requests().iter().filter(|r| live_patch(r)).count();
    let reply = final_reply(&failing);
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("repair admission failure");
    let after = votes(db.pool(), &id).await;
    let cleared = !dirty(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(
        edits, 1,
        "Discord accepted the live edit before settlement failed"
    );
    assert_eq!((durable, after), (1, 1));
    assert!(pending && cleared);
    assert_saved_vote_reply(&reply);
}

// A real post-HTTP database settlement failure must not turn a committed vote
// into a claim that creation failed. The trigger is confined to this owned fixture.
#[tokio::test]
async fn dirty_clear_storage_failure_still_replies_vote_saved() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9440, "settlement receipt").await;
    sqlx::raw_sql(
        "CREATE FUNCTION acceptance_refuse_clear() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF OLD.display_dirty AND NOT NEW.display_dirty THEN
            RAISE EXCEPTION 'fixture dirty settlement refused'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER acceptance_refuse_clear BEFORE UPDATE ON polls
        FOR EACH ROW EXECUTE FUNCTION acceptance_refuse_clear();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let failing = good().await;
    let outcome = runtime(db.pool(), &failing).handle(&vote(9441, &id)).await;
    let durable = votes(db.pool(), &id).await;
    let pending = dirty(db.pool(), &id).await;
    let edits = failing.requests().iter().filter(|r| live_patch(r)).count();
    let reply = final_reply(&failing);
    sqlx::raw_sql(
        "DROP TRIGGER acceptance_refuse_clear ON polls; DROP FUNCTION acceptance_refuse_clear();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("settlement repair");
    let after = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(edits, 1);
    assert_eq!((durable, after), (1, 1));
    assert!(pending);
    assert_saved_vote_reply(&reply);
}

// A pre-commit failure has no saved vote to acknowledge; optimistic replies lie.
#[tokio::test]
async fn precommit_vote_storage_failure_never_claims_saved() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9450, "not saved").await;
    sqlx::raw_sql(
        "CREATE FUNCTION acceptance_refuse_vote() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'fixture vote refused'; END $$;
        CREATE TRIGGER acceptance_refuse_vote BEFORE INSERT OR UPDATE ON poll_votes
        FOR EACH ROW EXECUTE FUNCTION acceptance_refuse_vote();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let failing = good().await;
    let outcome = runtime(db.pool(), &failing).handle(&vote(9451, &id)).await;
    let count = votes(db.pool(), &id).await;
    let pending = dirty(db.pool(), &id).await;
    let reply = final_reply(&failing);
    let edits = failing.requests().iter().filter(|r| live_patch(r)).count();
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(count, 0);
    assert!(!pending);
    assert_eq!(edits, 0);
    assert!(
        !claims_vote_saved(reply["content"].as_str().unwrap()),
        "failed vote transaction must not claim saved"
    );
}

// A terminal missing display must stop the retry loop, without discarding the vote.
#[tokio::test]
async fn definitively_rejected_live_patch_does_not_remain_dirty_forever() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9460, "deleted display").await;
    let rejected = failing_patch(ScriptedResponse::status(404)).await;
    let outcome = runtime(db.pool(), &rejected).handle(&vote(9461, &id)).await;
    let pending = dirty(db.pool(), &id).await;
    let count = votes(db.pool(), &id).await;
    let recovery = good().await;
    runtime(db.pool(), &recovery)
        .tick_polls()
        .await
        .expect("restart after proven rejection");
    let calls = recovery.requests().len();
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(count, 1);
    assert!(
        !pending,
        "a definitive Rejected display must not remain dirty forever"
    );
    assert_eq!(
        calls, 0,
        "restart must not endlessly retry a definitively rejected display"
    );
}

// A proven-no-effect closing PATCH is not an uncertain results POST. It must not
// strand the results fence before the one permitted results send even starts.
#[tokio::test]
async fn closing_patch_404_still_posts_and_settles_results_exactly_once() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9470, "missing display close").await;
    runtime(db.pool(), &mock)
        .handle(&vote(9471, &id))
        .await
        .unwrap();
    expire(db.pool(), &id, 1).await;
    let rejected = failing_patch(ScriptedResponse::status(404)).await;
    let _ = runtime(db.pool(), &rejected).tick_polls().await;
    let settled = fence(db.pool(), &id).await;
    let requests = rejected.requests();
    let posts: Vec<_> = requests.iter().filter(|r| channel_post(r)).collect();
    let payloads: Vec<Value> = posts
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let restart = good().await;
    runtime(db.pool(), &restart)
        .tick_polls()
        .await
        .expect("settled restart");
    let after = fence(db.pool(), &id).await;
    let replays = restart.requests().len();
    db.close().await.expect("drop fixture");
    assert_eq!(requests.iter().filter(|r| live_patch(r)).count(), 1);
    assert_eq!(
        posts.len(),
        1,
        "404 display edit must not suppress the results POST"
    );
    assert!(
        live_patch(&requests[0]) && channel_post(&requests[1]),
        "close edits before posting results"
    );
    assert!(payloads[0]["embeds"][0]
        .to_string()
        .contains("missing display close"));
    assert_eq!(
        settled,
        (
            "closed".into(),
            false,
            true,
            Some("9999".into()),
            Some("[1, 0]".into())
        )
    );
    assert_eq!(after, settled);
    assert_eq!(replays, 0);
}

async fn uncertain_close(response: ScriptedResponse, interaction: u64) {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, interaction, "uncertain closing edit").await;
    expire(db.pool(), &id, 1).await;
    let uncertain = failing_patch(response).await;
    let outcome = runtime(db.pool(), &uncertain).tick_polls().await;
    let armed = fence(db.pool(), &id).await;
    let calls = uncertain.requests();
    let a = runtime(db.pool(), &mock);
    let b = runtime(db.pool(), &mock);
    let before = mock.requests().len();
    let (a, b) = tokio::join!(a.tick_polls(), b.tick_polls());
    let after = fence(db.pool(), &id).await;
    let replay = mock.requests().len() - before;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(calls.iter().filter(|r| live_patch(r)).count(), 1);
    assert_eq!(calls.iter().filter(|r| channel_post(r)).count(), 0);
    assert_eq!(
        armed,
        ("closed".into(), true, false, None, Some("[0, 0]".into()))
    );
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(
        after, armed,
        "uncertain PATCH must not release the durable no-replay fence"
    );
    assert_eq!(
        replay, 0,
        "restarting/competing workers must not replay the uncertain close"
    );
}

#[tokio::test]
async fn unavailable_closing_patch_retains_no_replay_fence_and_never_posts_results() {
    uncertain_close(ScriptedResponse::status(503), 9480).await;
}

#[tokio::test]
async fn ambiguous_closing_patch_retains_no_replay_fence_and_never_posts_results() {
    uncertain_close(
        ScriptedResponse::json(200, json!({"id":"100"})).truncated_body(),
        9490,
    )
    .await;
}

#[tokio::test]
async fn closing_patch_admission_settlement_failure_never_releases_no_replay_fence() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9500, "uncertain admission close").await;
    expire(db.pool(), &id, 1).await;
    let uncertain = good().await;
    let outcome = with_executor(db.pool(), settlement_executor(&uncertain))
        .tick_polls()
        .await;
    let armed = fence(db.pool(), &id).await;
    let before = mock.requests().len();
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("restart skips uncertain settlement");
    let replay = mock.requests().len() - before;
    let calls = uncertain.requests();
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert_eq!(calls.iter().filter(|r| live_patch(r)).count(), 1);
    assert_eq!(calls.iter().filter(|r| channel_post(r)).count(), 0);
    assert!(armed.1 && !armed.2);
    assert_eq!(replay, 0);
}

// Rate limits are not terminal Rejected displays. Preserve the idempotent retry,
// not an endlessly dirty terminal error nor a falsely successful first tick.
#[tokio::test]
async fn rate_limited_refresh_remains_dirty_until_a_later_successful_tick() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9510, "retry rate limit").await;
    pending_vote(db.pool(), &id, 9511).await;
    let limited = failing_patch(ScriptedResponse::rate_limited(0.0, "0")).await;
    let outcome = runtime(db.pool(), &limited).tick_polls().await;
    let pending = dirty(db.pool(), &id).await;
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("retry permitted after rate limit");
    let cleared = !dirty(db.pool(), &id).await;
    let count = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert!(pending && cleared);
    assert_eq!(count, 1);
    assert_eq!(
        limited.requests().iter().filter(|r| live_patch(r)).count(),
        1
    );
}

#[derive(Debug)]
struct BlockedAdmission(TokenKey);
impl SendAdmission for BlockedAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.0
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Blocked) })
    }
}

#[tokio::test]
async fn guarded_refresh_remains_dirty_until_admission_recovers() {
    let db = fixture().await;
    let mock = good().await;
    let id = create(db.pool(), &mock, 9520, "retry guard").await;
    pending_vote(db.pool(), &id, 9521).await;
    let blocked = good().await;
    let executor = ActionExecutor::with_admission(
        "synthetic-poll-token".into(),
        Some(blocked.origin()),
        Arc::new(BlockedAdmission(
            TokenKey::for_bot_token("synthetic-poll-token").unwrap(),
        )),
    )
    .unwrap();
    let outcome = with_executor(db.pool(), executor).tick_polls().await;
    let pending = dirty(db.pool(), &id).await;
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("retry after guard recovers");
    let cleared = !dirty(db.pool(), &id).await;
    let count = votes(db.pool(), &id).await;
    db.close().await.expect("drop fixture");
    assert!(outcome.is_err());
    assert!(pending && cleared);
    assert_eq!(count, 1);
    assert_eq!(
        blocked.requests().len(),
        0,
        "admission failure occurs before HTTP"
    );
}

// A poison dirty poll must not starve later dirty peers or the due-close phase.
#[tokio::test]
async fn failed_refresh_does_not_stop_other_dirty_and_due_polls_and_returns_first_error() {
    let db = fixture().await;
    let mock = good().await;
    let first = create(db.pool(), &mock, 9530, "first dirty failure").await;
    let later = create(db.pool(), &mock, 9531, "later dirty success").await;
    let due = create(db.pool(), &mock, 9532, "due despite dirty failure").await;
    pending_vote(db.pool(), &first, 9533).await;
    pending_vote(db.pool(), &later, 9534).await;
    let first_message: String = sqlx::query_scalar("SELECT message_id FROM polls WHERE id=$1")
        .bind(&first)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let failed_path = format!("/api/v10/channels/{CHANNEL}/messages/{first_message}");
    let chosen = failed_path.clone();
    let failure = MockRest::with_responder(move |r| {
        if live_patch(r) && r.path == chosen {
            ScriptedResponse::status(503)
        } else {
            success(r)
        }
    })
    .await;
    // With only one dirty failure present this is the externally observed first
    // error; the second call must keep it, even while making progress on peers.
    sqlx::query("UPDATE polls SET display_dirty=false WHERE id=$1")
        .bind(&later)
        .execute(db.pool())
        .await
        .unwrap();
    let expected_error = runtime(db.pool(), &failure).tick_polls().await.unwrap_err();
    sqlx::query("UPDATE polls SET display_dirty=true WHERE id=$1")
        .bind(&later)
        .execute(db.pool())
        .await
        .unwrap();
    expire(db.pool(), &due, 1).await;
    let before = failure.requests().len();
    let outcome = tokio::time::timeout(WAIT, runtime(db.pool(), &failure).tick_polls())
        .await
        .expect("bounded batch tick");
    let first_pending = dirty(db.pool(), &first).await;
    let later_pending = dirty(db.pool(), &later).await;
    let due_state = fence(db.pool(), &due).await;
    let requests = failure.requests()[before..].to_vec();
    let durable = (
        votes(db.pool(), &first).await,
        votes(db.pool(), &later).await,
    );
    db.close().await.expect("drop fixture");
    assert_eq!(
        outcome.unwrap_err(),
        expected_error,
        "batch must still surface its first error"
    );
    assert!(requests
        .iter()
        .any(|r| live_patch(r) && r.path == failed_path));
    assert!(first_pending, "uncertain failed display remains pending");
    assert!(
        !later_pending,
        "one failed refresh must not stop another dirty refresh"
    );
    assert_eq!(durable, (1, 1));
    assert!(
        due_state.2,
        "one failed refresh must not stop a due close/results settlement"
    );
    assert_eq!(requests.iter().filter(|r| channel_post(r)).count(), 1);
}

// The same isolation is required within the due-close phase. An uncertain first
// closing edit retains its own fence, not a veto on independent due polls.
#[tokio::test]
async fn failed_close_does_not_stop_later_due_polls_or_hide_first_error() {
    let db = fixture().await;
    let mock = good().await;
    let first = create(db.pool(), &mock, 9540, "first close failure").await;
    let later = create(db.pool(), &mock, 9541, "later due success").await;
    expire(db.pool(), &first, 2).await;
    expire(db.pool(), &later, 1).await;
    let first_message: String = sqlx::query_scalar("SELECT message_id FROM polls WHERE id=$1")
        .bind(&first)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let first_path = format!("/api/v10/channels/{CHANNEL}/messages/{first_message}");
    let chosen = first_path.clone();
    let failure = MockRest::with_responder(move |r| {
        if live_patch(r) && r.path == chosen {
            ScriptedResponse::status(503)
        } else {
            success(r)
        }
    })
    .await;
    let outcome = tokio::time::timeout(WAIT, runtime(db.pool(), &failure).tick_polls())
        .await
        .expect("bounded close batch");
    let first_state = fence(db.pool(), &first).await;
    let later_state = fence(db.pool(), &later).await;
    let requests = failure.requests();
    let posts: Vec<Value> = requests
        .iter()
        .filter(|r| channel_post(r))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let before = mock.requests().len();
    runtime(db.pool(), &mock)
        .tick_polls()
        .await
        .expect("settled peers and uncertain first are not replayed");
    let replay = mock.requests().len() - before;
    db.close().await.expect("drop fixture");
    assert!(
        outcome.is_err(),
        "progress on later polls must not hide the first failure"
    );
    assert!(
        live_patch(&requests[0]) && requests[0].path == first_path,
        "fixture exercises failure first"
    );
    assert!(first_state.1 && !first_state.2);
    assert!(
        later_state.2,
        "one failed close must not prevent the next due poll from posting results"
    );
    assert_eq!(posts.len(), 1);
    assert!(posts[0]["embeds"][0]
        .to_string()
        .contains("later due success"));
    assert_eq!(
        replay, 0,
        "neither settled nor uncertain results may replay"
    );
}

// Distinct failure kinds expose implementations that continue correctly but
// overwrite the first error with the last. The third poll must still settle.
#[tokio::test]
async fn tick_preserves_first_error_when_a_later_poll_fails_differently() {
    let db = fixture().await;
    let mock = good().await;
    let first = create(db.pool(), &mock, 9550, "first unavailable close").await;
    let second = create(db.pool(), &mock, 9551, "second rejected result").await;
    let third = create(db.pool(), &mock, 9552, "third independent success").await;
    expire(db.pool(), &first, 3).await;
    expire(db.pool(), &second, 2).await;
    expire(db.pool(), &third, 1).await;
    let first_message: String = sqlx::query_scalar("SELECT message_id FROM polls WHERE id=$1")
        .bind(&first)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let first_path = format!("/api/v10/channels/{CHANNEL}/messages/{first_message}");
    let second_id = second.clone();
    let failure = MockRest::with_responder(move |r| {
        if live_patch(r) && r.path == first_path {
            ScriptedResponse::status(503)
        } else if channel_post(r)
            && serde_json::from_slice::<Value>(&r.body).expect("results request JSON")["nonce"]
                == format!("r-{second_id}")
        {
            ScriptedResponse::status(403)
        } else {
            success(r)
        }
    })
    .await;
    let outcome = tokio::time::timeout(WAIT, runtime(db.pool(), &failure).tick_polls())
        .await
        .expect("bounded mixed-error batch");
    let first_state = fence(db.pool(), &first).await;
    let second_state = fence(db.pool(), &second).await;
    let third_state = fence(db.pool(), &third).await;
    let requests = failure.requests();
    let posts: Vec<Value> = requests
        .iter()
        .filter(|r| channel_post(r))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    db.close().await.expect("drop fixture");
    assert!(
        matches!(outcome, Err(DiscordError::Unavailable(_))),
        "first Unavailable must survive a later Rejected result, not be overwritten or hidden"
    );
    assert!(first_state.1 && !first_state.2);
    assert!(second_state.1 && !second_state.2);
    assert!(
        third_state.2,
        "a second failure must not starve the third due poll"
    );
    assert_eq!(
        posts.len(),
        2,
        "only the second and third polls reach results POST"
    );
    assert_eq!(posts[0]["nonce"], format!("r-{second}"));
    assert_eq!(posts[1]["nonce"], format!("r-{third}"));
}
