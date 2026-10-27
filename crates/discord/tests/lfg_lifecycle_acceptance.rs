//! Protected acceptance corpus: the builder must not edit this file.
//! Real migrated disposable Postgres, LFG service/router and HTTP executor;
//! only Discord is a loopback double. The supervised-job case lives in the bot
//! crate (`crates/bot/src/lfg_lifecycle_jobs_acceptance.rs`), the only place
//! the binary's own `crate::jobs` resolves. No implementation diff informed
//! these expectations. See tests/lfg-lifecycle.acceptance.json for the contract.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use time::OffsetDateTime;
use two_bot_core::{
    lfg::{self, LfgPost, LfgSelectAction, LfgStatus},
    lfg_store as store, RouterGates,
};
use two_bot_discord::{
    interactions::InteractionRuntime,
    lfg_interactions::{LfgError, LfgInteractions, LfgRequest},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: u64 = 1111;
const OWNER: &str = "3333";
const FUTURE: &str = "2199-01-01T00:00:00.000Z";
const OLD: &str = "2000-01-01T00:00:00.000Z";

// Baseline compatibility adapter, NOT a sweep implementation: the baseline has
// no lifecycle entrypoint and therefore does no work. An inherent production
// method with this signature takes precedence at every unqualified call below.
// Returning a count without changing real rows / HTTP cannot satisfy the tests.
trait MissingLifecycleSweep {
    async fn sweep_lifecycle(
        &self,
        executor: &ActionExecutor,
        guild_id: &str,
        bot_user_id: u64,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<usize, LfgError>;
}
impl MissingLifecycleSweep for LfgInteractions {
    async fn sweep_lifecycle(
        &self,
        _executor: &ActionExecutor,
        _guild_id: &str,
        _bot_user_id: u64,
        _now: OffsetDateTime,
        _limit: usize,
    ) -> Result<usize, LfgError> {
        Ok(0)
    }
}

fn clock() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(4_102_444_800).expect("2100-01-01 UTC")
}

fn post(id: &str, starts_at: &str, message: Option<&str>, created_at: &str) -> LfgPost {
    LfgPost {
        id: id.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: message.map(str::to_owned),
        title: "Offline raid".into(),
        starts_at: starts_at.into(),
        status: LfgStatus::Open,
        created_by: OWNER.into(),
        created_at: created_at.into(),
        closed_at: None,
    }
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("request JSON")
}

fn ok_rest(request: &RestRequest) -> ScriptedResponse {
    if request.method == "GET" {
        ScriptedResponse::json(200, json!([]))
    } else if request.path.ends_with("/callback") {
        ScriptedResponse::status(204)
    } else {
        ScriptedResponse::json(200, json!({"id": "9000"}))
    }
}

struct Fixture {
    db: TestDatabase,
    service: Arc<LfgInteractions>,
    executor: Arc<ActionExecutor>,
    rest: MockRest,
}
impl Fixture {
    async fn new(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        // The shared guard rejects production/staging, credentials and libpq
        // overrides. Absence/setup failure is NOT silently counted as a pass.
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let rest = MockRest::with_responder(responder).await;
        let executor = Arc::new(
            ActionExecutor::with_proxy("offline-lfg-test-token".into(), Some(rest.origin()))
                .expect("loopback executor"),
        );
        let service = Arc::new(LfgInteractions::new(db.pool().clone()));
        Self {
            db,
            service,
            executor,
            rest,
        }
    }

    async fn seed(&self, row: &LfgPost) {
        let roles = lfg::spec_roles(
            &row.id,
            &lfg::parse_role_spec("tank:Tank:1,dps:DPS:1").expect("roles"),
        );
        store::put_lfg(self.db.pool(), row, &roles, true)
            .await
            .expect("seed LFG");
    }

    async fn saved(&self, id: &str) -> LfgPost {
        store::get_lfg(self.db.pool(), GUILD, id)
            .await
            .expect("read LFG")
            .expect("retained row")
    }

    async fn sweep(&self, limit: usize) -> Result<usize, LfgError> {
        // Keep the compatibility trait live after an inherent method is added.
        let _baseline_entrypoint = <LfgInteractions as MissingLifecycleSweep>::sweep_lifecycle;
        self.service
            .sweep_lifecycle(&self.executor, GUILD, BOT, clock(), limit)
            .await
    }

    async fn execute(
        &self,
        request: LfgRequest,
        actor: &str,
        interaction: u64,
    ) -> Result<String, LfgError> {
        self.service
            .execute(&self.executor, request, GUILD, actor, interaction, BOT)
            .await
    }

    async fn finish(self) -> Vec<RestRequest> {
        let requests = self.rest.requests();
        self.rest.shutdown().await;
        drop(self.service);
        drop(self.executor);
        self.db.close().await.expect("fixture teardown");
        requests
    }
}

fn signup(id: &str) -> LfgRequest {
    LfgRequest::Select(LfgSelectAction::Signup {
        post_id: id.into(),
        role_key: "tank".into(),
    })
}
fn create() -> LfgRequest {
    LfgRequest::Create {
        title: "New raid".into(),
        starts_at: FUTURE.into(),
        roles: "tank:Tank:1".into(),
        channel_id: CHANNEL.into(),
    }
}

#[test]
fn lifecycle_job_attempts_have_their_own_counter_not_the_other_bucket() {
    let metrics = two_bot_core::metrics::Metrics::default();
    metrics.job_success("lfg_sweep", 123);
    metrics.job_failure("lfg_sweep", two_bot_core::metrics::JobErrorClass::Rest);
    let rendered = metrics.render(None);
    assert!(rendered.contains("two_bot_job_runs_total{job=\"lfg_sweep\",outcome=\"success\"} 1\n"));
    assert!(rendered.contains("two_bot_job_runs_total{job=\"lfg_sweep\",outcome=\"failure\"} 1\n"));
    assert!(rendered.contains("two_bot_job_runs_total{job=\"other\",outcome=\"success\"} 0\n"));
    assert!(rendered.contains("two_bot_job_runs_total{job=\"other\",outcome=\"failure\"} 0\n"));
}

#[test]
fn posted_message_exposes_the_identifier_needed_to_close_it() {
    // Removing the visible ID strands a member even though the component's
    // custom_id still contains it. Inspect member-visible content, not source.
    let row = post("lfg-47001", FUTURE, Some("5001"), OLD);
    let rendered = lfg::lfg_content(&row, &[], &[]);
    assert!(
        rendered.contains("lfg-47001"),
        "the public post must expose its close identifier: {rendered}"
    );
}

#[tokio::test]
async fn a_past_start_signup_is_refused_without_writing_membership() {
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-past", OLD, Some("5001"), OLD)).await;
    let reply = f.execute(signup("lfg-past"), "3334", 47002).await;
    let signups = store::list_lfg_signups(f.db.pool(), "lfg-past")
        .await
        .expect("signups");
    f.finish().await;
    let reply = reply.expect("actionable refusal, not a transport failure");
    assert!(
        signups.is_empty(),
        "past-start signup must never be committed"
    );
    let text = reply.to_lowercase();
    assert!(
        text.contains("start") || text.contains("past"),
        "explain why signup was refused: {reply}"
    );
    assert!(
        text.contains("/lfg") || text.contains("new post"),
        "name a recovery action: {reply}"
    );
}

#[tokio::test]
async fn committed_signup_refresh_failure_does_not_claim_state_is_unchanged() {
    let f = Fixture::new(|r| {
        if r.method == "PATCH" {
            ScriptedResponse::status(403)
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-signup-copy", FUTURE, Some("5001"), OLD))
        .await;
    let reply = f.execute(signup("lfg-signup-copy"), "3334", 47003).await;
    let signups = store::list_lfg_signups(f.db.pool(), "lfg-signup-copy")
        .await
        .expect("signups");
    f.finish().await;
    assert_eq!(signups.len(), 1, "signup committed despite Discord failure");
    let reply = reply.expect("post-commit warning");
    assert!(
        !reply.to_lowercase().contains("state is unchanged"),
        "false post-commit claim: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("refresh"),
        "disclose stale message: {reply}"
    );
}

#[tokio::test]
async fn committed_close_refresh_failure_does_not_claim_state_is_unchanged() {
    let f = Fixture::new(|r| {
        if r.method == "PATCH" {
            ScriptedResponse::status(403)
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-close-copy", FUTURE, Some("5001"), OLD))
        .await;
    let reply = f
        .execute(
            LfgRequest::Close {
                post_id: "lfg-close-copy".into(),
            },
            OWNER,
            47004,
        )
        .await;
    let saved = f.saved("lfg-close-copy").await;
    f.finish().await;
    assert_eq!(saved.status, LfgStatus::Closed);
    let reply = reply.expect("post-commit warning");
    assert!(
        !reply.to_lowercase().contains("state is unchanged"),
        "false post-commit claim: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("refresh"),
        "disclose stale message: {reply}"
    );
}

#[tokio::test]
async fn expired_posts_are_closed_and_their_messages_have_no_signup_controls() {
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-expired", OLD, Some("5001"), OLD)).await;
    let count = f.sweep(3).await;
    let saved = f.saved("lfg-expired").await;
    let requests = f.finish().await;
    assert_eq!(
        count.expect("sweep"),
        1,
        "count the processed lifecycle row"
    );
    assert_eq!(saved.status, LfgStatus::Closed);
    assert!(saved.closed_at.is_some());
    let edit = requests
        .iter()
        .find(|r| r.method == "PATCH" && r.path.ends_with("/messages/5001"))
        .expect("refresh the existing message, never post a replacement");
    assert_eq!(body(edit)["components"], json!([]));
    assert!(body(edit)["content"]
        .as_str()
        .expect("content")
        .to_lowercase()
        .contains("closed"));
    assert_eq!(body(edit)["allowed_mentions"]["parse"], json!([]));
    assert!(!requests.iter().any(|r| r.method == "POST"));
}

#[tokio::test]
async fn twenty_expired_posts_eventually_free_the_unchanged_twenty_post_cap() {
    let f = Fixture::new(ok_rest).await;
    for n in 0..20 {
        f.seed(&post(
            &format!("lfg-expired-{n:02}"),
            OLD,
            Some(&format!("{}", 5100 + n)),
            OLD,
        ))
        .await;
    }
    let before = f.execute(create(), OWNER, 47005).await;
    let mut counts = Vec::new();
    // Seven bounded calls suffice for twenty rows at limit three; an eighth
    // must observe no work. A stalled/no-op sweep cannot satisfy the cap check.
    for _ in 0..8 {
        counts.push(f.sweep(3).await.expect("bounded sweep"));
    }
    let after = f.execute(create(), OWNER, 47006).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lfg_posts WHERE guild_id = $1 AND status = 'open'",
    )
    .bind(GUILD)
    .fetch_one(f.db.pool())
    .await
    .expect("open count");
    f.finish().await;
    assert!(
        before.is_err(),
        "21st open post is still refused before cleanup"
    );
    assert!(
        counts.iter().all(|count| *count <= 3),
        "batch exceeded its row budget: {counts:?}"
    );
    assert_eq!(
        counts.iter().sum::<usize>(),
        20,
        "process each stale row once"
    );
    assert_eq!(counts.last(), Some(&0), "settled sweep must be idempotent");
    assert!(
        after.is_ok(),
        "sweep must free capacity, not raise the cap: {after:?}"
    );
    assert_eq!(open, 1);
}

#[tokio::test]
async fn aged_orphan_recovers_the_bot_nonce_without_reposting() {
    let nonce = lfg::lfg_nonce("lfg-orphan-recover");
    let f = Fixture::new(move |r| {
        if r.method == "GET" {
            ScriptedResponse::json(
                200,
                json!([{"id": "6001", "channel_id": CHANNEL, "author": {"id": "1111", "bot": true}, "nonce": nonce}]),
            )
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-orphan-recover", FUTURE, None, OLD)).await;
    let count = f.sweep(3).await;
    let saved = f.saved("lfg-orphan-recover").await;
    let requests = f.finish().await;
    assert_eq!(count.expect("recover sweep"), 1);
    assert_eq!(saved.message_id.as_deref(), Some("6001"));
    assert_eq!(saved.status, LfgStatus::Open);
    assert!(requests.iter().any(|r| r.method == "GET"));
    assert!(
        !requests.iter().any(|r| r.method == "POST"),
        "nonce recovery cannot resend"
    );
}

#[tokio::test]
async fn aged_orphan_without_a_recoverable_message_closes_and_frees_capacity() {
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-orphan-missing", FUTURE, None, OLD)).await;
    let count = f.sweep(3).await;
    let saved = f.saved("lfg-orphan-missing").await;
    let requests = f.finish().await;
    assert_eq!(count.expect("orphan sweep"), 1);
    assert_eq!(saved.status, LfgStatus::Closed);
    assert!(saved.closed_at.is_some());
    assert!(
        requests.iter().any(|r| r.method == "GET"),
        "attempt nonce recovery first"
    );
    assert!(!requests.iter().any(|r| r.method == "POST"));
}

#[tokio::test]
async fn a_foreign_author_cannot_rescue_an_orphan_with_a_matching_nonce() {
    let nonce = lfg::lfg_nonce("lfg-foreign-nonce");
    let f = Fixture::new(move |r| {
        if r.method == "GET" {
            ScriptedResponse::json(
                200,
                json!([{"id": "6002", "channel_id": CHANNEL, "author": {"id": "9999", "bot": true}, "nonce": nonce}]),
            )
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-foreign-nonce", FUTURE, None, OLD)).await;
    let count = f.sweep(3).await;
    let saved = f.saved("lfg-foreign-nonce").await;
    f.finish().await;
    assert_eq!(count.expect("orphan sweep"), 1);
    assert_eq!(saved.status, LfgStatus::Closed);
    assert!(
        saved.message_id.is_none(),
        "foreign author is never accepted as our post"
    );
}

#[tokio::test]
async fn one_failed_message_refresh_does_not_stall_later_rows() {
    let attempts = AtomicUsize::new(0);
    let f = Fixture::new(move |r| {
        if r.method == "PATCH" && attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            // Fail the first refresh regardless of database iteration order.
            ScriptedResponse::status(403)
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-a-failing", OLD, Some("5001"), OLD)).await;
    f.seed(&post("lfg-b-healthy", OLD, Some("5002"), OLD)).await;
    let result = f.sweep(3).await;
    let first = f.saved("lfg-a-failing").await;
    let second = f.saved("lfg-b-healthy").await;
    let requests = f.finish().await;
    assert!(
        result.is_ok(),
        "row-local Discord refusal must not abort the batch"
    );
    assert_eq!(first.status, LfgStatus::Closed);
    assert_eq!(second.status, LfgStatus::Closed);
    assert!(requests
        .iter()
        .any(|r| r.method == "PATCH" && r.path.ends_with("/messages/5001")));
    assert!(requests
        .iter()
        .any(|r| r.method == "PATCH" && r.path.ends_with("/messages/5002")));
}

#[tokio::test]
async fn sweep_preserves_future_posts_young_orphans_and_other_guilds() {
    let f = Fixture::new(ok_rest).await;
    let future = post("lfg-future", FUTURE, Some("5001"), OLD);
    let young = post("lfg-young", FUTURE, None, "2100-01-01T00:00:00.000Z");
    let mut foreign = post("lfg-foreign", OLD, Some("5002"), OLD);
    foreign.guild_id = "2223".into();
    for row in [&future, &young, &foreign] {
        f.seed(row).await;
    }
    let count = f.sweep(3).await;
    let saved_future = f.saved("lfg-future").await;
    let saved_young = f.saved("lfg-young").await;
    let saved_foreign = store::get_lfg(f.db.pool(), "2223", "lfg-foreign")
        .await
        .expect("foreign read")
        .expect("foreign row");
    let requests = f.finish().await;
    assert_eq!(count.expect("no eligible work"), 0);
    assert_eq!(saved_future, future);
    assert_eq!(saved_young, young);
    assert_eq!(saved_foreign, foreign);
    assert!(
        requests.is_empty(),
        "no speculative REST for ineligible rows"
    );
}

#[tokio::test]
async fn zero_budget_does_no_work_even_with_expired_rows() {
    let f = Fixture::new(ok_rest).await;
    let original = post("lfg-zero-budget", OLD, Some("5001"), OLD);
    f.seed(&original).await;
    let count = f.sweep(0).await;
    let saved = f.saved(&original.id).await;
    let requests = f.finish().await;
    assert_eq!(count.expect("zero budget"), 0);
    assert_eq!(saved, original);
    assert!(requests.is_empty());
}

#[tokio::test]
async fn future_signup_replay_retains_one_membership_and_one_audit() {
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-replay", FUTURE, Some("5001"), OLD)).await;
    let first = f.execute(signup("lfg-replay"), "3334", 47007).await;
    let original = store::list_lfg_signups(f.db.pool(), "lfg-replay")
        .await
        .expect("first membership");
    let second = f.execute(signup("lfg-replay"), "3334", 47007).await;
    let replay = store::list_lfg_signups(f.db.pool(), "lfg-replay")
        .await
        .expect("replayed membership");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM announcements_audit_log WHERE id = 'lfg:signup:47007'",
    )
    .fetch_one(f.db.pool())
    .await
    .expect("audit count");
    f.finish().await;
    assert_eq!(first.expect("join"), "LFG joined.");
    assert_eq!(second.expect("replay"), "LFG joined.");
    assert_eq!(original.len(), 1);
    assert_eq!(replay, original, "replay must not change joined_at");
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn creation_and_close_still_require_manage_events_at_the_router() {
    let f = Fixture::new(ok_rest).await;
    f.seed(&post("lfg-authority", FUTURE, Some("5001"), OLD))
        .await;
    let rt = InteractionRuntime::new(
        RouterGates {
            configured_guild: Some(2222),
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
        },
        f.db.pool().clone(),
        ActionExecutor::with_proxy("offline-lfg-test-token".into(), Some(f.rest.origin()))
            .expect("executor"),
        BOT,
        two_bot_core::ClassifierConfig::default(),
    );
    let mut results = Vec::new();
    for (id, name, options) in [
        (
            47008_u64,
            "lfg",
            json!([{"type":3,"name":"title","value":"Raid"},{"type":3,"name":"starts-at","value":FUTURE},{"type":3,"name":"roles","value":"tank:Tank:1"}]),
        ),
        (
            47009,
            "lfg-close",
            json!([{"type":3,"name":"id","value":"lfg-authority"}]),
        ),
    ] {
        let interaction = serde_json::from_value(json!({
            "id": id.to_string(), "application_id":"1111", "type":2, "guild_id":GUILD,
            "channel":{"id":CHANNEL,"type":0}, "member":{"user":{"id":OWNER,"username":"member","discriminator":"0","avatar":null},
                "roles":[],"joined_at":null,"deaf":false,"mute":false,"flags":0,"permissions":"0"},
            "token":"offline-interaction-token", "version":1, "entitlements":[],
            "authorizing_integration_owners":{}, "data":{"id":"1","name":name,"type":1,"options":options}
        })).expect("wire interaction");
        results.push(rt.handle(&interaction).await);
    }
    let saved = f.saved("lfg-authority").await;
    let created = store::get_lfg(f.db.pool(), GUILD, "lfg-47008")
        .await
        .expect("create check");
    drop(rt);
    let requests = f.finish().await;
    assert!(results
        .into_iter()
        .all(|r| r.expect("handled permission refusal")));
    assert_eq!(saved.status, LfgStatus::Open);
    assert!(created.is_none());
    assert!(!requests
        .iter()
        .any(|r| r.path.starts_with("/api/v10/channels/")));
    for r in requests.iter().filter(|r| r.path.ends_with("/callback")) {
        assert_eq!(
            body(r)["data"]["content"],
            two_bot_core::MANAGE_EVENTS_REQUIRED
        );
    }
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.ends_with("/callback"))
            .count(),
        2
    );
}
