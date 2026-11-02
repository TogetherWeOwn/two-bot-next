#![cfg(feature = "db")]

//! Protected behavior corpus for actor-attributed giveaway lifecycle writes.
//! Run against the disposable database only:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//! --test giveaway_actor_audit_acceptance -- --test-threads=1
//!
//! No future API shim: every command uses the real router, executor and store.
//! Missing actor facts and non-atomic writes must fail assertions, not compilation.
//! The loopback Discord double replaces transport only. SQL uses TestDatabase.
//! `control_` cases preserve refusals and existing cancel/draw/reroll contracts.

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{types::time::OffsetDateTime, PgPool};
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_GUILD, giveaway_store as store, ClassifierConfig, InteractionRouter,
    RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060701";
const FOREIGN: &str = "1550102030405060799";
const CHANNEL: &str = "1550102030405060702";
const CREATOR: &str = "1550102030405060710";
const ACTOR: &str = "1550102030405060711";
const OTHER: &str = "1550102030405060712";
const ENTRANT: &str = "1550102030405060721";
const TARGET: &str = "gw_actor_audit";
const PRIZE: &str = "private-prize-body-must-not-appear";
const NONCE: &str = "private-draw-nonce-must-not-appear";
const CLAIM: &str = "private-claim-capability-must-not-appear";
const FAILURE: &str = "Giveaway unavailable; try again later.";
// Keep unrelated cases from occupying the runtime effects mutex while a lock
// witness waits. This does not serialize the independent workers within a case.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_760_000_000 + seconds).expect("fixed stamp")
}

async fn fixture() -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create guarded disposable database"),
        ),
        Err(std::env::VarError::NotPresent) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the disposable database; an absent URL is not RED"
            );
            eprintln!("SKIP giveaway actor audit: TWO_TEST_DATABASE_URL absent");
            None
        }
        Err(_) => panic!("test bootstrap URL must be Unicode"),
    }
}

async fn seed(pool: &PgPool, status: &str) -> store::GiveawayRow {
    store::create_giveaway(
        pool,
        &store::NewGiveaway {
            guild_id: GUILD.into(),
            id: TARGET.into(),
            prize: PRIZE.into(),
            channel_id: CHANNEL.into(),
            message_id: "1550102030405060703".into(),
            created_by: CREATOR.into(),
            winners_count: 1,
            ends_at: at(3600),
            requirements_json: json!({"roles": [], "min_account_age_days": 0}),
            max_entries: 500,
            draw_nonce: NONCE.into(),
            created_at: at(0),
        },
    )
    .await
    .expect("seed definition without success audit");
    assert!(store::upsert_entry(pool, GUILD, TARGET, ENTRANT, at(1))
        .await
        .expect("seed private entrant"));
    sqlx::query("UPDATE giveaways SET status = $1, paused_at = CASE WHEN $1 = 'paused' THEN now() - interval '1 hour' ELSE NULL END WHERE guild_id = $2 AND id = $3")
        .bind(status).bind(GUILD).bind(TARGET).execute(pool).await.expect("seed lifecycle");
    row(pool).await
}

async fn row(pool: &PgPool) -> store::GiveawayRow {
    store::get_giveaway(pool, GUILD, TARGET)
        .await
        .expect("read definition")
        .expect("definition exists")
}

async fn mock() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        if request.path.contains("/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::json(200, json!({"id": "1550102030405060703"}))
        }
    })
    .await
}

fn runtime(pool: &PgPool, mock: &MockRest) -> InteractionRuntime {
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().expect("guild snowflake")),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    });
    InteractionRuntime::with_router(
        router,
        pool.clone(),
        ActionExecutor::with_proxy("mock-token-not-a-credential".into(), Some(mock.origin()))
            .expect("loopback transport"),
        0,
        ClassifierConfig::default(),
    )
}

fn command(
    seq: u64,
    actor: &str,
    guild: &str,
    action: &str,
    id: &str,
    minutes: i64,
) -> Interaction {
    let mut options = vec![json!({"name": "id", "type": 3, "value": id})];
    if action == "extend" {
        options.push(json!({"name": "duration-minutes", "type": 4, "value": minutes}));
    }
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": guild},
        "id": seq.to_string(), "token": "fixture-interaction-token", "type": 2,
        "version": 1, "guild_id": guild, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": PERM_MANAGE_GUILD.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": actor, "username": "staff", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1,
            "options": [{"name": action, "type": 1, "options": options}]}
    }))
    .expect("authorized staff wire interaction")
}

