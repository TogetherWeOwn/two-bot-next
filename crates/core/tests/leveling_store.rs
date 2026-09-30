#![cfg(feature = "db")]

//! Opt-in database proof, restricted to agent-testdb or the CI service
//! container. Run:
//! cargo test -p two-bot-core --features db --locked --test leveling_store -- --ignored
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Ports the runtime half of legacy `test/unit.leveling.test.ts` against the
//! sqlx store in `leveling_store` (message cooldown, voice minutes +
//! cooldown, tie-breaks, ceiling behaviour, reward replacement, rank text).

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, QueryBuilder};
use two_bot_core::leveling::{
    leaderboard_reply, rank_reply, total_xp_for_level, LevelRoleReward, MAX_STORED_XP,
};
use two_bot_core::leveling_store as store;

const MIGRATION: &str = include_str!("../../cutover/migrations/0002_leveling.sql");
const GUILD: &str = "1545644954272137297";
const A: &str = "100000000000000001";
const B: &str = "100000000000000002";

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

// No DATABASE_URL or inherited credentials: tests accept only the approved
// agent container or the ephemeral Postgres service in GitHub Actions.
async fn pool() -> (PgPool, PgPool, String) {
    let ci = std::env::var("CI").as_deref() == Ok("true")
        && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
        && std::env::var("TWO_LEVELING_TEST_CI").as_deref() == Ok("1");
    let host = if ci { "127.0.0.1" } else { "agent-testdb" };
    let options = PgConnectOptions::new()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database(if ci { "postgres" } else { "agent_test" })
        .options([("statement_timeout", "5000ms")]);
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone())
        .await
        .expect("approved test DB must connect; no credential fallback");
    let schema = format!("leveling_test_{:032x}", rand::random::<u128>());
    // Identifier is a fixed prefix + generated hexadecimal, never user input.
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .expect("test schema");
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(query)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("schema pool");
    sqlx::raw_sql(MIGRATION)
        .execute(&pool)
        .await
        .expect("idempotent leveling migration");
    sqlx::raw_sql(MIGRATION)
        .execute(&pool)
        .await
        .expect("migration re-runs cleanly");
    (admin, pool, schema)
}

