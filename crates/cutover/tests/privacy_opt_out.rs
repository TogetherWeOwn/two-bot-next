//! Privacy-control acceptance against a migrated disposable database.
//!
//! Failing-first contract for `/privacy opt-out` + per-guild analytics toggles.
//! Every test below MUST FAIL on the current head because the storage contract
//! does not exist yet: `privacy_opt_outs`, `guild_analytics_settings` and
//! `privacy_opt_out_audit` have no migration, and `record_event` performs no
//! suppression or toggle check. The Builder implements on this branch and must
//! not edit this file.
//!
//! Failing command (controller):
//! ```sh
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_ci \
//!   python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test privacy_opt_out
//! ```
//! Expected pre-implementation failure: each test errors with
//! `relation "privacy_opt_outs" (or "guild_analytics_settings" /
//! "privacy_opt_out_audit") does not exist`, proving the contract is absent.
//! `record_event` additionally persists suppressed writes today, so count
//! assertions would fail even with the tables present but the gating unwired.
//!
//! What these tests pin (parent acceptance criteria):
//! 1. Opt-out purges the member's event-store rows AND installs a guild-scoped
//!    suppression entry; later per-user writes for that pair are dropped.
//! 2. Aggregate counters keep counting (other members' rows still persist and
//!    guild totals still increment).
//! 3. Guild scoping: other guilds and other members are byte-equivalent
//!    survivors; suppression is never global.
//! 4. Per-guild analytics toggles disable presence / activity / voice ingestion
//!    per guild, independently per category.
//! 5. Exactly one audit row per opt-out/purge, written atomically with the
//!    purge + suppression (rollback on audit failure).
//! 6. Re-enable (opt-back-in) restores persistence; defaults (no rows) match
//!    current behavior.
//!
//! Coverage limits (stated, not silent): these SQL-store tests simulate the
//! opt-out service with explicit SQL (`INSERT` suppression, `DELETE` purge,
//! `INSERT` audit) because no `/privacy` command, service function or migration
//! exists yet. They do not invoke gateway presence/voice dispatch
//! (`crates/bot/src/gateway.rs`), `command_runtime.rs` (no `/privacy` command),
//! `PgFunnelStore` / `onboarding_store` / invite snapshots / `record_earliest` /
//! `touch_activity` / `mark_bot` producer paths, or `backfill.rs` replay
//! producers. Until every producer checks suppression/toggles, erased data may
//! be recreated by an uncovered path. Gateway-level dispatch fixtures must prove
//! disabled categories are not ingested at runtime.

use two_bot_core::database_tls::TlsPolicy;
use two_bot_cutover::db::{record_event, CutoverDb, FunnelWrite};
use two_bot_testsupport::TestDatabase;