async fn invoke(runtime: &InteractionRuntime, interaction: &Interaction) {
    assert!(runtime.handle(interaction).await.expect("dispatch command"));
}

async fn facts(pool: &PgPool) -> Vec<(Value, String)> {
    sqlx::query_as("SELECT to_jsonb(a), a.xmin::text FROM automation_audit_log a WHERE guild_id = $1 AND target_key = $2 ORDER BY action, id")
        .bind(GUILD).bind(TARGET).fetch_all(pool).await.expect("existing discoverable audit table query")
}

async fn row_transaction(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM giveaways WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(TARGET)
        .fetch_one(pool)
        .await
        .expect("read mutation transaction")
}

fn assert_minimal(fact: &Value, action: &str, actor: &str) {
    assert_eq!(fact["guild_id"], GUILD);
    assert_eq!(
        fact["actor_id"], actor,
        "authenticated staff actor, not creator or system"
    );
    assert_eq!(fact["target_key"], TARGET);
    assert_eq!(fact["action"], format!("giveaway.{action}"));
    assert_eq!(fact["outcome"], "ok");
    assert!(
        fact["reason"].is_null(),
        "success facts need no free-form reason"
    );
    assert!(fact["id"].as_str().is_some_and(|id| !id.is_empty()));
    let rendered = fact.to_string();
    for forbidden in [
        PRIZE,
        NONCE,
        CLAIM,
        ENTRANT,
        "private-seed-must-not-appear",
        "audit_private_database_marker",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "audit must not carry private data"
        );
    }
}

