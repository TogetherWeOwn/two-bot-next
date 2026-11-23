//! Synthetic PostgreSQL and SQLx fixtures for dormant attendance finalization.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use two_bot_cutover::event_attendance::finalization::{
    close_after_member_loss, close_observed, member_loss_page, open_page, CloseOutcome,
    CLOSES_PER_TURN, PAGE_SIZE,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000002";
const LOST: &str = "300000000000000001";
const NEIGHBOR: &str = "300000000000000002";
const JOIN: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database required");
    TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .unwrap()
}

async fn seed(pool: &PgPool, guild: &str, member: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO event_voice_sessions
             (guild_id, member_id, channel_id, channel_name, name_observed_at, joined_at, event_keys)
         VALUES ($1, $2, '400000000000000001', 'Fixture room', $3, $3, ARRAY['original'])
         RETURNING id",
    )
    .bind(guild)
    .bind(member)
    .bind(JOIN)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn state(
    pool: &PgPool,
    id: i64,
) -> (
    Option<OffsetDateTime>,
    Option<String>,
    String,
    Option<OffsetDateTime>,
) {
    sqlx::query_as(
        "SELECT left_at, event_keys::text, channel_name, name_observed_at
         FROM event_voice_sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn unsupported_tags_finalize_by_id_and_preserve_metadata() {
    let db = database().await;
    let pool = db.pool();
    sqlx::query("ALTER TABLE event_voice_sessions ALTER COLUMN event_keys DROP NOT NULL")
        .execute(pool)
        .await
        .unwrap();
    let left = JOIN + Duration::minutes(20);
    let mut cases = Vec::new();
    for expr in [
        "NULL::text[]",
        "ARRAY['a', NULL]::text[]",
        "'{{a,b},{c,d}}'::text[]",
        "'[0:1]={a,b}'::text[]",
        "'[2:3]={a,b}'::text[]",
        "array_fill('a'::text, ARRAY[129])",
        "ARRAY[repeat('é', 8193)]",
    ] {
        let id = seed(pool, GUILD, LOST).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE event_voice_sessions SET event_keys = {expr} WHERE id = $1"
        )))
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        cases.push((id, state(pool, id).await));
    }

    let open = open_page(pool, GUILD, LOST, 0).await.unwrap();
    assert_eq!(open.len(), cases.len());
    for (row, (id, mut before)) in open.into_iter().zip(cases) {
        assert_eq!(row.id, id);
        assert_eq!(row.joined_at, Some(JOIN));
        assert_eq!(row.channel_id.as_deref(), Some("400000000000000001"));
        assert_eq!(row.event_keys, None);
        assert_eq!(
            close_observed(pool, GUILD, LOST, id, left).await.unwrap(),
            CloseOutcome::ClosedWithUnsupportedTags
        );
        before.0 = Some(left);
        assert_eq!(state(pool, id).await, before);
        assert_eq!(
            close_observed(pool, GUILD, LOST, id, JOIN).await.unwrap(),
            CloseOutcome::AlreadyClosedOrAbsent
        );
        assert_eq!(state(pool, id).await, before);
    }
    assert!(open_page(pool, GUILD, LOST, 0).await.unwrap().is_empty());
    db.close().await.unwrap();
}

#[tokio::test]
async fn locked_close_checks_shape_after_an_earlier_healthy_read() {
    let db = database().await;
    let pool = db.pool();
    let id = seed(pool, GUILD, LOST).await;
    let open = open_page(pool, GUILD, LOST, 0).await.unwrap();
    assert_eq!(open[0].event_keys.as_deref().unwrap(), ["original"]);
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("UPDATE event_voice_sessions SET event_keys = '[0:1]={a,b}'::text[] WHERE id = $1")
        .bind(id)
        .execute(&mut *writer)
        .await
        .unwrap();

    let left = JOIN + Duration::minutes(20);
    let close_pool = pool.clone();
    let mut close =
        tokio::spawn(async move { close_observed(&close_pool, GUILD, LOST, id, left).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut close)
            .await
            .is_err()
    );
    writer.commit().await.unwrap();
    assert_eq!(
        close.await.unwrap().unwrap(),
        CloseOutcome::ClosedWithUnsupportedTags
    );
    let (closed_at, tags, ..) = state(pool, id).await;
    assert_eq!(closed_at, Some(left));
    assert_eq!(tags.as_deref(), Some("[0:1]={a,b}"));
    db.close().await.unwrap();
}

#[tokio::test]
async fn member_loss_pages_preserve_original_close_and_healthy_neighbor_credit() {
    let db = database().await;
    let pool = db.pool();
    let original = seed(pool, GUILD, LOST).await;
    let neighbor = seed(pool, GUILD, NEIGHBOR).await;
    let foreign = seed(pool, OTHER_GUILD, LOST).await;
    let mut lost = Vec::new();
    for _ in 0..PAGE_SIZE + 1 {
        lost.push(seed(pool, GUILD, LOST).await);
    }
    let left = JOIN + Duration::minutes(20);
    assert_eq!(
        close_observed(pool, GUILD, LOST, original, left)
            .await
            .unwrap(),
        CloseOutcome::Closed
    );
    let mut before = Vec::new();
    for id in [original, neighbor, foreign] {
        before.push((id, state(pool, id).await));
    }
    assert_eq!(
        member_loss_page(pool, GUILD, LOST, 0).await.unwrap(),
        lost[..32]
    );
    assert_eq!(open_page(pool, GUILD, LOST, 0).await.unwrap().len(), 32);
    assert_eq!(
        member_loss_page(pool, GUILD, LOST, lost[31]).await.unwrap(),
        lost[32..]
    );
    assert!(close_after_member_loss(pool, GUILD, LOST, &lost[..9])
        .await
        .is_err());
    assert_eq!(
        close_after_member_loss(pool, GUILD, LOST, &[original, neighbor, foreign])
            .await
            .unwrap(),
        0
    );
    for batch in lost.chunks(CLOSES_PER_TURN) {
        assert_eq!(
            close_after_member_loss(pool, GUILD, LOST, batch)
                .await
                .unwrap(),
            batch.len() as u64
        );
    }
    assert_eq!(
        close_after_member_loss(pool, GUILD, LOST, &[])
            .await
            .unwrap(),
        0
    );
    assert!(member_loss_page(pool, GUILD, LOST, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(state(pool, lost[0]).await.0, Some(JOIN));
    for (id, before) in before {
        assert_eq!(state(pool, id).await, before);
    }
    assert_eq!(
        close_observed(pool, GUILD, NEIGHBOR, neighbor, left)
            .await
            .unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(
        state(pool, neighbor).await.0.unwrap() - JOIN,
        Duration::minutes(20)
    );
    db.close().await.unwrap();
}
