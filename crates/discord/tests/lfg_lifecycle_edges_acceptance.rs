//! Protected acceptance extension: the Builder must not edit this file.
//! Test Author-owned lifecycle edge cases; preserve the original corpus verbatim.
//! Real disposable Postgres and real service/HTTP executor; Discord alone is doubled.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use two_bot_core::{
    lfg::{self, LfgPost, LfgSignup, LfgStatus},
    lfg_store as store,
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
const OLD: &str = "2000-01-01T00:00:00.000Z";
const FUTURE: &str = "2199-01-01T00:00:00.000Z";

// Baseline-only adapter: an inherent implementation wins method resolution.
// Never simulate lifecycle changes in this compatibility seam.
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

fn ok_rest(request: &RestRequest) -> ScriptedResponse {
    if request.method == "GET" {
        ScriptedResponse::json(200, json!([]))
    } else {
        ScriptedResponse::json(200, json!({"id": "9000"}))
    }
}

struct Fixture {
    db: TestDatabase,
    service: Arc<LfgInteractions>,
    executor: ActionExecutor,
    rest: MockRest,
}
impl Fixture {
    async fn new(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let rest = MockRest::with_responder(responder).await;
        let executor =
            ActionExecutor::with_proxy("offline-lfg-edge-test-token".into(), Some(rest.origin()))
                .expect("loopback executor");
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
            &lfg::parse_role_spec("tank:Tank:1").expect("roles"),
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

    async fn sweep(&self, bot: u64, now: OffsetDateTime, limit: usize) -> Result<usize, LfgError> {
        let _baseline_entrypoint = <LfgInteractions as MissingLifecycleSweep>::sweep_lifecycle;
        self.service
            .sweep_lifecycle(&self.executor, GUILD, bot, now, limit)
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

#[tokio::test]
async fn expiry_grace_includes_exactly_fifteen_minutes_but_not_one_second_less() {
    let f = Fixture::new(ok_rest).await;
    let row = post(
        "lfg-grace-boundary",
        "2100-01-01T00:00:00.000Z",
        Some("5001"),
        OLD,
    );
    f.seed(&row).await;
    let young = f.sweep(BOT, clock() + Duration::seconds(899), 20).await;
    let before = f.saved(&row.id).await;
    let early_requests = f.rest.requests();
    let boundary = f.sweep(BOT, clock() + Duration::seconds(900), 20).await;
    let after = f.saved(&row.id).await;
    f.finish().await;
    assert_eq!(young.expect("inside grace"), 0);
    assert_eq!(before, row);
    assert!(early_requests.is_empty(), "no premature Discord edit");
    assert_eq!(boundary.expect("grace elapsed"), 1);
    assert_eq!(after.status, LfgStatus::Closed);
    assert!(after.closed_at.is_some());
}

#[tokio::test]
async fn orphan_age_includes_exactly_five_minutes_but_not_one_second_less() {
    let f = Fixture::new(ok_rest).await;
    let row = post(
        "lfg-orphan-boundary",
        FUTURE,
        None,
        "2100-01-01T00:00:00.000Z",
    );
    f.seed(&row).await;
    let young = f.sweep(BOT, clock() + Duration::seconds(299), 20).await;
    let before = f.saved(&row.id).await;
    let early_requests = f.rest.requests();
    let boundary = f.sweep(BOT, clock() + Duration::seconds(300), 20).await;
    let after = f.saved(&row.id).await;
    let requests = f.finish().await;
    assert_eq!(young.expect("young orphan"), 0);
    assert_eq!(before, row);
    assert!(early_requests.is_empty(), "no premature history lookup");
    assert_eq!(boundary.expect("orphan aged"), 1);
    assert_eq!(after.status, LfgStatus::Closed);
    assert!(requests.iter().any(|r| r.method == "GET"));
    assert!(!requests.iter().any(|r| r.method == "POST"));
}

#[tokio::test]
async fn an_expired_recovered_orphan_is_edited_closed_instead_of_reposted() {
    let nonce = lfg::lfg_nonce("lfg-expired-recovered");
    let f = Fixture::new(move |r| {
        if r.method == "GET" {
            ScriptedResponse::json(
                200,
                json!([{
                    "id": "6001", "channel_id": CHANNEL,
                    "author": {"id": "1111", "bot": true}, "nonce": nonce,
                }]),
            )
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-expired-recovered", OLD, None, OLD)).await;
    let count = f.sweep(BOT, clock(), 20).await;
    let saved = f.saved("lfg-expired-recovered").await;
    let requests = f.finish().await;
    assert_eq!(count.expect("recover expired orphan"), 1);
    assert_eq!(saved.message_id.as_deref(), Some("6001"));
    assert_eq!(saved.status, LfgStatus::Closed);
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].path.ends_with("/messages/6001"));
    let payload: Value = serde_json::from_slice(&edits[0].body).expect("edit JSON");
    assert_eq!(payload["components"], json!([]));
    assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    assert!(payload["content"]
        .as_str()
        .expect("content")
        .to_lowercase()
        .contains("closed"));
    assert!(!requests.iter().any(|r| r.method == "POST"));
}

#[tokio::test]
async fn unreadable_orphan_history_closes_without_a_speculative_repost() {
    let f = Fixture::new(|r| {
        if r.method == "GET" {
            ScriptedResponse::status(403)
        } else {
            ok_rest(r)
        }
    })
    .await;
    f.seed(&post("lfg-unreadable", FUTURE, None, OLD)).await;
    let count = f.sweep(BOT, clock(), 20).await;
    let saved = f.saved("lfg-unreadable").await;
    let requests = f.finish().await;
    assert_eq!(count.expect("bounded refusal recovery"), 1);
    assert_eq!(saved.status, LfgStatus::Closed);
    assert!(saved.closed_at.is_some());
    assert!(saved.message_id.is_none());
    assert!(requests.iter().any(|r| r.method == "GET"));
    assert!(!requests.iter().any(|r| r.method != "GET"));
}

#[tokio::test]
async fn zero_bot_identity_refuses_without_database_or_discord_mutation() {
    let f = Fixture::new(ok_rest).await;
    let row = post("lfg-zero-bot", OLD, None, OLD);
    f.seed(&row).await;
    let result = f.sweep(0, clock(), 20).await;
    let saved = f.saved(&row.id).await;
    let requests = f.finish().await;
    assert!(
        result.is_err(),
        "no authoritative bot identity must fail closed"
    );
    assert_eq!(saved, row);
    assert!(requests.is_empty());
}

#[tokio::test]
async fn a_large_requested_budget_still_processes_at_most_twenty_rows() {
    let f = Fixture::new(ok_rest).await;
    // Bypass admission only in this owned disposable fixture: a preexisting
    // oversized backlog must not expand one production sweep's work budget.
    for n in 0..25 {
        sqlx::query("INSERT INTO lfg_posts (id, guild_id, channel_id, message_id, title, starts_at, status, created_by, created_at) VALUES ($1, $2, $3, $4, 'Raid', $5::timestamptz, 'open', $6, $5::timestamptz)")
            .bind(format!("lfg-budget-{n:02}"))
            .bind(GUILD).bind(CHANNEL).bind((5000 + n).to_string())
            .bind(OLD).bind(OWNER).execute(f.db.pool()).await.expect("seed stale backlog");
    }
    let count = f.sweep(BOT, clock(), 100).await;
    let closed: i64 = sqlx::query_scalar("SELECT count(*) FROM lfg_posts WHERE status = 'closed'")
        .fetch_one(f.db.pool())
        .await
        .expect("closed count");
    let remaining = f.sweep(BOT, clock(), 100).await;
    let settled = f.sweep(BOT, clock(), 100).await;
    let requests = f.finish().await;
    assert_eq!(count.expect("bounded sweep"), 20);
    assert_eq!(closed, 20);
    assert_eq!(remaining.expect("remaining backlog"), 5);
    assert_eq!(settled.expect("settled"), 0);
    assert_eq!(requests.iter().filter(|r| r.method == "PATCH").count(), 25);
    assert!(!requests.iter().any(|r| r.method == "POST"));
}

#[test]
fn the_close_identifier_survives_a_full_roster_at_the_message_limit() {
    let row = post("lfg-full-roster", FUTURE, Some("5001"), OLD);
    let roles = lfg::spec_roles(
        &row.id,
        &lfg::parse_role_spec("squad:Squad:99").expect("roles"),
    );
    let signups: Vec<_> = (0..99)
        .map(|n| LfgSignup {
            lfg_id: row.id.clone(),
            user_id: (10_000_000_000_000_000_000_u64 + n).to_string(),
            role_key: "squad".into(),
            joined_at: OLD.into(),
        })
        .collect();
    let content = lfg::lfg_content(&row, &roles, &signups);
    assert!(content.chars().count() <= 2000, "Discord message bound");
    assert!(
        content.contains(&row.id),
        "roster truncation must not strand the close identifier"
    );
}

#[tokio::test]
async fn create_replay_cannot_repost_or_reopen_an_orphan_closed_by_the_sweep() {
    let f = Fixture::new(ok_rest).await;
    // A crash after persistence and before send leaves the original create's
    // deterministic row. The sweep closes it when no bot nonce can be found.
    f.seed(&post("lfg-48001", FUTURE, None, OLD)).await;
    let swept = f.sweep(BOT, clock(), 20).await;
    let before = f.saved("lfg-48001").await;
    let request_count = f.rest.requests().len();
    let replay = f
        .service
        .execute(
            &f.executor,
            LfgRequest::Create {
                title: "Offline raid".into(),
                starts_at: FUTURE.into(),
                roles: "tank:Tank:1".into(),
                channel_id: CHANNEL.into(),
            },
            GUILD,
            OWNER,
            48001,
            BOT,
        )
        .await;
    let after = f.saved("lfg-48001").await;
    let requests = f.finish().await;
    assert_eq!(swept.expect("close orphan"), 1);
    assert_eq!(before.status, LfgStatus::Closed);
    assert!(
        replay.is_ok(),
        "terminal Create replay must reply successfully: {replay:?}"
    );
    assert_eq!(
        after, before,
        "Create replay cannot mutate the terminal row"
    );
    assert!(
        !requests[request_count..].iter().any(|r| r.method == "POST"),
        "Create replay must not send a closed orphan to Discord: {replay:?}"
    );
}

#[tokio::test]
async fn runtime_create_replay_of_a_swept_orphan_replies_without_reposting_or_mutating() {
    let f = Fixture::new(|request| {
        if request.path.starts_with("/api/v10/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ok_rest(request)
        }
    })
    .await;
    f.seed(&post("lfg-48002", FUTURE, None, OLD)).await;
    let swept = f.sweep(BOT, clock(), 20).await;
    let before = f.saved("lfg-48002").await;
    let roles_before = store::list_lfg_roles(f.db.pool(), &before.id)
        .await
        .expect("roles before");
    let signups_before = store::list_lfg_signups(f.db.pool(), &before.id)
        .await
        .expect("signups before");
    let request_count = f.rest.requests().len();
    let runtime = replay_runtime(&f);
    let replay = runtime.handle(&create_replay(48002)).await;
    let after = f.saved("lfg-48002").await;
    let roles_after = store::list_lfg_roles(f.db.pool(), &before.id)
        .await
        .expect("roles after");
    let signups_after = store::list_lfg_signups(f.db.pool(), &before.id)
        .await
        .expect("signups after");
    drop(runtime);
    let requests = f.finish().await;

    assert_eq!(swept.expect("close orphan"), 1);
    assert_eq!(before.status, LfgStatus::Closed);
    assert_eq!(before.message_id, None, "history proved nonce absent");
    assert!(
        matches!(replay, Ok(true)),
        "Create replay must succeed, not fail receipt validation: {replay:?}"
    );
    assert_eq!(after, before, "terminal post must remain byte-identical");
    assert_eq!(roles_after, roles_before);
    assert_eq!(signups_after, signups_before);
    let replay_requests = &requests[request_count..];
    assert!(
        !replay_requests
            .iter()
            .any(|r| { r.method == "POST" && r.path.starts_with("/api/v10/channels/") }),
        "terminal Create replay must never repost"
    );
    assert_eq!(
        replay_requests
            .iter()
            .filter(|r| { r.method == "POST" && r.path.starts_with("/api/v10/interactions/") })
            .count(),
        1,
        "acknowledge the replay once"
    );
    let replies: Vec<_> = replay_requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.starts_with("/api/v10/webhooks/"))
        .collect();
    assert_eq!(
        replies.len(),
        1,
        "finish the deferred interaction successfully"
    );
    let reply: Value = serde_json::from_slice(&replies[0].body).expect("reply JSON");
    let content = reply["content"].as_str().expect("reply content");
    assert!(!content.is_empty(), "a successful replay must have a reply");
    assert!(!content.contains("invalid mutation receipt"), "{content}");
}

fn replay_runtime(f: &Fixture) -> InteractionRuntime {
    InteractionRuntime::new(
        two_bot_core::RouterGates {
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
        f.executor.clone(),
        BOT,
        two_bot_core::ClassifierConfig::default(),
    )
}

fn create_replay(id: u64) -> twilight_model::application::interaction::Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": BOT.to_string(), "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {
            "user": {"id": OWNER, "username": "member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
            "permissions": two_bot_core::commands::PERM_MANAGE_EVENTS.to_string()
        },
        "token": "offline-lfg-replay-token", "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "lfg", "type": 1, "options": [
            {"type": 3, "name": "title", "value": "Offline raid"},
            {"type": 3, "name": "starts-at", "value": FUTURE},
            {"type": 3, "name": "roles", "value": "tank:Tank:1"}
        ]}
    }))
    .expect("wire Create replay")
}