async fn teardown(admin: PgPool, schema: &str) {
    QueryBuilder::<Postgres>::new("DROP SCHEMA ")
        .push(schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .expect("drop test schema");
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn message_awards_enforce_durable_minute_cooldown(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    let channel = "200000000000000001";
    let first = store::award_message(&pool, GUILD, A, &at(0), Some(channel)).await?;
    let blocked = store::award_message(&pool, GUILD, A, &at(59), Some(channel)).await?;
    let second = store::award_message(&pool, GUILD, A, &at(60), Some(channel)).await?;
    assert_eq!(first.awarded, 15);
    assert_eq!(blocked.awarded, 0);
    assert_eq!(second.awarded, 15);
    assert_eq!(store::profile(&pool, GUILD, A).await?.message_xp, 30);
    // `xp_awards.xp` is INTEGER (INT4) per the legacy DDL; decode as i32.
    let awards: Vec<(String, i32)> =
        sqlx::query_as("SELECT source, xp FROM xp_awards WHERE guild_id = $1 ORDER BY id")
            .bind(GUILD)
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        awards,
        vec![("message".to_owned(), 15), ("message".to_owned(), 15),]
    );
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn voice_awards_use_completed_minutes_and_voice_cooldown(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    let channel = "300000000000000001";
    let first = store::award_voice(&pool, GUILD, A, 125, &at(0), Some(channel)).await?;
    let blocked = store::award_voice(&pool, GUILD, A, 600, &at(30), Some(channel)).await?;
    let second = store::award_voice(&pool, GUILD, A, 60, &at(60), Some(channel)).await?;
    assert_eq!(first.awarded, 10);
    assert_eq!(blocked.awarded, 0);
    assert_eq!(second.awarded, 5);
    assert_eq!(store::profile(&pool, GUILD, A).await?.voice_xp, 15);
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn profile_and_leaderboard_break_xp_ties_by_member_id(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    // Same total for both members: import is idempotent in the fixture only
    // via repeated equal rows, so seed through two identical awards is not
    // possible under cooldown — write the tie directly, then exercise reads.
    for member in [B, A] {
        sqlx::query(
            "INSERT INTO member_levels
               (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
             VALUES ($1, $2, 100, 100, 0, 0, $3::text::timestamptz)",
        )
        .bind(GUILD)
        .bind(member)
        .bind(at(0))
        .execute(&pool)
        .await?;
    }
    let board = store::leaderboard(&pool, GUILD, 10).await?;
    assert_eq!(
        board
            .iter()
            .map(|e| (e.member_id.clone(), e.rank))
            .collect::<Vec<_>>(),
        vec![(A.to_owned(), 1), (B.to_owned(), 2)]
    );
    assert_eq!(store::profile(&pool, GUILD, B).await?.rank, 2);
    let reply = leaderboard_reply(&board);
    assert!(reply.suppress_mentions);
    assert_eq!(
        reply.content,
        "**TWO XP Leaderboard**\n**1.** <@100000000000000001> · level **1** · 100 XP\n**2.** <@100000000000000002> · level **1** · 100 XP"
    );
    // Limit clamps to the legacy 1–25 bounds.
    assert_eq!(store::leaderboard(&pool, GUILD, 0).await?.len(), 1);
    assert_eq!(store::leaderboard(&pool, GUILD, 100).await?.len(), 2);
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn ceiling_rejection_preserves_cooldown_and_rank_text_reads(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    sqlx::query(
        "INSERT INTO member_levels
           (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
         VALUES ($1, $2, $3, 0, 0, $3, $4::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(A)
    .bind(MAX_STORED_XP as i64)
    .bind(at(0))
    .execute(&pool)
    .await?;
    let blocked = store::award_message(&pool, GUILD, A, &at(60), None).await?;
    assert_eq!(blocked.awarded, 0);
    assert_eq!(blocked.total_xp, MAX_STORED_XP);
    // Legacy golden: a rejected award leaves no cooldown row behind.
    let cooldowns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM xp_cooldowns WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(A)
    .fetch_one(&pool)
    .await?;
    assert_eq!(cooldowns, 0);

    let text = rank_reply(&store::profile(&pool, GUILD, A).await?, "Player One").content;
    assert!(text.contains("Player One"), "rank text names the member");
    assert!(text.contains("Rank **#1**"), "sole member ranks first");
    assert!(text.contains("to level "), "rank text names the next level");
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn rewards_replace_atomically_and_reject_bad_rows(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    store::replace_role_rewards(
        &pool,
        GUILD,
        &[
            LevelRoleReward {
                level: 10,
                role_id: "400000000000000010".to_owned(),
            },
            LevelRoleReward {
                level: 5,
                role_id: "400000000000000005".to_owned(),
            },
        ],
    )
    .await?;
    assert_eq!(
        store::role_rewards(&pool, GUILD).await?,
        vec![
            LevelRoleReward {
                level: 5,
                role_id: "400000000000000005".to_owned()
            },
            LevelRoleReward {
                level: 10,
                role_id: "400000000000000010".to_owned()
            },
        ]
    );
    store::replace_role_rewards(
        &pool,
        GUILD,
        &[LevelRoleReward {
            level: 20,
            role_id: "400000000000000020".to_owned(),
        }],
    )
    .await?;
    assert_eq!(
        store::role_rewards(&pool, GUILD).await?,
        vec![LevelRoleReward {
            level: 20,
            role_id: "400000000000000020".to_owned()
        }]
    );
    // Invalid rows fail before any write: the level-20 ladder survives.
    let bad = store::replace_role_rewards(
        &pool,
        GUILD,
        &[LevelRoleReward {
            level: 0,
            role_id: "400000000000000001".to_owned(),
        }],
    )
    .await;
    assert!(bad.is_err());
    assert_eq!(store::role_rewards(&pool, GUILD).await?.len(), 1);
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn level_up_crosses_threshold_and_plans_grants(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    // Bring A to 99 XP (message 15 × 6 = 90, then a 9-XP top-up), then one
    // more award crosses the L1 floor at 100.
    for step in 0..6 {
        store::award_message(&pool, GUILD, A, &at(step * 61), None).await?;
    }
    store::award(
        &pool,
        GUILD,
        A,
        two_bot_core::leveling::XpSource::Message,
        9,
        &at(6 * 61 + 1),
        None,
    )
    .await?;
    let award = store::award_message(&pool, GUILD, A, &at(7 * 61 + 2), None).await?;
    assert!(award.leveled_up);
    assert_eq!((award.previous_level, award.level), (0, 1));
    assert_eq!(award.total_xp, 114);
    assert_eq!(total_xp_for_level(1), 100);

    store::replace_role_rewards(
        &pool,
        GUILD,
        &[LevelRoleReward {
            level: 1,
            role_id: "400000000000000001".to_owned(),
        }],
    )
    .await?;
    let rewards = store::role_rewards(&pool, GUILD).await?;
    let profile = store::profile(&pool, GUILD, A).await?;
    assert_eq!(profile.level, 1);
    let plan = two_bot_core::leveling::plan_reward_roles(
        profile.level,
        &rewards,
        &[],
        two_bot_core::leveling::level_role_writes_allowed(false),
        false,
    );
    assert_eq!(plan.grant, vec!["400000000000000001".to_owned()]);
    assert!(plan.revoke.is_empty());
    // Bad timestamps are refused before any write.
    let bad = store::award_message(&pool, GUILD, A, "not-a-timestamp", None).await;
    assert!(matches!(
        bad,
        Err(two_bot_core::LevelingStoreError::InvalidTimestamp(_))
    ));
    // Unknown members read zero XP and rank below everyone holding XP
    // (COUNT(*) + 1 over strictly-greater rows) — never an error.
    let ghost = store::profile(&pool, GUILD, "100000000000000099").await?;
    assert_eq!((ghost.xp, ghost.rank), (0, 2));
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn concurrent_first_message_awards_have_one_winner(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = pool.clone();
        tasks.spawn(async move { store::award_message(&pool, GUILD, A, &at(0), None).await });
    }
    let mut winners = 0;
    while let Some(result) = tasks.join_next().await {
        let award = result.expect("award task")?;
        winners += u64::from(award.awarded == 15);
        assert_eq!(award.total_xp, 15, "losers read the committed winner's XP");
    }
    assert_eq!(winners, 1);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn concurrent_message_and_voice_awards_emit_one_level_up(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, imported_xp, updated_at)
                 VALUES ($1, $2, 99, 99, $3::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(A)
    .bind(at(0))
    .execute(&pool)
    .await?;
    let timestamp = at(60);
    let (message, voice) = tokio::join!(
        store::award_message(&pool, GUILD, A, &timestamp, None),
        store::award_voice(&pool, GUILD, A, 60, &timestamp, None),
    );
    let (message, voice) = (message?, voice?);
    assert_eq!((message.awarded, voice.awarded), (15, 5));
    assert_eq!(
        u64::from(message.leveled_up) + u64::from(voice.leveled_up),
        1
    );
    let profile = store::profile(&pool, GUILD, A).await?;
    assert_eq!(
        (
            profile.xp,
            profile.message_xp,
            profile.voice_xp,
            profile.imported_xp
        ),
        (119, 15, 5, 99)
    );
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn zero_and_oversized_awards_write_nothing() -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    let short = store::award_voice(&pool, GUILD, A, 59, &at(0), None).await?;
    let oversized = store::award(
        &pool,
        GUILD,
        A,
        two_bot_core::leveling::XpSource::Voice,
        MAX_STORED_XP + 1,
        &at(0),
        None,
    )
    .await?;
    assert_eq!((short.awarded, oversized.awarded), (0, 0));
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM member_levels),
                (SELECT COUNT(*) FROM xp_cooldowns),
                (SELECT COUNT(*) FROM xp_awards)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (0, 0, 0));
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the credential-free CI Postgres service"]
async fn audit_and_reward_write_failures_roll_back_everything(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let (admin, pool, schema) = pool().await;
    // The existing audit xp column is INT4. An overflowing audit insert must
    // roll back the preceding projection and cooldown writes too.
    let failed = store::award(
        &pool,
        GUILD,
        A,
        two_bot_core::leveling::XpSource::Voice,
        i32::MAX as u64 + 1,
        &at(0),
        None,
    )
    .await;
    assert!(matches!(
        failed,
        Err(two_bot_core::LevelingStoreError::Db(_))
    ));
    assert_eq!(store::current_award(&pool, GUILD, A).await?.total_xp, 0);
    assert_eq!(
        store::award_message(&pool, GUILD, A, &at(0), None)
            .await?
            .awarded,
        15
    );
    let original = [LevelRoleReward {
        level: 1,
        role_id: "400000000000000001".to_owned(),
    }];
    store::replace_role_rewards(&pool, GUILD, &original).await?;
    // Same role assigned at two levels violates the legacy UNIQUE constraint.
    let duplicate_role = [
        LevelRoleReward {
            level: 2,
            role_id: "400000000000000002".to_owned(),
        },
        LevelRoleReward {
            level: 3,
            role_id: "400000000000000002".to_owned(),
        },
    ];
    assert!(store::replace_role_rewards(&pool, GUILD, &duplicate_role)
        .await
        .is_err());
    assert_eq!(store::role_rewards(&pool, GUILD).await?, original);
    teardown(admin, &schema).await;
    pool.close().await;
    Ok(())
}
