//! Direct store boundaries, fixed vocabulary and timestamp ties on disposable Postgres.

use sqlx::PgPool;
use two_bot_cutover::voice_rooms::{PgRoomStore, ORPHAN_LEDGER_CAP, ORPHAN_LEDGER_PAGE_SIZE};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 100;
const FIRST: u64 = 8_000_000_000_000_000;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit disposable database required; never skip acceptance");
    TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("create isolated migrated fixture")
}

async fn seed_ties(pool: &PgPool, count: i64) {
    sqlx::query(
        "INSERT INTO voice_orphan_ledger (guild_id, channel_id, reason, recorded_at)
         SELECT $1, ($2 + n)::text, 'creator_compensation_failed', to_timestamp(1700000000)
         FROM generate_series(0::bigint, $3 - 1) AS n ORDER BY n",
    )
    .bind(GUILD.to_string())
    .bind(i64::try_from(FIRST).unwrap())
    .bind(count)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn past_end_page_preserves_the_exact_total() {
    let db = database().await;
    seed_ties(db.pool(), 11).await;
    let peer = db.independent_pool().await.unwrap();
    let store = PgRoomStore::new(peer.clone());
    let page = store.orphan_evidence_page(GUILD, 3).await.unwrap();
    assert!(page.rows.is_empty());
    assert_eq!(page.total, 11, "an empty slice is not an empty ledger");
    let empty_guild = store.orphan_evidence_page(GUILD + 1, 1).await.unwrap();
    assert!(empty_guild.rows.is_empty());
    assert_eq!(empty_guild.total, 0);
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn direct_store_pages_reject_before_offset_arithmetic() {
    let db = database().await;
    seed_ties(db.pool(), 1000).await;
    let store = PgRoomStore::new(db.pool().clone());
    assert_eq!(ORPHAN_LEDGER_CAP, 1000);
    assert_eq!(ORPHAN_LEDGER_PAGE_SIZE, 10);
    let last = store.orphan_evidence_page(GUILD, 100).await.unwrap();
    assert_eq!(last.total, 1000);
    assert_eq!(last.rows.len(), 10);
    assert_eq!(
        last.rows
            .iter()
            .map(|row| row.channel_id)
            .collect::<Vec<_>>(),
        (FIRST..FIRST + 10).rev().collect::<Vec<_>>()
    );
    for invalid in [0, 101, usize::MAX] {
        let error = store
            .orphan_evidence_page(GUILD, invalid)
            .await
            .unwrap_err();
        assert!(
            matches!(error, sqlx::Error::InvalidArgument(ref message) if message.contains("invalid voice orphan page")),
            "page {invalid} must be explicitly refused before arithmetic: {error}"
        );
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn database_rejects_an_unknown_reason_without_changing_evidence() {
    let db = database().await;
    seed_ties(db.pool(), 1).await;
    let before: Vec<(String, String, String, time::OffsetDateTime)> =
        sqlx::query_as("SELECT guild_id, channel_id, reason, recorded_at FROM voice_orphan_ledger")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let error = sqlx::query(
        "INSERT INTO voice_orphan_ledger (guild_id, channel_id, reason)
         VALUES ('200', '9000000000000000', 'arbitrary')",
    )
    .execute(db.pool())
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    let after =
        sqlx::query_as("SELECT guild_id, channel_id, reason, recorded_at FROM voice_orphan_ledger")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(before, after);
    db.close().await.unwrap();
}

#[tokio::test]
async fn equal_timestamp_pages_use_channel_id_as_the_secondary_order() {
    let db = database().await;
    seed_ties(db.pool(), 23).await;
    // Exercise the declared SQL order, not incidental ordering from an index.
    sqlx::query("DROP INDEX voice_orphan_ledger_newest")
        .execute(db.pool())
        .await
        .unwrap();
    let peer = db.independent_pool().await.unwrap();
    let store = PgRoomStore::new(peer.clone());
    let mut channels = Vec::new();
    for number in 1..=3 {
        let page = store.orphan_evidence_page(GUILD, number).await.unwrap();
        assert_eq!(page.total, 23);
        assert_eq!(page.rows.len(), if number == 3 { 3 } else { 10 });
        channels.extend(page.rows.into_iter().map(|row| row.channel_id));
    }
    assert_eq!(channels, (FIRST..FIRST + 23).rev().collect::<Vec<_>>());
    peer.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn equal_timestamp_retention_evicts_only_the_lowest_channel_id() {
    let db = database().await;
    seed_ties(db.pool(), 1000).await;
    // A helpful index must not mask a missing tie-break in the trim query.
    sqlx::query("DROP INDEX voice_orphan_ledger_newest")
        .execute(db.pool())
        .await
        .unwrap();
    // Fix only fixture observation time so the newly recorded row also ties.
    sqlx::raw_sql(
        "CREATE FUNCTION tie_observation_time() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN NEW.recorded_at := to_timestamp(1700000000); RETURN NEW; END $$;
         CREATE TRIGGER tie_observation_time BEFORE INSERT ON voice_orphan_ledger
         FOR EACH ROW EXECUTE FUNCTION tie_observation_time()",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let writer = db.independent_pool().await.unwrap();
    PgRoomStore::new(writer.clone())
        .record_creator_orphan(GUILD, FIRST + 1000)
        .await
        .unwrap();
    writer.close().await;
    let reader = db.independent_pool().await.unwrap();
    let retained: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM voice_orphan_ledger WHERE guild_id = $1 ORDER BY channel_id",
    )
    .bind(GUILD.to_string())
    .fetch_all(&reader)
    .await
    .unwrap();
    assert_eq!(
        retained,
        (FIRST + 1..=FIRST + 1000)
            .map(|id| id.to_string())
            .collect::<Vec<_>>(),
        "the tied boundary must retain every higher ID, never an arbitrary heap row"
    );
    reader.close().await;
    db.close().await.unwrap();
}
