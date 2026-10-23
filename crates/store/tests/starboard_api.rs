//! Starboard foundation API acceptance (TOG-36320, paired build TOG-36122).
//!
//! Test file ONLY: this target is compile-RED until the builder lands the
//! planned public module `two_bot_store::starboard` (currently: `unresolved
//! import`). The build card implements, per the TOG-36320 spec:
//!
//! * `pub mod starboard` with `StarboardConfig` (`board_channel_id:
//!   Option<i64>`, `threshold: i16`, `emoji: String`, `excluded_channel_ids:
//!   Vec<i64>`, `updated_by: Option<String>`, `Default` = off/3/star/empty/None)
//!   and `StarboardPost` (`guild_id`, `source_channel_id`, `source_message_id`,
//!   `board_channel_id: i64`, `board_message_id: Option<i64>`, `star_count: i64`,
//!   `state: String`);
//! * async wrappers, `&PgPool` first, `i64` snowflakes, `Result<_, sqlx::Error>`:
//!   `get_starboard_config(pool, guild) -> Option<StarboardConfig>`,
//!   `update_starboard_config(pool, guild, &config) -> ()`,
//!   `delete_starboard_config(pool, guild) -> bool`,
//!   `add_starrer(pool, guild, channel, message, user) -> bool`,
//!   `remove_starrer(pool, guild, message, user) -> bool`,
//!   `recount_starrers(pool, guild, message) -> i64`,
//!   `insert_or_select_starboard_post(pool, &post) -> StarboardPost`;
//! * a DB `CHECK (threshold BETWEEN 1 AND 50)` on `starboard_config`, so
//!   out-of-range writes fail with SQLSTATE 23514 from Postgres, not Rust.
//!
//! What the tests prove (TOG-36320 item 1):
//!
//! 1. the wrappers exist with the exact public shapes above (compile proves
//!    the signatures; field-by-field asserts prove the structs, not a
//!    duplicate raw-SQL reimplementation);
//! 2. config roundtrip/defaults/delete through the wrappers, cross-checked
//!    against raw SQL in both directions;
//! 3. starrer add/remove bools (true once, false on duplicate) and an absolute
//!    distinct recount, independent per message/guild;
//! 4. post insert-or-select: 8 concurrent racers converge on one row and every
//!    racer reads the winner;
//! 5. the threshold range is a DB constraint (23514), with 1 and 50 accepted.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test starboard_api -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
//!
//! CI: the builder's PR must add `--test starboard_api` beside `--test
//! starboard_store` on the store Postgres step in `.github/workflows/check.yml`
//! (this file's `#[ignore]` tests never run in CI otherwise).
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_store::starboard::{
    add_starrer, delete_starboard_config, get_starboard_config, insert_or_select_starboard_post,
    recount_starrers, remove_starrer, update_starboard_config, StarboardConfig, StarboardPost,
};
use two_bot_store::{migrate, DB_POOL_MAX};

const GUILD: i64 = 100_000_000_000_000_001;
const OTHER_GUILD: i64 = 100_000_000_000_000_009;
const CHANNEL: i64 = 100_000_000_000_000_007;
const BOARD_CHANNEL: i64 = 100_000_000_000_000_010;
const MESSAGE: i64 = 100_000_000_000_000_008;
const OTHER_MESSAGE: i64 = 100_000_000_000_000_011;
const USER_A: i64 = 100_000_000_000_000_002;
const USER_B: i64 = 100_000_000_000_000_003;
const USER_C: i64 = 100_000_000_000_000_004;

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL").expect("set TEST_DATABASE_URL for DB tests");
        let options = PgConnectOptions::from_str(&url).expect("invalid TEST_DATABASE_URL");
        let ci_container = std::env::var("CI").as_deref() == Ok("true")
            && matches!(options.get_host(), "localhost" | "127.0.0.1" | "postgres");
        assert!(
            options.get_host() == "agent-testdb" || ci_container,
            "test containers only"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "starboard_api_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Self::connect(&url, &schema).await;
        migrate(&pool).await.unwrap();
        Self {
            admin,
            pool,
            schema,
        }
    }

    async fn connect(url: &str, schema: &str) -> Pool<Postgres> {
        let options = PgConnectOptions::from_str(url).expect("invalid TEST_DATABASE_URL");
        PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.to_owned()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

/// True when `err` is a Postgres check-violation (SQLSTATE 23514).
fn is_check_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "23514")
}

