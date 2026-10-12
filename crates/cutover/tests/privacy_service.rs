//! Service-level cover for `two_bot_cutover::privacy`: the atomic
//! purge-plus-suppression-plus-receipt, opt-back-in, and the analytics
//! toggle round trip. The acceptance contract itself lives in
//! `privacy_opt_out.rs`, which this file never duplicates: these tests call
//! the service instead of simulating it with raw SQL.

use two_bot_core::database_tls::TlsPolicy;
use two_bot_cutover::db::{record_event, CutoverDb, FunnelWrite};
use two_bot_cutover::privacy::{
    analytics_settings, opt_back_in, opt_out_member, set_analytics_settings,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const OTHER_GUILD: &str = "1545644954272137298";
const MEMBER: &str = "123456789012345678";
const OTHER_MEMBER: &str = "123456789012345679";

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the privacy service database suite"
            );
            eprintln!("SKIP privacy service integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

fn event(guild: &str, member: &str, event_type: &str) -> FunnelWrite {
    FunnelWrite {
        member_id: Some(member.to_owned()),
        guild_id: guild.to_owned(),
        event_type: event_type.to_owned(),
        occurred_at: "2026-10-12T00:00:00Z".to_owned(),
        source: "privacy-service-fixture".to_owned(),
        metadata: Some("{}".to_owned()),
    }
}

/// The service purges, suppresses and receipts in one call; later writes
/// drop, other pairs are untouched, and opt-back-in restores persistence.
#[tokio::test]
async fn service_opt_out_purges_suppresses_audits_then_reenables() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    for (guild, member) in [
        (GUILD, MEMBER),
        (GUILD, OTHER_MEMBER),
        (OTHER_GUILD, MEMBER),
    ] {
        record_event(&db, &event(guild, member, "first_message"))
            .await
            .unwrap();
    }

    let counts = opt_out_member(&db, GUILD, MEMBER, "operator:privacy-review")
        .await
        .expect("service opt-out commits");
    assert_eq!(counts.len(), 3);
    let purged: i64 = counts.iter().map(|c| c.rows).sum();
    assert_eq!(purged, 2, "one events row plus one members row");

    let (inserted, _) = record_event(&db, &event(GUILD, MEMBER, "first_message"))
        .await
        .unwrap();
    assert!(
        !inserted,
        "suppressed pair stays dropped after the service call"
    );
    // `member_join` is repeatable per timestamp, so a fresh join proves the
    // other member still persists while the opted-out pair drops.
    let mut join = event(GUILD, OTHER_MEMBER, "member_join");
    join.occurred_at = "2026-10-12T01:00:00Z".to_owned();
    let (inserted, _) = record_event(&db, &join).await.unwrap();
    assert!(inserted, "other members keep persisting after the opt-out");
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM privacy_opt_out_audit WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(MEMBER)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(receipts, 1);

    assert!(opt_back_in(&db, GUILD, MEMBER).await.unwrap());
    assert!(!opt_back_in(&db, GUILD, MEMBER).await.unwrap());
    let (inserted, _) = record_event(&db, &event(GUILD, MEMBER, "second_message"))
        .await
        .unwrap();
    assert!(inserted, "opt-back-in restores persistence");

    db.close().await;
    test_db.close().await.unwrap();
}

/// Invalid identities and actors fail before any write; nothing is purged,
/// suppressed or receipted.
#[tokio::test]
async fn service_opt_out_rejects_invalid_identity_and_actor() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    record_event(&db, &event(GUILD, MEMBER, "first_message"))
        .await
        .unwrap();

    for (guild, member, actor) in [
        ("not-a-snowflake", MEMBER, "operator:privacy-review"),
        (GUILD, "0", "operator:privacy-review"),
        (GUILD, MEMBER, ""),
        (GUILD, MEMBER, "has\ncontrol"),
    ] {
        assert!(
            opt_out_member(&db, guild, member, actor).await.is_err(),
            "{guild}/{member}/{actor:?} must be refused"
        );
    }
    assert!(opt_back_in(&db, GUILD, "bogus").await.is_err());

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD)
            .bind(MEMBER)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(remaining, 1);
    let suppression: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_outs")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(suppression, 0);
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_out_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(receipts, 0);

    db.close().await;
    test_db.close().await.unwrap();
}

/// Defaults are all-enabled; a stored triple round-trips per guild without
/// leaking across guilds.
#[tokio::test]
async fn analytics_settings_default_to_enabled_and_round_trip() {
    let Some((test_db, db)) = database().await else {
        return;
    };

    assert_eq!(
        analytics_settings(&db, GUILD).await.unwrap(),
        (true, true, true)
    );
    set_analytics_settings(&db, GUILD, false, true, false)
        .await
        .unwrap();
    assert_eq!(
        analytics_settings(&db, GUILD).await.unwrap(),
        (false, true, false)
    );
    assert_eq!(
        analytics_settings(&db, OTHER_GUILD).await.unwrap(),
        (true, true, true)
    );
    set_analytics_settings(&db, GUILD, true, true, true)
        .await
        .unwrap();
    assert_eq!(
        analytics_settings(&db, GUILD).await.unwrap(),
        (true, true, true)
    );
    assert!(analytics_settings(&db, "bogus").await.is_err());

    db.close().await;
    test_db.close().await.unwrap();
}
