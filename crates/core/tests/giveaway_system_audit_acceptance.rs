#![cfg(feature = "db")]

//! Companion regressions for the legacy system-actor lifecycle APIs.
//! Each success and rollback case must catch replacing its audited wrapper with
//! the old unaudited body. Run with the documented disposable database bootstrap:
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db \
//! --test giveaway_system_audit_acceptance -- --test-threads=1

use serde_json::{json, Value};
use sqlx::{types::time::OffsetDateTime, PgPool};
use two_bot_core::giveaway_store as store;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060801";
const TARGET: &str = "gw_system_audit";
const CREATOR: &str = "1550102030405060810";
const PRIZE: &str = "system-audit-private-prize";
const NONCE: &str = "system-audit-private-nonce";

enum LegacyAction {
    Pause,
    Resume,
    Extend,
}

impl LegacyAction {
    fn name(&self) -> &'static str {
        match self {
            Self::Pause => "giveaway.pause",
            Self::Resume => "giveaway.resume",
            Self::Extend => "giveaway.extend",
        }
    }

    fn deadline(&self) -> OffsetDateTime {
        match self {
            Self::Pause => at(3600),
            Self::Resume => at(5400),
            Self::Extend => at(4200),
        }
    }

    async fn invoke(&self, pool: &PgPool) -> Result<bool, sqlx::Error> {
        // Call the legacy entry points directly, never their actor-aware peers.
        match self {
            Self::Pause => store::pause_giveaway(pool, GUILD, TARGET, at(100)).await,
            Self::Resume => store::resume_giveaway(pool, GUILD, TARGET, at(1900))
                .await
                .map(|deadline| deadline == Some(at(5400))),
            Self::Extend => store::extend_giveaway(pool, GUILD, TARGET, 600, at(1900))
                .await
                .map(|deadline| deadline == Some(at(4200))),
        }
    }
}

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_760_000_000 + seconds).expect("fixed fixture stamp")
}

async fn fixture(action: &LegacyAction) -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable database");
    store::create_giveaway(
        db.pool(),
        &store::NewGiveaway {
            guild_id: GUILD.into(),
            id: TARGET.into(),
            prize: PRIZE.into(),
            channel_id: "1550102030405060802".into(),
            message_id: "1550102030405060803".into(),
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
    .expect("seed one open giveaway");
    if matches!(action, LegacyAction::Resume) {
        // Seed the precondition without invoking another lifecycle wrapper, so
        // the system-resume oracle does not depend on system-pause auditing.
        sqlx::query("UPDATE giveaways SET status = 'paused', paused_at = $1 WHERE guild_id = $2 AND id = $3")
            .bind(at(100)).bind(GUILD).bind(TARGET).execute(db.pool()).await.expect("seed paused precondition");
    }
    db
}

async fn row(pool: &PgPool) -> store::GiveawayRow {
    store::get_giveaway(pool, GUILD, TARGET)
        .await
        .expect("read lifecycle row")
        .expect("fixture exists")
}

async fn row_transaction(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM giveaways WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(TARGET)
        .fetch_one(pool)
        .await
        .expect("read lifecycle transaction")
}

async fn facts(pool: &PgPool) -> Vec<(Value, String)> {
    sqlx::query_as("SELECT to_jsonb(a), a.xmin::text FROM automation_audit_log a WHERE guild_id = $1 AND target_key = $2 ORDER BY id")
        .bind(GUILD).bind(TARGET).fetch_all(pool).await.expect("read scoped audit facts")
}

fn assert_system_fact(fact: &Value, action: &LegacyAction) {
    let mut fields: Vec<_> = fact
        .as_object()
        .expect("audit object")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "action",
            "actor_id",
            "created_at",
            "guild_id",
            "id",
            "outcome",
            "reason",
            "target_key"
        ]
    );
    assert_eq!(fact["guild_id"], GUILD);
    assert_eq!(fact["target_key"], TARGET);
    assert_eq!(fact["action"], action.name());
    assert_eq!(
        fact["actor_id"],
        Value::Null,
        "legacy callers are system actors, never the creator"
    );
    assert_eq!(fact["outcome"], "ok");
    assert_eq!(fact["reason"], Value::Null, "no free-form success payload");
    assert!(fact["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(fact["created_at"]
        .as_str()
        .is_some_and(|stamp| !stamp.is_empty()));
    for forbidden in [CREATOR, PRIZE, NONCE] {
        assert!(
            !fact.to_string().contains(forbidden),
            "ids/outcomes only, not definition contents"
        );
    }
}