fn reply_contents(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .iter()
        .filter(|request| request.path.contains("/webhooks/"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter_map(|body| body["content"].as_str().map(str::to_owned))
        .collect()
}

async fn success(action: &str) {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let before = seed(
        db.pool(),
        if action == "resume" { "paused" } else { "open" },
    )
    .await;
    let mock = mock().await;
    invoke(
        &runtime(db.pool(), &mock),
        &command(100, ACTOR, GUILD, action, TARGET, 30),
    )
    .await;
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    let mutation_tx = row_transaction(db.pool()).await;
    let replies = reply_contents(&mock);
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;

    assert!(
        replies.iter().any(|text| text.contains(match action {
            "pause" => "Giveaway paused",
            "resume" => "Giveaway resumed",
            _ => "Giveaway extended",
        })),
        "the authorized command must complete, not silently refuse"
    );
    match action {
        "pause" => {
            assert_eq!(after.status, "paused");
            assert_eq!(after.ends_at, before.ends_at);
            assert_eq!(after.paused_at, Some(after.updated_at));
        }
        "resume" => {
            assert_eq!(after.status, "open");
            assert_eq!(after.paused_at, None);
            let old_ms = before.ends_at.unix_timestamp_nanos() / 1_000_000;
            let paused_ms = before
                .paused_at
                .expect("paused marker")
                .unix_timestamp_nanos()
                / 1_000_000;
            let resumed_ms = after.updated_at.unix_timestamp_nanos() / 1_000_000;
            assert_eq!(
                after.ends_at.unix_timestamp_nanos() / 1_000_000,
                old_ms + (resumed_ms - paused_ms).max(0)
            );
        }
        _ => {
            assert_eq!(after.status, "open");
            assert_eq!((after.ends_at - before.ends_at).whole_seconds(), 1800);
        }
    }
    assert_eq!(
        audit.len(),
        1,
        "one successful transition requires one actor fact"
    );
    assert_minimal(&audit[0].0, action, ACTOR);
    assert_eq!(
        audit[0].1, mutation_tx,
        "audit and transition must commit in the SAME transaction, not adjacent autocommits"
    );
}

#[tokio::test]
async fn pause_commits_authenticated_actor_fact_atomically() {
    success("pause").await;
}
#[tokio::test]
async fn resume_commits_authenticated_actor_fact_atomically() {
    success("resume").await;
}
#[tokio::test]
async fn extend_commits_authenticated_actor_fact_atomically() {
    success("extend").await;
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn rollback(action: &str) {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let before = seed(
        db.pool(),
        if action == "resume" { "paused" } else { "open" },
    )
    .await;
    sqlx::query("ALTER TABLE automation_audit_log ADD CONSTRAINT audit_private_database_marker CHECK (action NOT IN ('giveaway.pause', 'giveaway.resume', 'giveaway.extend')) NOT VALID")
        .execute(db.pool()).await.expect("inject audit-only insertion failure in disposable fixture");
    let mock = mock().await;
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    async {
        tracing::warn!("audit-capture-control");
        invoke(
            &runtime(db.pool(), &mock),
            &command(101, ACTOR, GUILD, action, TARGET, 30),
        )
        .await;
    }
    .with_subscriber(subscriber)
    .await;
    let logged =
        String::from_utf8(logs.lock().expect("captured logs").clone()).expect("UTF-8 logs");
    assert!(
        logged.contains("audit-capture-control"),
        "logging absence is meaningful only with an active capture"
    );
    for forbidden in [
        PRIZE,
        NONCE,
        CLAIM,
        ENTRANT,
        "audit_private_database_marker",
        "violates check constraint",
    ] {
        assert!(
            !logged.contains(forbidden),
            "command logs must not expose private payloads or raw database errors"
        );
    }
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    let replies = reply_contents(&mock);
    let refreshed = mock
        .requests()
        .iter()
        .any(|r| r.path.contains("/channels/"));
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(
        after, before,
        "audit insertion failure must roll back ALL lifecycle/deadline columns"
    );
    assert!(audit.is_empty());
    assert_eq!(
        replies,
        vec![FAILURE],
        "return the existing bounded unavailable response, never raw SQL errors"
    );
    assert!(!refreshed, "do not publish a transition whose audit failed");
}

#[tokio::test]
async fn pause_audit_failure_rolls_back_and_returns_safe_unavailable() {
    rollback("pause").await;
}
#[tokio::test]
async fn resume_audit_failure_rolls_back_and_returns_safe_unavailable() {
    rollback("resume").await;
}
#[tokio::test]
async fn extend_audit_failure_rolls_back_and_returns_safe_unavailable() {
    rollback("extend").await;
}

#[tokio::test]
async fn control_refusals_leave_definition_and_success_audit_unchanged() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let mock = mock().await;
    let runtime = runtime(db.pool(), &mock);
    let mut seq = 200;
    for status in ["open", "paused", "drawing", "drawn", "cancelled"] {
        seed(db.pool(), status).await;
        let before = row(db.pool()).await;
        for action in ["pause", "resume", "extend"] {
            if (action == "pause" && status == "open")
                || (action == "resume" && status == "paused")
                || (action == "extend" && ["open", "paused"].contains(&status))
            {
                continue;
            }
            seq += 1;
            invoke(&runtime, &command(seq, ACTOR, GUILD, action, TARGET, 30)).await;
            assert_eq!(
                row(db.pool()).await,
                before,
                "wrong-state {action}/{status} must be inert"
            );
        }
        store::delete_giveaway(db.pool(), GUILD, TARGET)
            .await
            .expect("remove owned fixture between states");
    }
    seed(db.pool(), "open").await;
    let before = row(db.pool()).await;
    sqlx::query("INSERT INTO giveaways (guild_id, id, prize, channel_id, message_id, created_by, winners_count, ends_at, status, requirements_json, max_entries, draw_nonce, created_at, updated_at) SELECT $1, 'gw_foreign', prize, channel_id, message_id, created_by, winners_count, ends_at, status, requirements_json, max_entries, draw_nonce, created_at, updated_at FROM giveaways WHERE guild_id = $2 AND id = $3")
        .bind(FOREIGN).bind(GUILD).bind(TARGET).execute(db.pool()).await.expect("foreign target fixture");
    let foreign_before = store::get_giveaway(db.pool(), FOREIGN, "gw_foreign")
        .await
        .expect("foreign read");
    for action in ["pause", "resume", "extend"] {
        seq += 1;
        invoke(
            &runtime,
            &command(seq, ACTOR, GUILD, action, "gw_missing", 30),
        )
        .await;
        seq += 1;
        invoke(
            &runtime,
            &command(seq, ACTOR, GUILD, action, "gw_foreign", 30),
        )
        .await;
    }
    assert_eq!(
        store::get_giveaway(db.pool(), FOREIGN, "gw_foreign")
            .await
            .expect("foreign read"),
        foreign_before
    );
    let all_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("no foreign success facts");
    assert_eq!(all_audits, 0);
    for invalid in [0, -1, 43_201, i64::MAX] {
        seq += 1;
        invoke(
            &runtime,
            &command(seq, ACTOR, GUILD, "extend", TARGET, invalid),
        )
        .await;
    }
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after, before);
    assert!(
        audit.is_empty(),
        "unknown/foreign/wrong-state/invalid requests are not successes"
    );
}

#[tokio::test]
async fn repeat_extensions_and_second_pause_cycle_are_distinct_mutations() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let initial = seed(db.pool(), "open").await;
    let mock = mock().await;
    let runtime = runtime(db.pool(), &mock);
    for (seq, actor, action) in [
        (300, ACTOR, "extend"),
        (301, OTHER, "extend"),
        (302, ACTOR, "pause"),
        (303, OTHER, "resume"),
        (304, OTHER, "pause"),
    ] {
        invoke(&runtime, &command(seq, actor, GUILD, action, TARGET, 30)).await;
    }
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    let draws = store::get_draws(db.pool(), GUILD, TARGET)
        .await
        .expect("draw rows");
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after.status, "paused");
    assert!((after.ends_at - initial.ends_at).whole_seconds() >= 3600);
    assert!(
        draws.is_empty(),
        "lifecycle changes do not create draw rounds"
    );
    assert_eq!(
        audit.len(),
        5,
        "legitimate same-target writes must not collide on one action/target id"
    );
    for (action, actor, count) in [
        ("extend", ACTOR, 1),
        ("extend", OTHER, 1),
        ("pause", ACTOR, 1),
        ("pause", OTHER, 1),
        ("resume", OTHER, 1),
    ] {
        let matching: Vec<_> = audit
            .iter()
            .filter(|(f, _)| f["action"] == format!("giveaway.{action}") && f["actor_id"] == actor)
            .collect();
        assert_eq!(matching.len(), count);
        assert_minimal(&matching[0].0, action, actor);
    }
}