fn assert_config_eq(actual: &StarboardConfig, expected: &StarboardConfig) {
    assert_eq!(
        actual.board_channel_id, expected.board_channel_id,
        "board_channel_id"
    );
    assert_eq!(actual.threshold, expected.threshold, "threshold");
    assert_eq!(actual.emoji, expected.emoji, "emoji");
    assert_eq!(
        actual.excluded_channel_ids, expected.excluded_channel_ids,
        "excluded_channel_ids"
    );
    assert_eq!(actual.updated_by, expected.updated_by, "updated_by");
}

fn assert_post_eq(actual: &StarboardPost, expected: &StarboardPost) {
    assert_eq!(actual.guild_id, expected.guild_id, "guild_id");
    assert_eq!(
        actual.source_channel_id, expected.source_channel_id,
        "source_channel_id"
    );
    assert_eq!(
        actual.source_message_id, expected.source_message_id,
        "source_message_id"
    );
    assert_eq!(
        actual.board_channel_id, expected.board_channel_id,
        "board_channel_id"
    );
    assert_eq!(
        actual.board_message_id, expected.board_message_id,
        "board_message_id"
    );
    assert_eq!(actual.star_count, expected.star_count, "star_count");
    assert_eq!(actual.state, expected.state, "state");
}

#[test]
fn starboard_config_default_is_off_threshold_3_star() {
    // Hermetic: the Default impl is off (NULL board), threshold 3, star emoji,
    // no exclusions, no author. No DB needed; compile proves the module exists.
    let config = StarboardConfig::default();
    assert_eq!(config.board_channel_id, None);
    assert_eq!(config.threshold, 3);
    assert_eq!(config.emoji, "⭐");
    assert!(config.excluded_channel_ids.is_empty());
    assert_eq!(config.updated_by, None);
    // Public struct literal construction proves every field is public.
    let literal = StarboardConfig {
        board_channel_id: Some(BOARD_CHANNEL),
        threshold: 5,
        emoji: "🌟".to_owned(),
        excluded_channel_ids: vec![CHANNEL],
        updated_by: Some("admin-user".to_owned()),
    };
    assert_config_eq(
        &literal,
        &StarboardConfig {
            board_channel_id: Some(BOARD_CHANNEL),
            threshold: 5,
            emoji: "🌟".to_owned(),
            excluded_channel_ids: vec![CHANNEL],
            updated_by: Some("admin-user".to_owned()),
        },
    );
}