#[tokio::test]
async fn runtime_closed_recovery_retries_failed_refresh_without_reposting_or_losing_saved_state() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let visible = Arc::new(AtomicBool::new(false));
    let patches = Arc::new(AtomicUsize::new(0));
    let history_visible = visible.clone();
    let channel_patches = patches.clone();
    let f = Fixture::new(move |request| {
        if request.method == "GET" && history_visible.load(Ordering::SeqCst) {
            ScriptedResponse::json(
                200,
                json!([{
                    "id": "5003", "channel_id": CHANNEL,
                    "author": {"id": BOT.to_string(), "bot": true},
                    "nonce": lfg::lfg_nonce("lfg-48003")
                }]),
            )
        } else if request.method == "PATCH" && request.path.starts_with("/api/v10/channels/") {
            if channel_patches.fetch_add(1, Ordering::SeqCst) == 0 {
                ScriptedResponse::status(500)
            } else {
                ScriptedResponse::json(200, json!({"id": "5003"}))
            }
        } else if request.path.starts_with("/api/v10/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ok_rest(request)
        }
    })
    .await;
    f.seed(&post("lfg-48003", FUTURE, None, OLD)).await;
    store::signup_lfg(f.db.pool(), GUILD, "lfg-48003", "tank", "3334", OLD)
        .await
        .expect("retained signup");
    let swept = f.sweep(BOT, clock(), 20).await;
    let before = f.saved("lfg-48003").await;
    let roles_before = store::list_lfg_roles(f.db.pool(), &before.id)
        .await
        .expect("roles before");
    let signups_before = store::list_lfg_signups(f.db.pool(), &before.id)
        .await
        .expect("signups before");
    let audits_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE target_key = $1")
            .bind(&before.id)
            .fetch_one(f.db.pool())
            .await
            .expect("audits before");
    let request_count = f.rest.requests().len();
    visible.store(true, Ordering::SeqCst);
    let first_runtime = replay_runtime(&f);
    let first = first_runtime.handle(&create_replay(48003)).await;
    let recovered = f.saved("lfg-48003").await;
    drop(first_runtime);
    // Restart excludes an in-memory response cache as evidence of retry safety.
    let restarted = replay_runtime(&f);
    let second = restarted.handle(&create_replay(48003)).await;
    let after = f.saved("lfg-48003").await;
    let roles_after = store::list_lfg_roles(f.db.pool(), &before.id)
        .await
        .expect("roles after");
    let signups_after = store::list_lfg_signups(f.db.pool(), &before.id)
        .await
        .expect("signups after");
    let audits_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE target_key = $1")
            .bind(&before.id)
            .fetch_one(f.db.pool())
            .await
            .expect("audits after");
    drop(restarted);
    let requests = f.finish().await;

    assert_eq!(swept.expect("close orphan"), 1);
    assert_eq!(before.status, LfgStatus::Closed);
    assert_eq!(before.message_id, None);
    assert!(
        matches!(first, Ok(true)),
        "saved recovery must return its truthful outcome: {first:?}"
    );
    assert!(
        matches!(second, Ok(true)),
        "replayed refresh must succeed: {second:?}"
    );
    let mut expected = before;
    expected.message_id = Some("5003".into());
    assert_eq!(
        recovered, expected,
        "only authoritative nonce binding may change"
    );
    assert_eq!(
        after, recovered,
        "refresh retry cannot reopen or rewrite the post"
    );
    assert_eq!(roles_after, roles_before);
    assert_eq!(signups_before.len(), 1, "nonempty retention control");
    assert_eq!(signups_after, signups_before);
    assert_eq!(
        audits_after, audits_before,
        "no audit duplication on replay"
    );
    assert_eq!(
        patches.load(Ordering::SeqCst),
        2,
        "retry must refresh the already-known message"
    );
    let replay_requests = &requests[request_count..];
    assert!(!replay_requests
        .iter()
        .any(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/")));
    let refreshes: Vec<_> = replay_requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.starts_with("/api/v10/channels/"))
        .collect();
    assert_eq!(refreshes.len(), 2);
    for refresh in refreshes {
        assert_eq!(refresh.path, "/api/v10/channels/4444/messages/5003");
        let body: Value = serde_json::from_slice(&refresh.body).expect("refresh JSON");
        assert_eq!(
            body["components"],
            json!([]),
            "closed messages have no signup controls"
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
    let replies: Vec<Value> = replay_requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.starts_with("/api/v10/webhooks/"))
        .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 2, "both deferred interactions must finish");
    let first_reply = replies[0]["content"].as_str().expect("first content");
    assert!(
        first_reply.to_lowercase().contains("closed"),
        "{first_reply}"
    );
    assert!(
        first_reply.contains("Message refresh failed; the saved result above still applies."),
        "{first_reply}"
    );
    let second_reply = replies[1]["content"].as_str().expect("second content");
    assert!(
        second_reply.to_lowercase().contains("closed"),
        "{second_reply}"
    );
    assert!(
        !second_reply.contains("Message refresh failed"),
        "{second_reply}"
    );
}
