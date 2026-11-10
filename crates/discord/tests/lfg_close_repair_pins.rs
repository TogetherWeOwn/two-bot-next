//! Builder-owned pins for the durable close-refresh repair contract.
//! Covers two behaviours the protected acceptance suite does not probe:
//! a message first bound to an already-closed post re-arms repair, and rows
//! deferred past the sweep clock cannot crowd expiry work out of the shared
//! 20-row batch. Only Discord is doubled; close, repair, locks, migrations
//! and restart use real stores, as in the protected acceptance target.
#![cfg(feature = "db")]

use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use time::{Duration, OffsetDateTime};
use two_bot_core::{
    lfg::{self, LfgPost, LfgStatus, SignupOutcome},
    lfg_store as store,
};
use two_bot_discord::{
    lfg_interactions::{LfgError, LfgInteractions, LfgRequest},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

#[allow(dead_code)]
mod common;
use common::{MockRest, RestRequest, ScriptedResponse};

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: u64 = 1111;
const OWNER: &str = "3333";
const OLD: &str = "2000-01-01T00:00:00.000Z";
const FUTURE: &str = "2199-01-01T00:00:00.000Z";
const TOKEN: &str = "offline-lfg-close-repair-pins";

fn clock() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(4_102_444_800).expect("2100 UTC")
}

fn post(id: &str, message: Option<&str>, starts_at: &str) -> LfgPost {
    LfgPost {
        id: id.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: message.map(str::to_owned),
        title: "Offline <@3334> raid".into(),
        starts_at: starts_at.into(),
        status: LfgStatus::Open,
        created_by: OWNER.into(),
        created_at: "1990-01-01T00:00:00.000Z".into(),
        closed_at: None,
    }
}

fn is_edit(r: &RestRequest) -> bool {
    r.method == "PATCH" && r.path.starts_with("/api/v10/channels/")
}

fn edits(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests.iter().filter(|r| is_edit(r)).collect()
}

/// Every mutating channel call must be a closed edit of an allowed saved
/// message; GET history reads may happen but nothing may send or delete.
fn assert_only_closed_edits(requests: &[RestRequest], allowed_messages: &[&str]) {
    for r in requests
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
    {
        if r.method == "GET" {
            continue;
        }
        assert_eq!(r.method, "PATCH", "no replacement send or deletion");
        assert!(
            allowed_messages
                .iter()
                .any(|id| r.path == format!("/api/v10/channels/{CHANNEL}/messages/{id}")),
            "edit only the exact saved message: {}",
            r.path
        );
        let body: Value = serde_json::from_slice(&r.body).expect("edit JSON");
        assert_eq!(
            body["components"],
            json!([]),
            "closed controls cannot return"
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }
}

struct Fixture {
    db: TestDatabase,
    rest: MockRest,
    executor: ActionExecutor,
    status: Arc<AtomicU16>,
}

impl Fixture {
    async fn with_history(status: u16, history: Value) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("disposable test bootstrap required; no skip");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable database");
        let status = Arc::new(AtomicU16::new(status));
        let mode = status.clone();
        let rest = MockRest::with_responder(move |r| {
            if is_edit(r) {
                let code = mode.load(Ordering::SeqCst);
                ScriptedResponse::json(code, json!({"id": "5001"}))
            } else if r.method == "GET" {
                ScriptedResponse::json(200, history.clone())
            } else if r.path.starts_with("/api/v10/interactions/") {
                ScriptedResponse::status(204)
            } else {
                ScriptedResponse::json(200, json!({"id": "5001"}))
            }
        })
        .await;
        let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin()))
            .expect("loopback executor");
        Self {
            db,
            rest,
            executor,
            status,
        }
    }

    async fn seed(&self, row: &LfgPost) {
        let roles = lfg::spec_roles(
            &row.id,
            &lfg::parse_role_spec("tank:Tank:2,healer:Healer:2").expect("roles"),
        );
        store::put_lfg(self.db.pool(), row, &roles, true)
            .await
            .expect("seed post");
        if row.status == LfgStatus::Open {
            assert_eq!(
                store::signup_lfg(
                    self.db.pool(),
                    &row.guild_id,
                    &row.id,
                    "tank",
                    "3334",
                    "1999-01-01T00:00:00.000Z"
                )
                .await
                .expect("seed retained member"),
                SignupOutcome::Joined
            );
        }
    }

    async fn saved(&self, id: &str) -> LfgPost {
        store::get_lfg(self.db.pool(), GUILD, id)
            .await
            .expect("read post")
            .expect("retained post")
    }

    async fn close(&self, id: &str) -> Result<String, LfgError> {
        LfgInteractions::new(self.db.pool().clone())
            .execute(
                &self.executor,
                LfgRequest::Close { post_id: id.into() },
                GUILD,
                OWNER,
                89001,
                BOT,
            )
            .await
    }

    // Every tick is a new service on a genuinely independent pool, so no
    // in-memory state can explain successful recovery.
    async fn tick(&self, at: OffsetDateTime, limit: usize) -> usize {
        let peer = self.db.independent_pool().await.expect("restart pool");
        let service = LfgInteractions::new(peer.clone());
        let result = service
            .sweep_lifecycle(&self.executor, GUILD, BOT, at, limit)
            .await;
        drop(service);
        peer.close().await;
        result.expect("bounded sweep")
    }

    async fn settled(&self, at: OffsetDateTime) -> bool {
        let before = edits(&self.rest.requests()).len();
        self.tick(at, 20).await;
        edits(&self.rest.requests()).len() == before
    }

    async fn finish(self) -> Vec<RestRequest> {
        let requests = self.rest.requests();
        self.rest.shutdown().await;
        drop(self.executor);
        self.db.close().await.expect("explicit fixture teardown");
        requests
    }
}