#[test]
fn starboard_post_fields_are_public_and_constructible() {
    // Hermetic: struct-literal construction proves the seven public fields.
    let post = StarboardPost {
        guild_id: GUILD,
        source_channel_id: CHANNEL,
        source_message_id: MESSAGE,
        board_channel_id: BOARD_CHANNEL,
        board_message_id: Some(9_001),
        star_count: 3,
        state: "posted".to_owned(),
    };
    assert_post_eq(
        &post,
        &StarboardPost {
            guild_id: GUILD,
            source_channel_id: CHANNEL,
            source_message_id: MESSAGE,
            board_channel_id: BOARD_CHANNEL,
            board_message_id: Some(9_001),
            star_count: 3,
            state: "posted".to_owned(),
        },
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_get_returns_none_for_fresh_guild() {
    let f = Fixture::new().await;
    assert!(
        get_starboard_config(&f.pool, GUILD)
            .await
            .unwrap()
            .is_none(),
        "a fresh guild has no config row"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_update_and_get_roundtrip_via_wrappers() {
    let f = Fixture::new().await;
    // Default row through the wrappers: off, 3, star, empty, None.
    update_starboard_config(&f.pool, GUILD, &StarboardConfig::default())
        .await
        .unwrap();
    let fetched = get_starboard_config(&f.pool, GUILD)
        .await
        .unwrap()
        .expect("default update must create a row");
    assert_config_eq(&fetched, &StarboardConfig::default());
    // Full row through the wrappers.
    let full = StarboardConfig {
        board_channel_id: Some(BOARD_CHANNEL),
        threshold: 5,
        emoji: "🌟".to_owned(),
        excluded_channel_ids: vec![CHANNEL],
        updated_by: Some("admin-user".to_owned()),
    };
    update_starboard_config(&f.pool, GUILD, &full)
        .await
        .unwrap();
    let fetched = get_starboard_config(&f.pool, GUILD)
        .await
        .unwrap()
        .expect("full update must be readable");
    assert_config_eq(&fetched, &full);
    // Raw SQL sees exactly what the wrappers wrote (wrappers, not a shadow copy).
    let raw: (Option<i64>, i16, String, Vec<i64>, Option<String>) = sqlx::query_as(
        "SELECT board_channel_id, threshold, emoji, excluded_channel_ids, updated_by
         FROM starboard_config WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        raw,
        (
            Some(BOARD_CHANNEL),
            5,
            "🌟".to_owned(),
            vec![CHANNEL],
            Some("admin-user".to_owned())
        )
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_update_overwrites_and_delete_removes() {
    let f = Fixture::new().await;
    let first = StarboardConfig {
        board_channel_id: Some(BOARD_CHANNEL),
        threshold: 7,
        emoji: "🌟".to_owned(),
        excluded_channel_ids: vec![CHANNEL],
        updated_by: Some("first-admin".to_owned()),
    };
    update_starboard_config(&f.pool, GUILD, &first)
        .await
        .unwrap();
    let second = StarboardConfig {
        board_channel_id: None,
        threshold: 1,
        emoji: "⭐".to_owned(),
        excluded_channel_ids: vec![],
        updated_by: Some("other-admin".to_owned()),
    };
    update_starboard_config(&f.pool, GUILD, &second)
        .await
        .unwrap();
    let fetched = get_starboard_config(&f.pool, GUILD).await.unwrap().unwrap();
    assert_config_eq(&fetched, &second);
    assert!(
        delete_starboard_config(&f.pool, GUILD).await.unwrap(),
        "deleting a present row returns true"
    );
    assert!(
        get_starboard_config(&f.pool, GUILD)
            .await
            .unwrap()
            .is_none(),
        "deleted config must read back as None"
    );
    assert!(
        !delete_starboard_config(&f.pool, GUILD).await.unwrap(),
        "deleting an absent row returns false"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_wrapper_agrees_with_raw_sql_in_both_directions() {
    // Raw write -> wrapper read, then wrapper write -> raw read.
    let f = Fixture::new().await;
    sqlx::query(
        "INSERT INTO starboard_config
             (guild_id, board_channel_id, threshold, emoji,
              excluded_channel_ids, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(GUILD)
    .bind(BOARD_CHANNEL)
    .bind(9_i16)
    .bind("💫")
    .bind(vec![CHANNEL, OTHER_GUILD])
    .bind("raw-author")
    .execute(&f.pool)
    .await
    .unwrap();
    let fetched = get_starboard_config(&f.pool, GUILD)
        .await
        .unwrap()
        .expect("wrapper must read a raw-inserted row");
    assert_config_eq(
        &fetched,
        &StarboardConfig {
            board_channel_id: Some(BOARD_CHANNEL),
            threshold: 9,
            emoji: "💫".to_owned(),
            excluded_channel_ids: vec![CHANNEL, OTHER_GUILD],
            updated_by: Some("raw-author".to_owned()),
        },
    );
    update_starboard_config(&f.pool, GUILD, &StarboardConfig::default())
        .await
        .unwrap();
    let raw: (Option<i64>, i16, String, Vec<i64>, Option<String>) = sqlx::query_as(
        "SELECT board_channel_id, threshold, emoji, excluded_channel_ids, updated_by
         FROM starboard_config WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(raw, (None, 3, "⭐".to_owned(), vec![], None));
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn starrer_add_remove_bools_and_distinct_recount() {
    let f = Fixture::new().await;
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 0);
    // First add of each user returns true; the recount is the distinct count.
    for user in [USER_A, USER_B, USER_C] {
        assert!(
            add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, user)
                .await
                .unwrap(),
            "first add must return true"
        );
    }
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 3);
    // Replay (gateway resume): duplicate adds return false and never move the count.
    for user in [USER_A, USER_B, USER_C] {
        assert!(
            !add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, user)
                .await
                .unwrap(),
            "duplicate add must return false"
        );
    }
    assert_eq!(
        recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(),
        3,
        "replayed adds must not double-count"
    );
    // Remove returns true once, then false for the absent row.
    assert!(remove_starrer(&f.pool, GUILD, MESSAGE, USER_B)
        .await
        .unwrap());
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 2);
    assert!(
        !remove_starrer(&f.pool, GUILD, MESSAGE, USER_B)
            .await
            .unwrap(),
        "duplicate remove must return false"
    );
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 2);
    assert!(add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_B)
        .await
        .unwrap());
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 3);
    // Other messages/guilds recount independently.
    assert!(add_starrer(&f.pool, GUILD, CHANNEL, OTHER_MESSAGE, USER_A)
        .await
        .unwrap());
    assert!(add_starrer(&f.pool, OTHER_GUILD, CHANNEL, MESSAGE, USER_A)
        .await
        .unwrap());
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 3);
    assert_eq!(
        recount_starrers(&f.pool, GUILD, OTHER_MESSAGE)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        recount_starrers(&f.pool, OTHER_GUILD, MESSAGE)
            .await
            .unwrap(),
        1
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn starrer_wrapper_agrees_with_raw_sql() {
    let f = Fixture::new().await;
    assert!(add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_A)
        .await
        .unwrap());
    let channel: i64 = sqlx::query_scalar(
        "SELECT source_channel_id FROM starboard_starrers
         WHERE guild_id = $1 AND source_message_id = $2 AND user_id = $3",
    )
    .bind(GUILD)
    .bind(MESSAGE)
    .bind(USER_A)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        channel, CHANNEL,
        "wrapper add must carry the source channel"
    );
    // Raw insert is visible to the wrapper recount.
    sqlx::query(
        "INSERT INTO starboard_starrers
             (guild_id, source_channel_id, source_message_id, user_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(MESSAGE)
    .bind(USER_B)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(recount_starrers(&f.pool, GUILD, MESSAGE).await.unwrap(), 2);
    assert!(
        !add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_B)
            .await
            .unwrap(),
        "wrapper add over a raw row must return false"
    );
    assert!(remove_starrer(&f.pool, GUILD, MESSAGE, USER_A)
        .await
        .unwrap());
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM starboard_starrers WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(remaining, 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn post_insert_or_select_race_yields_one_winner_row() {
    // 8 concurrent ReactionAdd events race insert-or-select; exactly one post
    // row survives and every racer reads the winner's row through the wrapper.
    let f = Fixture::new().await;
    let mut handles = Vec::new();
    for racer in 0..8_i64 {
        let pool = f.pool.clone();
        handles.push(tokio::spawn(async move {
            let candidate = StarboardPost {
                guild_id: GUILD,
                source_channel_id: CHANNEL,
                source_message_id: MESSAGE,
                board_channel_id: BOARD_CHANNEL,
                board_message_id: Some(9_000_000_000_000_000 + racer),
                star_count: 0,
                state: "posted".to_owned(),
            };
            insert_or_select_starboard_post(&pool, &candidate).await
        }));
    }
    let mut seen = Vec::new();
    for handle in handles {
        seen.push(handle.await.unwrap().unwrap());
    }
    for row in &seen[1..] {
        assert_post_eq(row, &seen[0]);
    }
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM starboard_posts WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(GUILD)
    .bind(MESSAGE)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "the race must leave exactly one post row");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn post_wrapper_roundtrips_nullable_board_message_and_state() {
    let f = Fixture::new().await;
    // Pending post: NULL board message until posted.
    let pending = StarboardPost {
        guild_id: GUILD,
        source_channel_id: CHANNEL,
        source_message_id: MESSAGE,
        board_channel_id: BOARD_CHANNEL,
        board_message_id: None,
        star_count: 0,
        state: "below_threshold".to_owned(),
    };
    let stored = insert_or_select_starboard_post(&f.pool, &pending)
        .await
        .unwrap();
    assert_post_eq(&stored, &pending);
    // A second insert for the same source returns the winner, not the newcomer.
    let rival = StarboardPost {
        guild_id: GUILD,
        source_channel_id: CHANNEL,
        source_message_id: MESSAGE,
        board_channel_id: BOARD_CHANNEL,
        board_message_id: Some(9_101),
        star_count: 7,
        state: "posted".to_owned(),
    };
    let again = insert_or_select_starboard_post(&f.pool, &rival)
        .await
        .unwrap();
    assert_post_eq(&again, &pending);
    let raw: (Option<i64>, i64, String) = sqlx::query_as(
        "SELECT board_message_id, star_count, state::text FROM starboard_posts
         WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(GUILD)
    .bind(MESSAGE)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(raw, (None, 0, "below_threshold".to_owned()));
    // Other guilds/messages insert fine through the wrapper.
    for (guild, message) in [(OTHER_GUILD, MESSAGE), (GUILD, OTHER_MESSAGE)] {
        let other = StarboardPost {
            guild_id: guild,
            source_channel_id: CHANNEL,
            source_message_id: message,
            board_channel_id: BOARD_CHANNEL,
            board_message_id: Some(9_102),
            star_count: 3,
            state: "posted".to_owned(),
        };
        let stored = insert_or_select_starboard_post(&f.pool, &other)
            .await
            .unwrap();
        assert_post_eq(&stored, &other);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_posts")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 3);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn threshold_check_constraint_rejects_outside_1_to_50() {
    // The range is a DB CHECK, so out-of-range writes fail with 23514 from
    // Postgres even through the wrapper; 1 and 50 are the accepted edges.
    let f = Fixture::new().await;
    for threshold in [0_i16, 51, -1, 100, i16::MIN, i16::MAX] {
        let bad = StarboardConfig {
            threshold,
            ..StarboardConfig::default()
        };
        let err = update_starboard_config(&f.pool, GUILD, &bad)
            .await
            .unwrap_err();
        assert!(
            is_check_violation(&err),
            "threshold {threshold} must be a DB check violation (23514), got {err:?}"
        );
    }
    assert!(
        get_starboard_config(&f.pool, GUILD)
            .await
            .unwrap()
            .is_none(),
        "rejected thresholds must leave no row"
    );
    for threshold in [1_i16, 50] {
        let ok = StarboardConfig {
            threshold,
            ..StarboardConfig::default()
        };
        update_starboard_config(&f.pool, GUILD, &ok)
            .await
            .unwrap_or_else(|_| panic!("threshold {threshold} must be accepted"));
        let fetched = get_starboard_config(&f.pool, GUILD).await.unwrap().unwrap();
        assert_eq!(fetched.threshold, threshold);
    }
    // Raw SQL hits the same DB constraint, proving it lives in Postgres.
    let err = sqlx::query("INSERT INTO starboard_config (guild_id, threshold) VALUES ($1, 0)")
        .bind(OTHER_GUILD)
        .execute(&f.pool)
        .await
        .unwrap_err();
    assert!(
        is_check_violation(&err),
        "raw threshold 0 must be a DB check violation, got {err:?}"
    );
    f.finish().await;
}