#[tokio::test]
async fn concurrent_pause_and_resume_have_one_winner_fact_each() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "open").await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent worker pool");
    let mock = mock().await;
    let left = runtime(db.pool(), &mock);
    let right = runtime(&peer, &mock);
    let p1 = command(400, ACTOR, GUILD, "pause", TARGET, 30);
    let p2 = command(401, OTHER, GUILD, "pause", TARGET, 30);
    tokio::join!(invoke(&left, &p1), invoke(&right, &p2));
    let paused = row(db.pool()).await;
    let pause_facts = facts(db.pool()).await;
    let r1 = command(402, ACTOR, GUILD, "resume", TARGET, 30);
    let r2 = command(403, OTHER, GUILD, "resume", TARGET, 30);
    tokio::join!(invoke(&left, &r1), invoke(&right, &r2));
    let resumed = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    peer.close().await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(paused.status, "paused");
    assert_eq!(
        pause_facts.len(),
        1,
        "the refused second pause emits no success fact"
    );
    assert_eq!(resumed.status, "open");
    assert_eq!(
        audit.len(),
        2,
        "the refused second resume emits no success fact"
    );
    for action in ["pause", "resume"] {
        let matching: Vec<_> = audit
            .iter()
            .filter(|(f, _)| f["action"] == format!("giveaway.{action}"))
            .collect();
        assert_eq!(matching.len(), 1);
        let actor = matching[0].0["actor_id"].as_str().expect("human actor");
        assert!([ACTOR, OTHER].contains(&actor));
        assert_minimal(&matching[0].0, action, actor);
    }
}

