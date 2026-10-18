//! Per-guild aggregate expiry acceptance (TOG-28393, for build TOG-28386 / PR #1082).
//!
//! Pins the shipped `expire_stale_guild_aggregates` retention behavior at the
//! public-store boundary: stale rows expire scoped to one guild, fresh rows
//! survive, other guilds are untouched until their own expiry, and the
//! opt-out gate keeps filtering exactly as before expiry runs. Expiry is
//! delete-only on guild-keyed counter rows; it neither reads nor writes the
//! `privacy_opt_outs` suppression table.
//!
//! Disposable database only (`TestDatabase` over the guarded agent-testdb
//! service). Skips without `TWO_TEST_DATABASE_URL`; never staging or
//! production. Synthetic fixtures only: no Discord, no live network, no
//! secrets.

#![cfg(feature = "db")]

use std::collections::BTreeSet;

use two_bot_core::game_activity::{
    ActivitySnapshot, ActivityType, GuildGameAggregate, PresenceDelta, VoiceSpan,
};
use two_bot_core::game_activity_store::{
    aggregate_filtered, expire_stale_guild_aggregates, list_aggregate_guilds, opted_out_user_keys,
    read_guild_aggregates, upsert_guild_aggregates,
};
use two_bot_testsupport::TestDatabase;

const MIN_MS: i64 = 60_000;
const T0: i64 = 1_750_000_000_000;

fn playing_delta(user_key: &str, at_ms: i64, game: &str) -> PresenceDelta {
    PresenceDelta {
        user_key: user_key.to_owned(),
        observed_at_ms: at_ms,
        activities: vec![ActivitySnapshot {
            activity_type: ActivityType::Playing,
            name: game.to_owned(),
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

fn voice_span(user_key: &str, join_ms: i64, leave_ms: i64) -> VoiceSpan {
    VoiceSpan {
        user_key: user_key.to_owned(),
        join_ms,
        leave_ms,
        self_stream: false,
    }
}

fn row(
    guild: &str,
    game: Option<&str>,
    total_play_ms: i64,
    in_voice_ms: i64,
    session_count: u64,
) -> GuildGameAggregate {
    GuildGameAggregate {
        guild_id: guild.to_owned(),
        game: game.map(str::to_owned),
        total_play_ms,
        in_voice_ms,
        session_count,
    }
}

async fn fixture(test: &str) -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure game-activity expiry acceptance ({test})"
            );
            eprintln!(
                "SKIP game_activity_expiry_acceptance ({test}): TWO_TEST_DATABASE_URL not set"
            );
            None
        }
    }
}

async fn backdate(pool: &sqlx::PgPool, guild: &str, game: &str, days: i32) {
    sqlx::query(
        "UPDATE game_activity_guild_aggregates
            SET updated_at = now() - make_interval(days => $3)
          WHERE guild_id = $1 AND game = $2",
    )
    .bind(guild)
    .bind(game)
    .bind(days)
    .execute(pool)
    .await
    .expect("backdate stale row");
}

/// Old rows expire scoped per guild while fresh rows survive: guild-a holds
/// one stale and one fresh game, guild-b holds one stale game, guild-c holds
/// only a fresh game. The dry-run listing previews exactly the rows the
/// delete takes, and each guild's expiry touches only that guild.
#[tokio::test]
async fn old_rows_expire_per_guild_while_fresh_rows_survive() {
    let Some(db) = fixture("per-guild-expiry").await else {
        return;
    };
    let pool = db.pool();

    upsert_guild_aggregates(
        pool,
        &[
            row("guild-a", Some("Apex"), 30 * MIN_MS, MIN_MS, 1),
            row("guild-a", Some("Valorant"), 10 * MIN_MS, 0, 1),
            row("guild-b", Some("Apex"), 99 * MIN_MS, 0, 9),
            row("guild-c", Some("Apex"), 5 * MIN_MS, 0, 1),
        ],
    )
    .await
    .expect("seed flush");
    backdate(pool, "guild-a", "Apex", 100).await;
    backdate(pool, "guild-b", "Apex", 100).await;

    // Dry-run preview names only the stale guilds with their stale counts.
    assert_eq!(
        list_aggregate_guilds(pool, 90)
            .await
            .expect("guild listing"),
        vec![("guild-a".to_owned(), 1), ("guild-b".to_owned(), 1),],
        "fresh-only guild-c must never appear in the sweep listing"
    );

    // Expiring guild-a deletes only its stale row.
    assert_eq!(
        expire_stale_guild_aggregates(pool, "guild-a", 90)
            .await
            .expect("expire guild-a"),
        1
    );
    let kept = read_guild_aggregates(pool, "guild-a")
        .await
        .expect("read guild-a");
    assert_eq!(kept.len(), 1, "fresh row survives, got {kept:?}");
    assert_eq!(kept[0].game.as_deref(), Some("Valorant"));
    assert_eq!(kept[0].total_play_ms, 10 * MIN_MS);
    assert_eq!(
        read_guild_aggregates(pool, "guild-b")
            .await
            .expect("read guild-b")
            .len(),
        1,
        "guild-b keeps its equally stale row until its own expiry"
    );

    // Guild-b's own expiry clears it; re-expiring a fresh guild deletes nothing.
    assert_eq!(
        expire_stale_guild_aggregates(pool, "guild-b", 90)
            .await
            .expect("expire guild-b"),
        1
    );
    assert!(read_guild_aggregates(pool, "guild-b")
        .await
        .expect("read emptied guild")
        .is_empty());
    assert_eq!(
        expire_stale_guild_aggregates(pool, "guild-a", 90)
            .await
            .expect("re-expire fresh guild"),
        0
    );
    assert_eq!(
        read_guild_aggregates(pool, "guild-c")
            .await
            .expect("read fresh-only guild")
            .len(),
        1,
        "the fresh-only guild keeps its row throughout"
    );
}