const GUILD_A: &str = "1545644954272137297";
const GUILD_B: &str = "1545644954272137298";
const MEMBER_A: &str = "123456789012345678";
const MEMBER_B: &str = "123456789012345679";

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the privacy opt-out database suite"
            );
            eprintln!("SKIP privacy opt-out integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    // The fixture owns its database; open the public CutoverDb handle against
    // that same database (already migrated, so skip migrations). Derive the URL
    // from the bootstrap binding so CI and controller hosts both resolve.
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

fn event(guild: &str, member: &str, event_type: &str, at: &str) -> FunnelWrite {
    FunnelWrite {
        member_id: Some(member.to_owned()),
        guild_id: guild.to_owned(),
        event_type: event_type.to_owned(),
        occurred_at: at.to_owned(),
        source: "privacy-fixture".to_owned(),
        metadata: Some("{}".to_owned()),
    }
}

async fn event_count(db: &CutoverDb, guild: &str, member: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
        .bind(guild)
        .bind(member)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn guild_event_count(db: &CutoverDb, guild: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1")
        .bind(guild)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn member_row_count(db: &CutoverDb, guild: &str, member: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1 AND member_id = $2")
        .bind(guild)
        .bind(member)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn opt_out(db: &CutoverDb, guild: &str, member: &str) {
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(guild)
        .bind(member)
        .execute(db.pool())
        .await
        .expect("guild-scoped opt-out table must exist");
}

/// A guild/member suppression key must stop future event-store writes only for
/// that pair, including the members projection. No suppression means today's
/// event persistence stays enabled.
#[tokio::test]
async fn suppressed_member_event_write_is_dropped_only_for_the_matching_guild() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    opt_out(&db, GUILD_A, MEMBER_A).await;

    for (guild, member) in [
        (GUILD_A, MEMBER_A),
        (GUILD_A, MEMBER_B),
        (GUILD_B, MEMBER_A),
        (GUILD_B, MEMBER_B),
    ] {
        let (inserted, _) = record_event(
            &db,
            &event(guild, member, "first_message", "2026-10-12T00:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(
            inserted,
            !(guild == GUILD_A && member == MEMBER_A),
            "suppression must drop only the matching pair"
        );
    }

    assert_eq!(event_count(&db, GUILD_A, MEMBER_A).await, 0);
    assert_eq!(event_count(&db, GUILD_A, MEMBER_B).await, 1);
    assert_eq!(event_count(&db, GUILD_B, MEMBER_A).await, 1);
    assert_eq!(event_count(&db, GUILD_B, MEMBER_B).await, 1);
    // The members projection is per-user persistence too: a dropped write must
    // not create it for the suppressed pair.
    assert_eq!(member_row_count(&db, GUILD_A, MEMBER_A).await, 0);
    assert_eq!(member_row_count(&db, GUILD_A, MEMBER_B).await, 1);
    assert_eq!(member_row_count(&db, GUILD_B, MEMBER_A).await, 1);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Opt-out purges the member's pre-existing event-store rows in the opting
/// guild only; the other member and the other guild survive byte-equivalent.
#[tokio::test]
async fn opt_out_purges_preexisting_member_rows_and_leaves_others() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    for (i, (guild, member)) in [
        (GUILD_A, MEMBER_A),
        (GUILD_A, MEMBER_B),
        (GUILD_B, MEMBER_A),
        (GUILD_B, MEMBER_B),
    ]
    .iter()
    .enumerate()
    {
        record_event(
            &db,
            &event(
                guild,
                member,
                "first_message",
                &format!("2026-10-1{i}T00:00:00Z"),
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(guild_event_count(&db, GUILD_A).await, 2);

    // Service simulation: install suppression, purge events + members
    // projection, write the audit receipt (see audit tests for atomicity).
    opt_out(&db, GUILD_A, MEMBER_A).await;
    let purged: u64 = sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
        .bind(GUILD_A)
        .bind(MEMBER_A)
        .execute(db.pool())
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(purged, 1);
    sqlx::query("DELETE FROM members WHERE guild_id = $1 AND member_id = $2")
        .bind(GUILD_A)
        .bind(MEMBER_A)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO privacy_opt_out_audit (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUILD_A)
        .bind(MEMBER_A)
        .execute(db.pool())
        .await
        .expect("one audit row per opt-out/purge");

    assert_eq!(event_count(&db, GUILD_A, MEMBER_A).await, 0);
    assert_eq!(member_row_count(&db, GUILD_A, MEMBER_A).await, 0);
    assert_eq!(event_count(&db, GUILD_A, MEMBER_B).await, 1);
    assert_eq!(event_count(&db, GUILD_B, MEMBER_A).await, 1);
    assert_eq!(event_count(&db, GUILD_B, MEMBER_B).await, 1);
    assert_eq!(guild_event_count(&db, GUILD_A).await, 1);
    assert_eq!(guild_event_count(&db, GUILD_B).await, 2);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Suppression must not stop aggregate counting: other members' rows still
/// persist and guild totals still increment after the opt-out.
#[tokio::test]
async fn member_suppression_does_not_stop_guild_aggregate_counts() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    opt_out(&db, GUILD_A, MEMBER_A).await;

    for (guild, member) in [
        (GUILD_A, MEMBER_A),
        (GUILD_A, MEMBER_B),
        (GUILD_B, MEMBER_A),
        (GUILD_B, MEMBER_B),
    ] {
        record_event(
            &db,
            &event(guild, member, "first_message", "2026-10-12T00:00:00Z"),
        )
        .await
        .unwrap();
    }

    // The suppressed pair contributes nothing, but every other write lands, so
    // aggregates computed over the store keep counting.
    assert_eq!(guild_event_count(&db, GUILD_A).await, 1);
    assert_eq!(guild_event_count(&db, GUILD_B).await, 2);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(total, 3);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Preferences are independently stored per guild and per analytics category.
/// This pins the data contract only; gateway presence, activity and voice
/// dispatch must separately prove that disabled categories are not ingested.
#[tokio::test]
async fn analytics_preferences_are_independent_per_guild() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    sqlx::query(
        "INSERT INTO guild_analytics_settings (guild_id, presence_enabled, activity_enabled, voice_enabled) \
         VALUES ($1, false, true, false)",
    )
    .bind(GUILD_A)
    .execute(db.pool())
    .await
    .expect("per-guild analytics settings table must exist");
    sqlx::query(
        "INSERT INTO guild_analytics_settings (guild_id, presence_enabled, activity_enabled, voice_enabled) \
         VALUES ($1, true, false, true)",
    )
    .bind(GUILD_B)
    .execute(db.pool())
    .await
    .unwrap();

    let settings: (bool, bool, bool) = sqlx::query_as(
        "SELECT presence_enabled, activity_enabled, voice_enabled FROM guild_analytics_settings WHERE guild_id = $1",
    )
    .bind(GUILD_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(settings, (false, true, false));
    let other_guild: (bool, bool, bool) = sqlx::query_as(
        "SELECT presence_enabled, activity_enabled, voice_enabled FROM guild_analytics_settings WHERE guild_id = $1",
    )
    .bind(GUILD_B)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(other_guild, (true, false, true));

    db.close().await;
    test_db.close().await.unwrap();
}

/// A disabled analytics category must drop ingestion for that guild while an
/// enabled category in the same guild still persists. Guild B (all enabled)
/// proves the toggle is guild-scoped.
#[tokio::test]
async fn disabled_analytics_category_drops_ingestion_while_enabled_category_persists() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    sqlx::query(
        "INSERT INTO guild_analytics_settings (guild_id, presence_enabled, activity_enabled, voice_enabled) \
         VALUES ($1, false, true, false)",
    )
    .bind(GUILD_A)
    .execute(db.pool())
    .await
    .expect("per-guild analytics settings table must exist");
    sqlx::query(
        "INSERT INTO guild_analytics_settings (guild_id, presence_enabled, activity_enabled, voice_enabled) \
         VALUES ($1, true, true, true)",
    )
    .bind(GUILD_B)
    .execute(db.pool())
    .await
    .unwrap();

    // Category representatives: presence_update, first_message (activity),
    // voice_session_start (voice analytics).
    for (event_type, at) in [
        ("presence_update", "2026-10-12T00:00:00Z"),
        ("first_message", "2026-10-12T00:01:00Z"),
        ("voice_session_start", "2026-10-12T00:02:00Z"),
    ] {
        record_event(&db, &event(GUILD_A, MEMBER_A, event_type, at))
            .await
            .unwrap();
        record_event(&db, &event(GUILD_B, MEMBER_A, event_type, at))
            .await
            .unwrap();
    }

    let guild_a: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD_A)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        guild_a, 1,
        "only the enabled activity category may persist in guild A"
    );
    let activity: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = 'first_message'",
    )
    .bind(GUILD_A)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(activity, 1);
    let guild_b: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD_B)
            .bind(MEMBER_A)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(guild_b, 3, "guild B has every category enabled");

    db.close().await;
    test_db.close().await.unwrap();
}

/// Exactly one audit row is written per opt-out/purge, scoped to the opting
/// guild/member; other pairs write nothing.
#[tokio::test]
async fn opt_out_writes_one_audit_row_per_purge() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    opt_out(&db, GUILD_A, MEMBER_A).await;
    sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
        .bind(GUILD_A)
        .bind(MEMBER_A)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO privacy_opt_out_audit (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUILD_A)
        .bind(MEMBER_A)
        .execute(db.pool())
        .await
        .expect("one audit row per opt-out/purge");

    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_out_audit WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(receipts, 1);
    let unrelated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_out_audit WHERE NOT (guild_id = $1 AND member_id = $2)",
    )
    .bind(GUILD_A)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(unrelated, 0);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Purge + suppression + audit must be transactionally safe: if the audit
/// receipt fails, the deletions and the suppression row roll back together.
#[tokio::test]
async fn audit_failure_rolls_back_purge_and_suppression() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    record_event(
        &db,
        &event(GUILD_A, MEMBER_A, "first_message", "2026-10-12T00:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(event_count(&db, GUILD_A, MEMBER_A).await, 1);

    // Late receipt failure probe: any audit insert raises, so the enclosing
    // opt-out transaction must roll back (mirrors the member-erasure receipt
    // trigger pattern).
    sqlx::raw_sql(
        "CREATE FUNCTION reject_privacy_opt_out_receipt() RETURNS trigger AS $$ BEGIN
        RAISE EXCEPTION 'fixture receipt unavailable'; END $$ LANGUAGE plpgsql;
        CREATE TRIGGER reject_privacy_opt_out_receipt BEFORE INSERT ON privacy_opt_out_audit
        FOR EACH ROW EXECUTE FUNCTION reject_privacy_opt_out_receipt();",
    )
    .execute(db.pool())
    .await
    .expect("audit table must exist to attach the failure probe");

    let attempt: Result<(), sqlx::Error> = async {
        let mut tx = db.pool().begin().await?;
        sqlx::query("DELETE FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD_A)
            .bind(MEMBER_A)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD_A)
            .bind(MEMBER_A)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO privacy_opt_out_audit (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD_A)
            .bind(MEMBER_A)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    assert!(
        attempt.is_err(),
        "audit failure must abort the opt-out transaction"
    );

    assert_eq!(
        event_count(&db, GUILD_A, MEMBER_A).await,
        1,
        "purge must roll back when the receipt fails"
    );
    let suppression: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD_A)
    .bind(MEMBER_A)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(suppression, 0);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Removing the suppression entry (opt-back-in) restores persistence for that
/// pair only.
#[tokio::test]
async fn re_enable_restores_persistence() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    opt_out(&db, GUILD_A, MEMBER_A).await;
    record_event(
        &db,
        &event(GUILD_A, MEMBER_A, "first_message", "2026-10-12T00:00:00Z"),
    )
    .await
    .unwrap();
    assert_eq!(
        event_count(&db, GUILD_A, MEMBER_A).await,
        0,
        "suppressed write must be dropped before re-enable"
    );

    let removed: u64 =
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD_A)
            .bind(MEMBER_A)
            .execute(db.pool())
            .await
            .unwrap()
            .rows_affected();
    assert_eq!(removed, 1);
    let (inserted, _) = record_event(
        &db,
        &event(GUILD_A, MEMBER_A, "first_message", "2026-10-12T01:00:00Z"),
    )
    .await
    .unwrap();
    assert!(inserted);
    assert_eq!(event_count(&db, GUILD_A, MEMBER_A).await, 1);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Defaults are unchanged: with no suppression or toggle rows, every write
/// persists exactly as today. The empty-table read pins that the contract
/// starts from zero rows, not from altered write behavior.
#[tokio::test]
async fn defaults_without_suppression_persist_all_writes() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    let suppression_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_outs")
        .fetch_one(db.pool())
        .await
        .expect("guild-scoped opt-out table must exist");
    assert_eq!(suppression_rows, 0);

    for (guild, member) in [
        (GUILD_A, MEMBER_A),
        (GUILD_A, MEMBER_B),
        (GUILD_B, MEMBER_A),
        (GUILD_B, MEMBER_B),
    ] {
        let (inserted, _) = record_event(
            &db,
            &event(guild, member, "first_message", "2026-10-12T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(inserted);
        assert_eq!(event_count(&db, guild, member).await, 1);
    }

    db.close().await;
    test_db.close().await.unwrap();
}
