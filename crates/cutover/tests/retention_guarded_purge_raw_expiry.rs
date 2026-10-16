//! A replay-guarded guild purge must defer without blocking independent raw expiry.

use std::future::Future;

use sqlx::PgPool;
use two_bot_cutover::retention::{handle_guild_delete, run_retention, RetentionMode};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "333333333333333333";
const MEMBER: &str = "444444444444444444";

async fn with_database<F, Fut>(check: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("guarded-purge retention acceptance requires the disposable test database");
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("create migrated disposable retention fixture");
    let result = tokio::spawn(check(db.pool().clone())).await;
    db.close()
        .await
        .expect("verify disposable fixture teardown");
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("guarded-purge retention acceptance task was cancelled");
    }
}

#[tokio::test]
async fn unresolved_guild_purge_defers_while_eligible_voice_capture_expires() {
    with_database(|pool| async move {
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(GUILD)
            .bind(MEMBER)
            .execute(&pool)
            .await
            .expect("seed member protected by the deferred guild purge");
        sqlx::query(
            "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
             VALUES ($1, $2, 150, 150, now())",
        )
        .bind(GUILD)
        .bind(MEMBER)
        .execute(&pool)
        .await
        .expect("seed guild data the queued purge would delete");

        sqlx::query(
            "INSERT INTO internal_idempotency
               (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id)
             VALUES (repeat('a', 64), repeat('b', 64), 'role.assign', repeat('c', 64),
                     'unknown', $1, $2, $2)",
        )
        .bind(GUILD)
        .bind(MEMBER)
        .execute(&pool)
        .await
        .expect("seed unresolved replay guard");
        assert!(
            handle_guild_delete(&pool, GUILD, None)
                .await
                .expect("enqueue guild purge"),
            "guild deletion must queue a purge"
        );

        sqlx::query(
            "INSERT INTO event_store
               (event_type, guild_id, user_id, event_time, received_at, dedup_key,
                channel_id, session_id)
             VALUES
               ('VOICE_STATE_UPDATE', $1, $2, now() - interval '31 days', now(), 'expired-voice', $3, 'expired'),
               ('VOICE_STATE_UPDATE', $1, $2, now() - interval '1 day', now(), 'fresh-voice', $3, 'fresh'),
               ('MESSAGE_CREATE', $1, $2, now() - interval '31 days', now(), 'untimed-event', $3, 'untimed')",
        )
        .bind(GUILD)
        .bind(MEMBER)
        .bind("555555555555555555")
        .execute(&pool)
        .await
        .expect("seed expired, fresh, and non-expiring raw captures");

        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("execute retention");
        assert_eq!(report.voice_raw_deleted, 1, "only the eligible voice capture expires");
        assert_eq!(report.guild_rows_purged, 0, "guarded guild purge deletes no rows");
        assert_eq!(report.guild_purges_failed, 0, "guard refusal is a deferral, not failure");
        assert_eq!(report.guild_purges_deferred, 1, "guarded guild purge remains deferred");

        let guard_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM internal_idempotency WHERE guild_id = $1 AND state = 'unknown'",
        )
        .bind(GUILD)
        .fetch_one(&pool)
        .await
        .expect("read replay guard");
        assert_eq!(guard_count, 1, "the unresolved replay guard must remain untouched");

        let level_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM member_levels WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(GUILD)
        .bind(MEMBER)
        .fetch_one(&pool)
        .await
        .expect("read purge-protected guild data");
        assert_eq!(level_count, 1, "guild data protected by the deferred purge survives");

        let captures: Vec<String> = sqlx::query_scalar(
            "SELECT dedup_key FROM event_store WHERE guild_id = $1 ORDER BY dedup_key",
        )
        .bind(GUILD)
        .fetch_all(&pool)
        .await
        .expect("read remaining raw captures");
        assert_eq!(captures, ["fresh-voice", "untimed-event"]);

        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM retention_guild_purge_queue WHERE guild_id = $1",
        )
        .bind(GUILD)
        .fetch_one(&pool)
        .await
        .expect("read queued guild purge");
        assert_eq!(queued, 1, "the deferred guild purge must remain queued");
    })
    .await;
}