/// The recovery-gain case: a close commits without a saved message, the
/// no-message refresh settles the intent, and later the accepted message is
/// recovered from history and bound to the closed post. That bind must
/// re-arm durable repair so the next sweep removes the signup controls;
/// here the recovery bind is exercised through the sweep's own history
/// recovery, then converted to a settled-but-unbound state to prove that
/// the first bind re-arms repair.
#[tokio::test]
async fn recovery_bound_message_rearms_repair_after_a_failed_closed_edit() {
    let id = "lfg-97701";
    let history = json!([{
        "id": "5001", "channel_id": CHANNEL,
        "author": {"id": BOT.to_string(), "bot": true}, "nonce": lfg::lfg_nonce(id)
    }]);
    let f = Fixture::with_history(403, history).await;
    f.seed(&post(id, None, OLD)).await;
    // The sweep closes the expired post, binds and fails to edit message 5001.
    let swept = f.tick(clock(), 20).await;
    let closed = f.saved(id).await;
    assert_eq!(swept, 1);
    assert_eq!(closed.status, LfgStatus::Closed);
    assert_eq!(closed.message_id.as_deref(), Some("5001"));
    assert_eq!(edits(&f.rest.requests()).len(), 1, "expiry edit failed");
    // Model the earlier settlement the reviewer identified: while the post
    // was unbound, a refresh cleared the intent. Now treat the row as the
    // reviewer did (settled, then a message gets bound).
    sqlx::query(
        "UPDATE lfg_posts SET message_id = NULL, close_refresh_pending = FALSE,
         close_refresh_attempts = 0, close_refresh_next_at = NULL
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(id)
    .execute(f.db.pool())
    .await
    .expect("model prior no-message settlement");
    let settled = store::get_lfg_close_refresh(f.db.pool(), GUILD, id)
        .await
        .expect("refresh state")
        .expect("post refresh state");
    assert!(!settled.pending, "fixture must start settled");
    // The late bind re-arms repair in the same statement.
    store::save_lfg_message_id(f.db.pool(), GUILD, id, "5001")
        .await
        .expect("recovered bind");
    f.status.store(200, Ordering::SeqCst);
    let repaired = f.tick(clock() + Duration::days(1), 20).await;
    let quiet = f.settled(clock() + Duration::days(2)).await;
    let after = f.saved(id).await;
    let requests = f.finish().await;
    assert_eq!(after, closed, "repair must not rewrite domain state");
    assert!(
        repaired > 0,
        "a message first bound to a closed post must keep durable repair"
    );
    assert_eq!(
        edits(&requests).len(),
        2,
        "exactly one repair edit of the recovered message"
    );
    assert!(quiet, "confirmed repair clears the rearmed intent");
    assert_only_closed_edits(&requests, &["5001"]);
}

/// More than one sweep batch of deferred (not-yet-due) pending repairs must
/// not occupy the shared 20-row cap: an expiring open post still closes in
/// the same tick, and no deferred row is re-attempted before its backoff
/// elapses. Deleting the due-filter from the candidate scan turns this red.
#[tokio::test]
async fn deferred_repairs_do_not_crowd_expiry_out_of_the_shared_batch() {
    let f = Fixture::with_history(403, json!([])).await;
    for n in 0..21 {
        let id = format!("crowd-deferred-{n}");
        f.seed(&post(&id, Some(&(5100 + n).to_string()), FUTURE))
            .await;
        f.close(&id).await.expect("saved close");
        // One failed repair each, then pin the durable backoff an hour beyond
        // the crowd tick so every pending row is not yet due.
        store::defer_lfg_close_refresh(f.db.pool(), GUILD, &id, 1, clock() + Duration::hours(1))
            .await
            .expect("defer pending row");
    }
    f.seed(&post("crowd-expiring", Some("5200"), OLD)).await;
    f.status.store(200, Ordering::SeqCst);
    let edits_before = edits(&f.rest.requests()).len();
    let processed = f.tick(clock(), 20).await;
    let expiring = f.saved("crowd-expiring").await;
    let requests_after_tick = f.rest.requests();
    let deferred_state = store::get_lfg_close_refresh(f.db.pool(), GUILD, "crowd-deferred-0")
        .await
        .expect("refresh state")
        .expect("post refresh state");
    // After the backoff, the deferred rows become candidates again.
    let later = f.tick(clock() + Duration::hours(2), 20).await;
    let requests = f.finish().await;
    assert_eq!(
        expiring.status,
        LfgStatus::Closed,
        "expiring open post must not be crowded out of the 20-row cap"
    );
    assert!(processed >= 1, "the expiring post was processed");
    assert_eq!(
        edits(&requests_after_tick).len() - edits_before,
        1,
        "the crowd tick edits only the expiring post"
    );
    assert!(
        edits(&requests_after_tick)[edits_before]
            .path
            .ends_with("/messages/5200"),
        "that edit targets the expiring post"
    );
    assert!(deferred_state.pending, "deferred rows stay pending");
    assert!(later > 0, "deferred rows progress once due");
    let mut allowed: Vec<String> = (0..21).map(|n| (5100 + n).to_string()).collect();
    allowed.push("5200".into());
    let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
    assert_only_closed_edits(&requests, &allowed_refs);
}
