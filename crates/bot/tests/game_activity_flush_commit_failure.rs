//! A rejected commit must requeue the drained play and voice interval.
//! The deferred constraint trigger fails only COMMIT, not the upsert, and
//! lives only in the owned disposable database.

use two_bot::voice_rooms::game_activity::{persist_guild, GameActivityCollector, PersistOutcome};
use two_bot_core::game_activity::{ActivitySnapshot, ActivityType, PresenceDelta};
use two_bot_core::game_activity_store::{read_guild_aggregates, GameActivityStoreError};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER: u64 = 123456789012345678;
const MIN: i64 = 60_000;
const T0: i64 = 1_750_000_000_000;

fn playing(at_ms: i64) -> PresenceDelta {
    PresenceDelta {
        user_key: MEMBER.to_string(),
        observed_at_ms: at_ms,
        activities: vec![ActivitySnapshot {
            activity_type: ActivityType::Playing,
            name: "Apex".to_owned(),
            application_id: None,
            details: None,
            state: None,
            party_size: None,
            assets: false,
            button_labels: Vec::new(),
            button_urls: Vec::new(),
            stream_url: None,
        }],
    }
}

#[tokio::test]
async fn rejected_commit_requeues_play_and_voice_for_exact_retry() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the game-activity commit failure database"
            );
            eprintln!("SKIP game-activity commit failure: TWO_TEST_DATABASE_URL not set");
            return;
        }
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("owned disposable database");
    let pool = fixture.pool();
    sqlx::query(
        "CREATE FUNCTION reject_aggregate_commit() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION USING ERRCODE = '23514',
                 MESSAGE = 'synthetic deferred aggregate failure';
         END
         $$",
    )
    .execute(pool)
    .await
    .expect("create commit failure function");
    sqlx::query(
        "CREATE CONSTRAINT TRIGGER reject_aggregate_commit
         AFTER INSERT OR UPDATE ON game_activity_guild_aggregates
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
         EXECUTE FUNCTION reject_aggregate_commit()",
    )
    .execute(pool)
    .await
    .expect("defer failure until commit");

    let collector = GameActivityCollector::new();
    collector.observe_presence(GUILD, playing(T0));
    collector.observe_presence(GUILD, playing(T0 + 30 * MIN));
    collector.observe_join(GUILD, MEMBER, T0);
    collector.observe_leave(GUILD, MEMBER, T0 + MIN);
    let error = persist_guild(pool, &collector, GUILD, 60 * MIN, T0 + 30 * MIN)
        .await
        .expect_err("deferred trigger rejects the commit");
    let GameActivityStoreError::Db(sqlx::Error::Database(error)) = error else {
        panic!("expected the synthetic deferred constraint failure");
    };
    assert_eq!(error.code().as_deref(), Some("23514"));
    assert_eq!(error.message(), "synthetic deferred aggregate failure");
    assert!(
        read_guild_aggregates(pool, &GUILD.to_string())
            .await
            .expect("read after rejected commit")
            .is_empty(),
        "a rejected commit persists none of the interval"
    );
    sqlx::query("DROP TRIGGER reject_aggregate_commit ON game_activity_guild_aggregates")
        .execute(pool)
        .await
        .expect("remove only the fixture fault");

    // No new observations: the retry must recover the drained interval,
    // retain its voice overlap and extend the same play carry to its clock.
    assert_eq!(
        persist_guild(pool, &collector, GUILD, 60 * MIN, T0 + 35 * MIN)
            .await
            .expect("retry after commit rejection"),
        PersistOutcome::Persisted {
            rows: 1,
            sessions: 1
        }
    );
    let rows = read_guild_aggregates(pool, &GUILD.to_string())
        .await
        .expect("read recovered interval");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].game.as_deref(), Some("Apex"));
    assert_eq!(rows[0].total_play_ms, 35 * MIN);
    assert_eq!(rows[0].in_voice_ms, MIN);
    assert_eq!(rows[0].session_count, 1);
    assert_eq!(
        persist_guild(pool, &collector, GUILD, 60 * MIN, T0 + 35 * MIN)
            .await
            .expect("already drained retry"),
        PersistOutcome::Empty
    );
    let after = read_guild_aggregates(pool, &GUILD.to_string())
        .await
        .expect("read after empty retry");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].total_play_ms, 35 * MIN);
    assert_eq!(after[0].in_voice_ms, MIN);
    assert_eq!(after[0].session_count, 1);
    fixture
        .close()
        .await
        .expect("drop owned disposable database");
}