#[tokio::test]
async fn concurrent_extensions_preserve_both_deadline_additions_and_actor_facts() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let before = seed(db.pool(), "paused").await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent worker pool");
    let mock = mock().await;
    let left = runtime(db.pool(), &mock);
    let right = runtime(&peer, &mock);
    let a = command(410, ACTOR, GUILD, "extend", TARGET, 20);
    let b = command(411, OTHER, GUILD, "extend", TARGET, 30);
    tokio::join!(invoke(&left, &a), invoke(&right, &b));
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    peer.close().await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!((after.ends_at - before.ends_at).whole_seconds(), 3000);
    assert_eq!(after.status, "paused");
    assert_eq!(after.paused_at, before.paused_at);
    assert_eq!(
        audit.len(),
        2,
        "both legitimate extensions need separate facts"
    );
    assert_ne!(audit[0].0["id"], audit[1].0["id"]);
    for actor in [ACTOR, OTHER] {
        let found = audit
            .iter()
            .find(|(f, _)| f["actor_id"] == actor)
            .expect("each authenticated actor attributed");
        assert_minimal(&found.0, "extend", actor);
    }
}

#[tokio::test]
async fn control_live_draw_claim_refuses_pause_without_a_success_fact() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "open").await;
    store::claim_specific(db.pool(), GUILD, TARGET, at(10), at(0), CLAIM)
        .await
        .expect("claim")
        .expect("live claim");
    let before = row(db.pool()).await;
    let mock = mock().await;
    invoke(
        &runtime(db.pool(), &mock),
        &command(420, ACTOR, GUILD, "pause", TARGET, 30),
    )
    .await;
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(
        after, before,
        "pause must preserve the unclaimed-open predicate"
    );
    assert!(audit.is_empty());
}

#[tokio::test]
async fn pause_contends_with_draw_claim_without_crossing_the_claim_fence() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "open").await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent draw worker");
    let mock = mock().await;
    let runtime = runtime(db.pool(), &mock);
    let pause = command(430, ACTOR, GUILD, "pause", TARGET, 30);
    // Due-scan and command do not share the runtime's process-local effects mutex.
    let (_, claim) = tokio::join!(
        invoke(&runtime, &pause),
        store::claim_due_giveaway(&peer, GUILD, at(4000), at(0), CLAIM)
    );
    let claimed = claim.expect("draw claim query").is_some();
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    peer.close().await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    if claimed {
        assert_eq!(after.status, "open");
        assert_eq!(after.claim_token.as_deref(), Some(CLAIM));
        assert!(
            audit.is_empty(),
            "a refused pause is not an audited success"
        );
    } else {
        assert_eq!(after.status, "paused");
        assert!(after.claim_token.is_none());
        assert_eq!(audit.len(), 1);
        assert_minimal(&audit[0].0, "pause", ACTOR);
    }
}

// Runtime commands share an effects mutex. These witnesses deliberately hold a
// database lock in an independent actor, so process-local serialization cannot
// hide a read-before-lock/lost-update bug in the store.
async fn locked_transition(action: &str) {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let before = seed(
        db.pool(),
        if action == "resume" { "paused" } else { "open" },
    )
    .await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent command worker");
    let mock = mock().await;
    let runtime = runtime(&peer, &mock);
    let interaction = command(450, ACTOR, GUILD, action, TARGET, 30);
    let mut holder = db.pool().begin().await.expect("holder transaction");
    sqlx::query("SELECT id FROM giveaways WHERE guild_id = $1 AND id = $2 FOR UPDATE")
        .bind(GUILD)
        .bind(TARGET)
        .fetch_one(&mut *holder)
        .await
        .expect("hold target row");
    let driver = async {
        let observed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid() AND wait_event_type = 'Lock' AND query ILIKE '%giveaways%')")
                    .fetch_one(db.pool()).await.expect("observe actual row-lock contention");
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.is_ok();
        match action {
            "pause" => {
                sqlx::query("UPDATE giveaways SET claim_token = $1, claimed_at = $2 WHERE guild_id = $3 AND id = $4")
                    .bind(CLAIM).bind(at(10)).bind(GUILD).bind(TARGET).execute(&mut *holder).await.expect("claim wins held-row race");
            }
            "resume" => {
                sqlx::query("UPDATE giveaways SET paused_at = paused_at - interval '10 minutes' WHERE guild_id = $1 AND id = $2")
                    .bind(GUILD).bind(TARGET).execute(&mut *holder).await.expect("change paused span while holding row");
            }
            _ => {
                sqlx::query("UPDATE giveaways SET ends_at = ends_at + interval '10 minutes' WHERE guild_id = $1 AND id = $2")
                    .bind(GUILD).bind(TARGET).execute(&mut *holder).await.expect("earlier serialized extension");
            }
        }
        holder
            .commit()
            .await
            .expect("release holder with intervening mutation");
        observed
    };
    let (_, observed) = tokio::join!(invoke(&runtime, &interaction), driver);
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    let mutation_tx = row_transaction(db.pool()).await;
    peer.close().await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert!(
        observed,
        "must witness a real database lock wait, not infer concurrency from task start order"
    );
    if action == "pause" {
        assert_eq!(after.status, "open");
        assert_eq!(after.claim_token.as_deref(), Some(CLAIM));
        assert!(
            audit.is_empty(),
            "pause must recheck the claim after the row lock releases"
        );
    } else {
        if action == "extend" {
            assert_eq!(
                (after.ends_at - before.ends_at).whole_seconds(),
                2400,
                "read the locked row's deadline, not a stale snapshot"
            );
        } else {
            let old_ms = before.ends_at.unix_timestamp_nanos() / 1_000_000;
            let paused_ms = before
                .paused_at
                .expect("paused marker")
                .unix_timestamp_nanos()
                / 1_000_000
                - 600_000;
            let resumed_ms = after.updated_at.unix_timestamp_nanos() / 1_000_000;
            assert_eq!(
                after.ends_at.unix_timestamp_nanos() / 1_000_000,
                old_ms + (resumed_ms - paused_ms).max(0),
                "resume must use the locked paused span"
            );
        }
        assert_eq!(audit.len(), 1);
        assert_minimal(&audit[0].0, action, ACTOR);
        assert_eq!(audit[0].1, mutation_tx);
    }
}

