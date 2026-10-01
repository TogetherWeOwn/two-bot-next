#![cfg(feature = "db")]

//! Database proof restricted to the named disposable agent-testdb service.
//! Create an empty two_bot_test_local bootstrap as described in CONTRIBUTING.md,
//! then run all 13 tests:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! cargo test -p two-bot-core --features db --locked --test leveling_store
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Ports the runtime half of legacy `test/unit.leveling.test.ts` against the
//! sqlx store in `leveling_store` (message cooldown, voice minutes +
//! cooldown, tie-breaks, ceiling behaviour, reward replacement, rank text).

use two_bot_core::leveling::{
    leaderboard_reply, rank_reply, total_xp_for_level, LevelRoleReward, MAX_STORED_XP,
};
use two_bot_core::leveling_store as store;
use two_bot_testsupport::TestDatabase;

const MIGRATION: &str = include_str!("../../cutover/migrations/0002_leveling.sql");
const GUILD: &str = "1545644954272137297";
const A: &str = "100000000000000001";
const B: &str = "100000000000000002";

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database");
    // Preserve the original repeated-DDL proof after applying the full chain.
    for _ in 0..2 {
        sqlx::raw_sql(MIGRATION)
            .execute(fixture.pool())
            .await
            .expect("migration re-runs cleanly");
    }
    fixture
}

#[tokio::test]
async fn message_awards_enforce_durable_minute_cooldown(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn voice_awards_use_completed_minutes_and_voice_cooldown(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let channel = "300000000000000001";
    let first = store::award_voice(&pool, GUILD, A, 125, &at(0), Some(channel)).await?;
    let blocked = store::award_voice(&pool, GUILD, A, 600, &at(30), Some(channel)).await?;
    let second = store::award_voice(&pool, GUILD, A, 60, &at(60), Some(channel)).await?;
    assert_eq!(first.awarded, 10);
    assert_eq!(blocked.awarded, 0);
    assert_eq!(second.awarded, 5);
    assert_eq!(store::profile(&pool, GUILD, A).await?.voice_xp, 15);
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn profile_and_leaderboard_break_xp_ties_by_member_id(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn ceiling_rejection_preserves_cooldown_and_rank_text_reads(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn rewards_replace_atomically_and_reject_bad_rows(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn level_up_crosses_threshold_and_plans_grants(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn submillisecond_awards_keep_exact_sixty_second_boundary(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    // TOG-10359: the parser truncates sub-millisecond input, but the store
    // bound the raw string — the stored first instant sat 500µs after the
    // second cutoff, so an award exactly 60 s later was rejected. All binds
    // now carry the millisecond-normalized form, matching legacy
    // `new Date(ms).toISOString()`.
    let first = store::award_message(&pool, GUILD, A, "2026-09-30T12:00:00.000500Z", None).await?;
    let second = store::award_message(&pool, GUILD, A, "2026-09-30T12:01:00.000500Z", None).await?;
    assert_eq!((first.awarded, second.awarded), (15, 15));
    assert_eq!(second.total_xp, 30);
    // Equivalent offset forms normalize to the same instants: 59 s after the
    // second award stays inside cooldown, 60 s fires again.
    let blocked =
        store::award_message(&pool, GUILD, A, "2026-09-30T14:01:59.000500+02:00", None).await?;
    assert_eq!(blocked.awarded, 0);
    let third =
        store::award_message(&pool, GUILD, A, "2026-09-30T14:02:00.000500+02:00", None).await?;
    assert_eq!(third.awarded, 15);
    // Stored instants carry no sub-millisecond residue.
    let stored: String = sqlx::query_scalar(
        "SELECT last_awarded_at::text FROM xp_cooldowns
         WHERE guild_id = $1 AND member_id = $2 AND source = 'message'",
    )
    .bind(GUILD)
    .bind(A)
    .fetch_one(&pool)
    .await?;
    assert!(
        stored.starts_with("2026-09-30 12:02:00"),
        "cooldown instant normalized to millis, got {stored}"
    );
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn profile_stays_consistent_under_concurrent_awards(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, message_xp, updated_at)
                 VALUES ($1, $2, 100, 100, $3::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(A)
    .bind(at(0))
    .execute(&pool)
    .await?;
    // TOG-10359: profile read XP, rank and count in three statements, so an
    // award committing between the first two ranked a sole member 2 of 1.
    // The single-statement read shares one snapshot: a sole member always
    // reads rank 1 of 1, whatever commits alongside.
    for round in 1..=30 {
        let timestamp = at(i64::from(round) * 61);
        let (awarded, profile) = tokio::join!(
            store::award_message(&pool, GUILD, A, &timestamp, None),
            store::profile(&pool, GUILD, A),
        );
        awarded?;
        let profile = profile?;
        assert_eq!(profile.member_count, 1, "round {round}");
        assert_eq!(profile.rank, 1, "round {round}");
        assert!(profile.rank <= profile.member_count, "round {round}");
    }
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_empty_ladder_replacements_keep_one_ladder(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
    let five = [LevelRoleReward {
        level: 5,
        role_id: "400000000000000005".to_owned(),
    }];
    let ten = [LevelRoleReward {
        level: 10,
        role_id: "400000000000000010".to_owned(),
    }];
    // TOG-10359: with an empty ladder both writers finished DELETE before
    // either INSERTed, committing the union of two independent
    // configurations. The per-guild advisory lock serializes replacements,
    // so every round stores exactly one requested ladder, never the union.
    for round in 0..10 {
        sqlx::query("TRUNCATE level_role_rewards")
            .execute(&pool)
            .await?;
        let (left, right) = tokio::join!(
            store::replace_role_rewards(&pool, GUILD, &five),
            store::replace_role_rewards(&pool, GUILD, &ten),
        );
        left?;
        right?;
        let ladder = store::role_rewards(&pool, GUILD).await?;
        assert!(
            ladder == five || ladder == ten,
            "round {round}: union of independent ladders: {ladder:?}"
        );
    }
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_first_message_awards_have_one_winner(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn concurrent_message_and_voice_awards_emit_one_level_up(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn zero_and_oversized_awards_write_nothing() -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}

#[tokio::test]
async fn audit_and_reward_write_failures_roll_back_everything(
) -> Result<(), two_bot_core::LevelingStoreError> {
    let fixture = database().await;
    let pool = fixture.pool().clone();
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
    fixture.close().await.expect("drop test database");
    Ok(())
}