/// Expiry leaves opt-out filtering unchanged: the opted-out member's play
/// never persists, the suppression row survives expiry, and the same filter
/// still excludes the opted-out member when run again after expiry.
#[tokio::test]
async fn expiry_leaves_opt_out_filtering_unchanged() {
    let Some(db) = fixture("opt-out-unchanged").await else {
        return;
    };
    let pool = db.pool();

    // Guild-a suppresses user-optout; guild-b suppresses nobody.
    sqlx::query(
        "INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ('guild-a', 'user-optout')",
    )
    .execute(pool)
    .await
    .expect("suppress pair");
    assert_eq!(
        opted_out_user_keys(pool, "guild-a")
            .await
            .expect("read suppressions"),
        ["user-optout".to_owned()].into_iter().collect(),
        "suppression is guild-scoped and exact before expiry"
    );

    // The opted-out member plays 30 minutes in voice; the kept member plays
    // 10 minutes with no voice. Only the kept member's session aggregates.
    let deltas = vec![
        playing_delta("user-optout", T0, "Apex"),
        playing_delta("user-kept", T0, "Apex"),
        playing_delta("user-optout", T0 + 30 * MIN_MS, "Apex"),
        playing_delta("user-kept", T0 + 10 * MIN_MS, "Apex"),
    ];
    let spans = vec![voice_span("user-optout", T0, T0 + 30 * MIN_MS)];
    let opted_out: BTreeSet<String> = ["user-optout".to_owned()].into_iter().collect();
    let filtered = aggregate_filtered("guild-a", &deltas, &spans, 60 * MIN_MS, &opted_out);
    assert_eq!(filtered.len(), 1, "only the kept member aggregates");
    assert_eq!(filtered[0].session_count, 1);
    assert_eq!(filtered[0].total_play_ms, 10 * MIN_MS);
    assert_eq!(
        filtered[0].in_voice_ms, 0,
        "the opted-out voice span must not leak into the kept session"
    );

    // Persist the filtered flush, plus one row for guild-b, then age only
    // guild-a's row past the horizon.
    upsert_guild_aggregates(pool, &filtered)
        .await
        .expect("persist filtered flush");
    upsert_guild_aggregates(pool, &[row("guild-b", Some("Apex"), 5 * MIN_MS, 0, 1)])
        .await
        .expect("seed other guild");
    backdate(pool, "guild-a", "Apex", 100).await;

    assert_eq!(
        expire_stale_guild_aggregates(pool, "guild-a", 90)
            .await
            .expect("expire guild-a"),
        1,
        "the stale aggregate row expires"
    );
    assert!(read_guild_aggregates(pool, "guild-a")
        .await
        .expect("read expired guild")
        .is_empty());

    // The suppression table is untouched by the delete: same member still
    // suppressed in guild-a, guild-b still empty.
    assert_eq!(
        opted_out_user_keys(pool, "guild-a")
            .await
            .expect("suppressions survive expiry"),
        ["user-optout".to_owned()].into_iter().collect(),
        "expiry must not drop suppression rows"
    );
    assert!(opted_out_user_keys(pool, "guild-b")
        .await
        .expect("other guild empty")
        .is_empty());
    let survivors: Vec<(String,)> =
        sqlx::query_as("SELECT member_id FROM privacy_opt_outs WHERE guild_id = 'guild-a'")
            .fetch_all(pool)
            .await
            .expect("raw suppression read");
    assert_eq!(
        survivors.len(),
        1,
        "exactly the one suppression row remains"
    );

    // The filter still excludes the opted-out member when run after expiry,
    // with identical output: filtering semantics did not change.
    let refiltered = aggregate_filtered("guild-a", &deltas, &spans, 60 * MIN_MS, &opted_out);
    assert_eq!(
        refiltered, filtered,
        "opt-out filtering is identical before and after expiry"
    );
}