async fn success(action: LegacyAction) {
    let db = fixture(&action).await;
    let before_tx = row_transaction(db.pool()).await;
    let before_facts = facts(db.pool()).await;
    let result = action.invoke(db.pool()).await;
    let after = row(db.pool()).await;
    let after_tx = row_transaction(db.pool()).await;
    let after_facts = facts(db.pool()).await;
    db.close().await.expect("drop owned disposable database");

    assert!(result.expect("legacy lifecycle succeeds with expected return value"));
    match action {
        LegacyAction::Pause => {
            assert_eq!(after.status, "paused");
            assert_eq!(after.paused_at, Some(at(100)));
            assert_eq!(after.updated_at, at(100));
        }
        LegacyAction::Resume | LegacyAction::Extend => {
            assert_eq!(after.status, "open");
            assert_eq!(after.paused_at, None);
            assert_eq!(after.updated_at, at(1900));
        }
    }
    assert_eq!(after.ends_at, action.deadline());
    assert_ne!(after_tx, before_tx, "a real lifecycle write must commit");
    // Isolate pre-existing create/setup facts instead of assuming an empty log.
    let added: Vec<_> = after_facts
        .iter()
        .filter(|(fact, _)| {
            !before_facts
                .iter()
                .any(|(before, _)| before["id"] == fact["id"])
        })
        .collect();
    assert_eq!(
        added.len(),
        1,
        "each legacy success adds exactly one minimal fact"
    );
    assert_system_fact(&added[0].0, &action);
    assert_eq!(
        added[0].1, after_tx,
        "legacy mutation and audit must share a transaction, not adjacent autocommits"
    );
}

async fn rollback(action: LegacyAction) {
    let db = fixture(&action).await;
    let before = row(db.pool()).await;
    let before_tx = row_transaction(db.pool()).await;
    let before_facts = facts(db.pool()).await;
    // Disposable-fixture fault injection only; no migration or grant changes.
    sqlx::query("ALTER TABLE automation_audit_log ADD CONSTRAINT reject_system_lifecycle_fact CHECK (action NOT IN ('giveaway.pause', 'giveaway.resume', 'giveaway.extend')) NOT VALID")
        .execute(db.pool()).await.expect("reject only lifecycle audit inserts");
    let result = action.invoke(db.pool()).await;
    let after = row(db.pool()).await;
    let after_tx = row_transaction(db.pool()).await;
    let after_facts = facts(db.pool()).await;
    db.close().await.expect("drop owned disposable database");

    let error = result.expect_err("legacy success cannot bypass a rejected audit insert");
    assert_eq!(
        error
            .as_database_error()
            .expect("audit check failure")
            .code()
            .as_deref(),
        Some("23514")
    );
    assert_eq!(
        after, before,
        "audit failure rolls back every state/deadline column"
    );
    assert_eq!(
        after_tx, before_tx,
        "a failed transaction must not replace the committed row"
    );
    assert_eq!(
        after_facts, before_facts,
        "no success fact from an aborted lifecycle write"
    );
}

#[tokio::test]
async fn legacy_pause_commits_one_null_actor_fact_in_the_mutation_transaction() {
    success(LegacyAction::Pause).await;
}

#[tokio::test]
async fn legacy_resume_commits_one_null_actor_fact_in_the_mutation_transaction() {
    success(LegacyAction::Resume).await;
}

#[tokio::test]
async fn legacy_extend_commits_one_null_actor_fact_in_the_mutation_transaction() {
    success(LegacyAction::Extend).await;
}

#[tokio::test]
async fn legacy_pause_audit_failure_rolls_back_the_state_change() {
    rollback(LegacyAction::Pause).await;
}

#[tokio::test]
async fn legacy_resume_audit_failure_rolls_back_the_deadline_and_pause_marker() {
    rollback(LegacyAction::Resume).await;
}

#[tokio::test]
async fn legacy_extend_audit_failure_rolls_back_the_deadline() {
    rollback(LegacyAction::Extend).await;
}