#[tokio::test]
async fn control_pause_rechecks_claim_after_a_real_row_lock_wait() {
    locked_transition("pause").await;
}
#[tokio::test]
async fn resume_uses_locked_paused_span_and_commits_its_fact() {
    locked_transition("resume").await;
}
#[tokio::test]
async fn extend_uses_locked_deadline_and_commits_its_fact() {
    locked_transition("extend").await;
}

#[tokio::test]
async fn control_unrepresentable_deadlines_create_neither_transition_nor_fact() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "paused").await;
    sqlx::query("UPDATE giveaways SET ends_at = '9999-12-31T23:59:59+00:00'::timestamptz WHERE guild_id = $1 AND id = $2")
        .bind(GUILD).bind(TARGET).execute(db.pool()).await.expect("representable edge fixture");
    let before = row(db.pool()).await;
    let mock = mock().await;
    let runtime = runtime(db.pool(), &mock);
    invoke(&runtime, &command(460, ACTOR, GUILD, "resume", TARGET, 30)).await;
    invoke(&runtime, &command(461, ACTOR, GUILD, "extend", TARGET, 30)).await;
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after, before);
    assert!(audit.is_empty());
}

#[tokio::test]
async fn resume_with_backwards_clock_still_audits_the_real_state_transition() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "paused").await;
    sqlx::query("UPDATE giveaways SET paused_at = now() + interval '1 hour' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD).bind(TARGET).execute(db.pool()).await.expect("future pause marker");
    let before = row(db.pool()).await;
    let mock = mock().await;
    invoke(
        &runtime(db.pool(), &mock),
        &command(470, ACTOR, GUILD, "resume", TARGET, 30),
    )
    .await;
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after.status, "open");
    assert_eq!(after.paused_at, None);
    assert_eq!(
        after.ends_at, before.ends_at,
        "backwards time must not shorten the deadline"
    );
    assert_eq!(
        audit.len(),
        1,
        "unchanged deadline is NOT a no-op when paused becomes open"
    );
    assert_minimal(&audit[0].0, "resume", ACTOR);
}

