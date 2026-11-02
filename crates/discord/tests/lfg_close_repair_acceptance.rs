//! Protected RED-first acceptance: durable repair of already-closed LFG messages.
//! Only Discord is doubled; close, audit, locks, migrations and restart use real stores.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

#[path = "support/lfg_close_repair_edges.rs"]
mod edges;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use time::{Duration, OffsetDateTime};
use two_bot_core::{
    lfg::{self, LfgPost, LfgRole, LfgSignup, LfgStatus, SignupOutcome},
    lfg_store as store,
    send_admission::{PgSendAdmission, SendAdmission},
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
const TOKEN: &str = "offline-lfg-close-repair-fixture";

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
        assert!(body["content"]
            .as_str()
            .expect("content")
            .to_lowercase()
            .contains("closed"));
    }
}

struct Fixture {
    db: TestDatabase,
    rest: MockRest,
    executor: ActionExecutor,
    status: Arc<AtomicU16>,
}

impl Fixture {
    async fn new(status: u16) -> Self {
        Self::with_history(status, None).await
    }

    async fn with_history(status: u16, history: Option<Value>) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("disposable test bootstrap required; no skip");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable database");
        let status = Arc::new(AtomicU16::new(status));
        let mode = status.clone();
        let rest = MockRest::with_responder(move |r| {
            if is_edit(r) {
                match mode.load(Ordering::SeqCst) {
                    // A request reached Discord but its response is unconfirmed.
                    599 => ScriptedResponse::status(200).delayed(std::time::Duration::from_secs(6)),
                    404 => ScriptedResponse::json(
                        404,
                        json!({"code": 10008, "message": "Unknown Message"}),
                    ),
                    // An unclassified 404 must not masquerade as Unknown Message.
                    498 => ScriptedResponse::json(404, json!({"message": "REMOTE_BODY_SENTINEL"})),
                    497 => {
                        if r.path.ends_with("/messages/5001") {
                            ScriptedResponse::status(403)
                        } else {
                            ScriptedResponse::json(200, json!({"id": "5002"}))
                        }
                    }
                    code => ScriptedResponse::json(
                        code,
                        json!({"id": "5001", "message": "REMOTE_BODY_SENTINEL"}),
                    ),
                }
            } else if r.method == "GET" {
                ScriptedResponse::json(200, history.clone().unwrap_or(json!([])))
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

    async fn roster(&self, id: &str) -> (Vec<LfgRole>, Vec<LfgSignup>) {
        (
            store::list_lfg_roles(self.db.pool(), id)
                .await
                .expect("roles"),
            store::list_lfg_signups(self.db.pool(), id)
                .await
                .expect("signups"),
        )
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

    // Every tick is a new service on a genuinely independent pool. No in-memory
    // pending set, receipt cache or cloned pool can explain successful recovery.
    async fn tick_with(
        &self,
        executor: &ActionExecutor,
        guild: &str,
        bot: u64,
        at: OffsetDateTime,
        limit: usize,
    ) -> Result<usize, LfgError> {
        let peer = self.db.independent_pool().await.expect("restart pool");
        let service = LfgInteractions::new(peer.clone());
        let result = service
            .sweep_lifecycle(executor, guild, bot, at, limit)
            .await;
        drop(service);
        peer.close().await;
        result
    }

    async fn tick(&self, at: OffsetDateTime, limit: usize) -> usize {
        self.tick_with(&self.executor, GUILD, BOT, at, limit)
            .await
            .expect("bounded sweep")
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

async fn explicit_failure_then_restart(fault: u16) {
    let f = Fixture::new(fault).await;
    f.seed(&post("repair-explicit", Some("5001"), FUTURE)).await;
    let roster = f.roster("repair-explicit").await;
    let reply = f.close("repair-explicit").await;
    let closed = f.saved("repair-explicit").await;
    let before = edits(&f.rest.requests()).len();
    let refused =
        store::signup_lfg(f.db.pool(), GUILD, "repair-explicit", "healer", "3335", OLD).await;
    f.status.store(200, Ordering::SeqCst);
    let repaired = f.tick(clock(), 20).await;
    let after = f.saved("repair-explicit").await;
    let retained = f.roster("repair-explicit").await;
    let settled = f.settled(clock() + Duration::days(1)).await;
    let requests = f.finish().await;
    assert_eq!(closed.status, LfgStatus::Closed);
    assert!(closed.closed_at.is_some());
    assert_eq!(
        refused.expect("saved closure refuses signup"),
        SignupOutcome::Closed
    );
    assert_eq!(after, closed, "repair must not rewrite domain state");
    assert_eq!(
        retained, roster,
        "preserve identities, roles and exact joined_at"
    );
    let reply = reply
        .expect("truthful committed-close response")
        .to_lowercase();
    assert!(reply.contains("clos"));
    assert!(
        reply.contains("refresh"),
        "disclose unconfirmed edit: {reply}"
    );
    assert!(!reply.contains("unchanged"));
    assert!(!reply.contains("remote_body_sentinel"));
    assert_eq!(before, 1, "fault was exercised after committing the close");
    assert!(
        repaired > 0,
        "RED: a durable closed-message edit must survive restart"
    );
    assert_eq!(
        edits(&requests).len(),
        before + 1,
        "one repair of the saved message"
    );
    assert!(settled, "confirmed repair must clear pending work");
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn forbidden_close_edit_is_repaired_after_restart_without_reopening_or_reposting() {
    explicit_failure_then_restart(403).await;
}

#[tokio::test]
async fn timed_out_close_edit_remains_retryable_after_restart() {
    explicit_failure_then_restart(599).await;
}

#[tokio::test]
async fn confirmed_close_edit_is_not_retried_or_reclassified_as_pending() {
    let f = Fixture::new(200).await;
    f.seed(&post("repair-success", Some("5001"), FUTURE)).await;
    let reply = f.close("repair-success").await;
    let closed = f.saved("repair-success").await;
    let quiet = f.settled(clock()).await && f.settled(clock() + Duration::days(1)).await;
    let requests = f.finish().await;
    assert!(reply
        .expect("close success")
        .to_lowercase()
        .contains("clos"));
    assert_eq!(closed.status, LfgStatus::Closed);
    assert!(quiet, "successful edit is already settled");
    assert_eq!(edits(&requests).len(), 1);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn definitive_unknown_message_settles_without_recreating_it() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-missing", Some("5001"), FUTURE)).await;
    f.close("repair-missing").await.expect("saved closure");
    let before = f.saved("repair-missing").await;
    f.status.store(404, Ordering::SeqCst);
    let attempted = f.tick(clock(), 20).await;
    let count = edits(&f.rest.requests()).len();
    f.status.store(200, Ordering::SeqCst);
    let quiet = f.settled(clock() + Duration::days(1)).await;
    let after = f.saved("repair-missing").await;
    let requests = f.finish().await;
    assert_eq!(
        after, before,
        "missing remote message never deletes recovery evidence"
    );
    assert!(
        attempted > 0,
        "RED: attempt the pending edit before classifying absence"
    );
    assert_eq!(count, 2);
    assert!(
        quiet,
        "definitive Unknown Message may settle, not retry forever"
    );
    assert_only_closed_edits(&requests, &["5001"]);
}

async fn lifecycle_failure_then_restart(recovered_orphan: bool) {
    let id = if recovered_orphan {
        "repair-orphan"
    } else {
        "repair-expiry"
    };
    let history = recovered_orphan.then(|| {
        json!([{
            "id": "5001", "channel_id": CHANNEL,
            "author": {"id": BOT.to_string(), "bot": true}, "nonce": lfg::lfg_nonce(id)
        }])
    });
    let f = Fixture::with_history(403, history).await;
    f.seed(&post(
        id,
        if recovered_orphan { None } else { Some("5001") },
        OLD,
    ))
    .await;
    let roster = f.roster(id).await;
    let first = f.tick(clock(), 20).await;
    let closed = f.saved(id).await;
    let initial_edits = edits(&f.rest.requests()).len();
    f.status.store(200, Ordering::SeqCst);
    let repaired = f.tick(clock() + Duration::days(1), 20).await;
    let after = f.saved(id).await;
    let retained = f.roster(id).await;
    let quiet = f.settled(clock() + Duration::days(2)).await;
    let requests = f.finish().await;
    assert_eq!(first, 1);
    assert_eq!(closed.status, LfgStatus::Closed);
    assert_eq!(closed.message_id.as_deref(), Some("5001"));
    assert_eq!(initial_edits, 1, "actually failed a close refresh");
    assert_eq!(after, closed);
    assert_eq!(retained, roster);
    assert!(
        repaired > 0,
        "RED: lifecycle closure must persist its failed refresh"
    );
    assert_eq!(edits(&requests).len(), 2);
    assert!(quiet);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn expiry_failed_edit_is_durable_and_retryable_after_restart() {
    lifecycle_failure_then_restart(false).await;
}

#[tokio::test]
async fn recovered_expired_orphan_failed_edit_is_durable_after_restart() {
    lifecycle_failure_then_restart(true).await;
}

#[tokio::test]
async fn unreadable_orphan_history_never_authorizes_a_replacement_or_guessed_edit() {
    let f = Fixture::with_history(200, Some(json!([{"id": "5001", "nonce": "untrusted"}]))).await;
    f.seed(&post("repair-unreadable", None, FUTURE)).await;
    let first = f.tick(clock(), 20).await;
    let closed = f.saved("repair-unreadable").await;
    f.tick(clock() + Duration::days(1), 20).await;
    let after = f.saved("repair-unreadable").await;
    let requests = f.finish().await;
    assert_eq!(first, 1);
    assert_eq!(closed.status, LfgStatus::Closed);
    assert_eq!(closed.message_id, None);
    assert_eq!(after, closed);
    assert!(!requests.is_empty(), "history failure control actually ran");
    assert!(
        requests.iter().all(|r| r.method == "GET"),
        "uncertainty is not send permission"
    );
}

#[tokio::test]
async fn close_commits_pending_repair_even_when_independent_audit_append_fails() {
    let f = Fixture::new(200).await;
    f.seed(&post("repair-audit", Some("5001"), FUTURE)).await;
    sqlx::query(
        "ALTER TABLE announcements_audit_log ADD CONSTRAINT repair_audit_fault CHECK (false)",
    )
    .execute(f.db.pool())
    .await
    .expect("disposable audit fault");
    let reply = f.close("repair-audit").await;
    let closed = f.saved("repair-audit").await;
    let before = f.rest.requests();
    sqlx::query("ALTER TABLE announcements_audit_log DROP CONSTRAINT repair_audit_fault")
        .execute(f.db.pool())
        .await
        .expect("remove disposable fault");
    let repaired = f.tick(clock(), 20).await;
    let after = f.saved("repair-audit").await;
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log")
        .fetch_one(f.db.pool())
        .await
        .expect("audit count");
    let requests = f.finish().await;
    let text = reply.expect("truthful saved-close response").to_lowercase();
    assert!(text.contains("clos") && text.contains("audit"));
    assert!(!text.contains("repair_audit_fault"));
    assert_eq!(closed.status, LfgStatus::Closed);
    assert_eq!(after, closed);
    assert!(
        before.is_empty(),
        "retain no-refresh-after-audit-loss semantics"
    );
    assert_eq!(
        audits, 0,
        "repair must not fabricate the missing original audit"
    );
    assert!(
        repaired > 0,
        "RED: independent audit failure must not lose close-repair intent"
    );
    assert_eq!(edits(&requests).len(), 1);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn rejected_close_transaction_produces_neither_closed_state_nor_repair_effect() {
    let f = Fixture::new(200).await;
    f.seed(&post("repair-db-fault", Some("5001"), FUTURE)).await;
    let before = f.saved("repair-db-fault").await;
    sqlx::query("ALTER TABLE lfg_posts ADD CONSTRAINT repair_close_fault CHECK (status = 'open')")
        .execute(f.db.pool())
        .await
        .expect("disposable close fault");
    let reply = f.close("repair-db-fault").await;
    sqlx::query("ALTER TABLE lfg_posts DROP CONSTRAINT repair_close_fault")
        .execute(f.db.pool())
        .await
        .expect("remove disposable fault");
    f.tick(clock(), 20).await;
    let after = f.saved("repair-db-fault").await;
    let requests = f.finish().await;
    assert!(reply.is_err(), "closure really failed before commit");
    assert_eq!(
        after, before,
        "atomic rejection cannot leave a phantom close"
    );
    assert!(
        requests.is_empty(),
        "no repair intent may escape failed close transaction"
    );
}

#[tokio::test]
async fn contended_pending_repair_skips_without_starving_an_unrelated_pending_row() {
    let f = Fixture::new(403).await;
    for (id, message) in [("repair-a-held", "5001"), ("repair-b-free", "5002")] {
        f.seed(&post(id, Some(message), FUTURE)).await;
        f.close(id).await.expect("saved close");
    }
    let holder_pool = f.db.independent_pool().await.expect("lock-holder pool");
    let mut holder = holder_pool.begin().await.expect("holder transaction");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lfg-runtime:{GUILD}:repair-a-held"))
        .execute(&mut *holder)
        .await
        .expect("hold existing runtime lock before tick");
    f.status.store(200, Ordering::SeqCst);
    let before = f.rest.requests().len();
    let tick = tokio::time::timeout(std::time::Duration::from_secs(3), f.tick(clock(), 20)).await;
    let during = f.rest.requests();
    let release = holder.rollback().await;
    holder_pool.close().await;
    let retry = f.tick(clock() + Duration::days(1), 20).await;
    let requests = f.finish().await;
    release.expect("release peer lock");
    assert!(tick.is_ok(), "contended work must not hold the batch");
    let progress = edits(&during[before..]);
    assert_eq!(
        progress.len(),
        1,
        "RED: unrelated pending row must progress"
    );
    assert!(progress[0].path.ends_with("/messages/5002"));
    assert!(retry > 0, "contended row is left durably eligible");
    assert_eq!(edits(&requests).len(), 4);
    assert_only_closed_edits(&requests, &["5001", "5002"]);
}

#[tokio::test]
async fn zero_budget_unknown_identity_foreign_guild_and_historical_rows_are_no_effect_controls() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-fences", Some("5001"), FUTURE)).await;
    f.close("repair-fences").await.expect("close");
    let closed = f.saved("repair-fences").await;
    let mut historical = post("repair-historical", Some("5002"), OLD);
    historical.status = LfgStatus::Closed;
    historical.closed_at = Some(OLD.into());
    f.seed(&historical).await;
    f.status.store(200, Ordering::SeqCst);
    let before = f.rest.requests().len();
    let zero = f.tick(clock(), 0).await;
    let unknown = f.tick_with(&f.executor, GUILD, 0, clock(), 20).await;
    let foreign = f.tick_with(&f.executor, "9999", BOT, clock(), 20).await;
    let fenced = f.rest.requests();
    let repaired = f.tick(clock(), 20).await;
    let after = f.saved("repair-fences").await;
    let old_after = f.saved("repair-historical").await;
    let requests = f.finish().await;
    assert_eq!(zero, 0);
    assert!(unknown.is_err());
    assert_eq!(foreign.expect("empty foreign pass"), 0);
    assert_eq!(fenced.len(), before, "fences precede REST");
    assert_eq!(after, closed);
    assert_eq!(old_after, historical);
    assert!(
        repaired > 0,
        "RED: authorized pending row remains repairable"
    );
    assert_eq!(
        edits(&requests).len(),
        2,
        "old closed rows are not a bulk backfill"
    );
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn repair_respects_real_durable_send_admission_and_remains_retryable() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-send-fence", Some("5001"), FUTURE))
        .await;
    f.close("repair-send-fence").await.expect("close");
    let admission =
        Arc::new(PgSendAdmission::new(f.db.pool().clone(), TOKEN).expect("fixture admission"));
    let held = admission.admit().await.expect("hold token-wide send lane");
    let executor = ActionExecutor::with_admission(TOKEN.into(), Some(f.rest.origin()), admission)
        .expect("governed executor");
    f.status.store(200, Ordering::SeqCst);
    let before = f.rest.requests().len();
    let _blocked = f.tick_with(&executor, GUILD, BOT, clock(), 20).await;
    let during = f.rest.requests().len();
    held.complete(None)
        .await
        .expect("release known pre-wire fixture holder");
    let retry = f
        .tick_with(&executor, GUILD, BOT, clock() + Duration::days(1), 20)
        .await;
    drop(executor);
    let requests = f.finish().await;
    assert_eq!(during, before, "repair must not bypass durable admission");
    assert!(
        retry.expect("retry after admission release") > 0,
        "RED: admission refusal cannot discard repair intent"
    );
    assert_eq!(edits(&requests).len(), 2);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn repeated_forbidden_repairs_back_off_and_eventually_repair_after_permissions_return() {
    let f = Fixture::new(403).await;
    f.seed(&post("repair-backoff", Some("5001"), FUTURE)).await;
    f.close("repair-backoff").await.expect("close");
    let initial = edits(&f.rest.requests()).len();
    let first = f.tick(clock(), 1).await;
    let attempted = edits(&f.rest.requests()).len();
    for _ in 0..8 {
        f.tick(clock(), 1).await;
    }
    let burst = edits(&f.rest.requests()).len();
    f.status.store(200, Ordering::SeqCst);
    let later = f.tick(clock() + Duration::days(1), 1).await;
    let requests = f.finish().await;
    assert!(
        first > 0 && attempted > initial,
        "RED: pending refusal is actually attempted"
    );
    assert_eq!(
        burst, attempted,
        "same-time ticks must honor durable backoff, not hammer Discord"
    );
    assert!(
        later > 0,
        "uncertain/forbidden work cannot be falsely settled"
    );
    assert_eq!(edits(&requests).len(), attempted + 1);
    assert_only_closed_edits(&requests, &["5001"]);
}

#[tokio::test]
async fn pending_repair_batch_obeys_budget_and_never_repeats_settled_rows() {
    let f = Fixture::new(403).await;
    for n in 0..23 {
        f.seed(&post(
            &format!("repair-budget-{n}"),
            Some(&format!("{}", 5100 + n)),
            FUTURE,
        ))
        .await;
        f.close(&format!("repair-budget-{n}")).await.expect("close");
    }
    f.status.store(200, Ordering::SeqCst);
    let initial = edits(&f.rest.requests()).len();
    f.tick(clock(), usize::MAX).await;
    let wide_batch = edits(&f.rest.requests()).len() - initial;
    let mut previous = edits(&f.rest.requests()).len();
    let mut sizes = Vec::new();
    for _ in 0..24 {
        f.tick(clock(), 2).await;
        let next = edits(&f.rest.requests()).len();
        sizes.push(next - previous);
        previous = next;
    }
    let requests = f.finish().await;
    assert!(
        wide_batch <= 20,
        "the existing hard sweep cap applies even with an unlimited caller budget"
    );
    assert!(
        sizes.iter().all(|n| *n <= 2),
        "per-tick wire work is bounded: {sizes:?}"
    );
    assert_eq!(
        wide_batch + sizes.iter().sum::<usize>(),
        23,
        "RED: all pending rows eventually progress under a bounded budget"
    );
    let messages: Vec<String> = (5100..5123).map(|id| id.to_string()).collect();
    for message in &messages {
        assert_eq!(
            edits(&requests)
                .iter()
                .filter(|r| r.path.ends_with(&format!("/messages/{message}")))
                .count(),
            2
        );
    }
    let allowed: Vec<&str> = messages.iter().map(String::as_str).collect();
    assert_only_closed_edits(&requests, &allowed);
}

fn close_interaction(guild: &str) -> twilight_model::application::interaction::Interaction {
    serde_json::from_value(json!({
        "id": "89002", "application_id": BOT.to_string(), "type": 2,
        "guild_id": guild, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": OWNER, "username": "fixture", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
            "permissions": two_bot_core::commands::PERM_MANAGE_EVENTS.to_string()},
        "token": "offline-close-interaction", "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "lfg-close", "type": 1, "options": [{"type": 3, "name": "id", "value": "repair-disabled"}]}
    })).expect("close interaction wire shape")
}

#[tokio::test]
async fn disabled_and_foreign_guild_close_interactions_never_create_repair_work() {
    let f = Fixture::new(200).await;
    f.seed(&post("repair-disabled", Some("5001"), FUTURE)).await;
    let before = f.saved("repair-disabled").await;
    for (enabled, guild) in [(false, GUILD), (true, "9999")] {
        let runtime = InteractionRuntime::new(
            two_bot_core::RouterGates {
                configured_guild: Some(2222),
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
            },
            f.db.pool().clone(),
            f.executor.clone(),
            BOT,
            two_bot_core::ClassifierConfig::default(),
        );
        runtime
            .handle(&close_interaction(guild))
            .await
            .expect("refusal transport");
        drop(runtime);
    }
    f.tick(clock(), 20).await;
    let after = f.saved("repair-disabled").await;
    let requests = f.finish().await;
    assert_eq!(after, before);
    assert!(
        !requests
            .iter()
            .any(|r| r.path.starts_with("/api/v10/channels/")),
        "activation and guild fences precede effects"
    );
}