#[tokio::test]
async fn lifecycle_writes_contend_with_cancel_without_reopening_a_closed_target() {
    let _serial = SERIAL.lock().await;
    for action in ["pause", "resume", "extend"] {
        let Some(db) = fixture().await else { return };
        seed(
            db.pool(),
            if action == "resume" { "paused" } else { "open" },
        )
        .await;
        let peer = db
            .independent_pool()
            .await
            .expect("independent cancellation worker");
        let mock = mock().await;
        let runtime = runtime(db.pool(), &mock);
        let interaction = command(480, ACTOR, GUILD, action, TARGET, 30);
        let (_, cancelled) = tokio::join!(
            invoke(&runtime, &interaction),
            store::cancel_audited(&peer, GUILD, TARGET, OTHER)
        );
        let cancelled = cancelled.expect("cancel operation");
        let after = row(db.pool()).await;
        let audit = facts(db.pool()).await;
        let success_word = match action {
            "pause" => "Giveaway paused",
            "resume" => "Giveaway resumed",
            _ => "Giveaway extended",
        };
        let succeeded = reply_contents(&mock)
            .iter()
            .any(|reply| reply.contains(success_word));
        peer.close().await;
        db.close().await.expect("drop disposable database");
        mock.shutdown().await;
        assert!(cancelled.is_some());
        assert_eq!(
            after.status, "cancelled",
            "the lifecycle operation must not reopen a concurrently cancelled row"
        );
        assert!(after.claim_token.is_none());
        let cancels: Vec<_> = audit
            .iter()
            .filter(|(f, _)| f["action"] == "giveaway.cancel")
            .collect();
        assert_eq!(cancels.len(), 1);
        assert_minimal(&cancels[0].0, "cancel", OTHER);
        let transitions: Vec<_> = audit
            .iter()
            .filter(|(f, _)| f["action"] == format!("giveaway.{action}"))
            .collect();
        assert_eq!(
            transitions.len(),
            usize::from(succeeded),
            "exactly the lifecycle command that actually succeeded needs a fact"
        );
        assert_eq!(audit.len(), cancels.len() + transitions.len());
        if let Some(fact) = transitions.first() {
            assert_minimal(&fact.0, action, ACTOR);
        }
    }
}

#[tokio::test]
async fn control_unauthorized_actor_cannot_mutate_or_create_a_success_fact() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    let before = seed(db.pool(), "open").await;
    let mock = mock().await;
    let runtime = runtime(db.pool(), &mock);
    for (seq, action) in [(490, "pause"), (491, "resume"), (492, "extend")] {
        let interaction = command(seq, ACTOR, GUILD, action, TARGET, 30);
        let mut wire = serde_json::to_value(&interaction).expect("wire value");
        wire["member"]["permissions"] = json!("0");
        let denied = serde_json::from_value(wire).expect("unauthorized interaction");
        invoke(&runtime, &denied).await;
    }
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    let refreshed = mock
        .requests()
        .iter()
        .any(|request| request.path.contains("/channels/"));
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after, before);
    assert!(audit.is_empty());
    assert!(!refreshed);
}

#[tokio::test]
async fn control_cancel_draw_and_reroll_facts_keep_existing_identity() {
    let _serial = SERIAL.lock().await;
    let Some(db) = fixture().await else { return };
    seed(db.pool(), "open").await;
    let mock = mock().await;
    invoke(
        &runtime(db.pool(), &mock),
        &command(440, ACTOR, GUILD, "cancel", TARGET, 30),
    )
    .await;
    store::audit_draw(db.pool(), GUILD, TARGET, 0, None, at(5))
        .await
        .expect("existing draw audit");
    store::audit_draw(db.pool(), GUILD, TARGET, 0, None, at(6))
        .await
        .expect("existing draw replay no-op");
    store::audit_reroll(db.pool(), GUILD, TARGET, 1, Some(OTHER), at(7))
        .await
        .expect("existing reroll audit");
    let after = row(db.pool()).await;
    let audit = facts(db.pool()).await;
    db.close().await.expect("drop disposable database");
    mock.shutdown().await;
    assert_eq!(after.status, "cancelled");
    assert_eq!(audit.len(), 3);
    assert_minimal(&audit[0].0, "cancel", ACTOR);
    assert_eq!(
        audit[1].0["id"],
        format!("giveaway.draw:{GUILD}:{TARGET}:0")
    );
    assert!(audit[1].0["actor_id"].is_null());
    assert_eq!(
        audit[2].0["id"],
        format!("giveaway.reroll:{GUILD}:{TARGET}:1")
    );
    assert_minimal(&audit[2].0, "reroll", OTHER);
}
