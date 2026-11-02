//! Starboard PR-1 store acceptance (TOG-36121, paired build TOG-36122).
//!
//! Test files ONLY: this target compiles green but each test FAILS until the
//! builder lands the starboard store (currently: `relation
//! "starboard_config" does not exist`). The build card implements:
//!
//! * `crates/store` migrations creating `starboard_config`,
//!   `starboard_starrers` and `starboard_posts` per the TOG-35924 spec,
//!   applied from the store migration chain so [`migrate`] creates them;
//! * small `two_bot_store::starboard_*` wrappers (suggested:
//!   `get/update_starboard_config`, `add/remove_starrer`, `recount_starrers`,
//!   `insert_or_select_starboard_post`) over the exact SQL below, so the
//!   wrappers and these tests agree byte for byte.
//!
//! What the tests prove (TOG-36121 AC2 + AC3 + AC4-store + AC5-schema):
//!
//! 1. config CRUD + defaults (threshold 3, emoji star, board channel nullable
//!    = feature off, no exclusions by default, one row per guild);
//! 2. starrer upsert/remove + recount from DB, idempotent under replay, with
//!    PK `(guild_id, source_message_id, user_id)`;
//! 3. post insert-or-select: concurrent inserts yield exactly one row per
//!    `(guild_id, source_message_id)` (UNIQUE), with the post state lifecycle
//!    columns (`posted` | `below_threshold` | `source_deleted`);
//! 4. PR-1 adds exactly the three tables — no posting/editing behavior yet.
//!
//! Only agent-testdb or a CI service container is admitted, with the
//! agent_test identity. DATABASE_URL is never consulted.
//!
//! Run: `cargo test -p two-bot-store --locked --test starboard_store -- --ignored`
//! with `TEST_DATABASE_URL` pointed at the authorized test database.
//!
//! CI: the builder's PR must add `--test starboard_store` to the store
//! Postgres step in `.github/workflows/check.yml` (this file's `#[ignore]`
//! tests never run in CI otherwise). Check step line ~1730.
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
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

/// Config snapshot. Mirrors the builder's `get_starboard_config`.
async fn get_config(
    pool: &Pool<Postgres>,
    guild_id: i64,
) -> Option<(Option<i64>, i32, String, Vec<i64>)> {
    sqlx::query_as(
        "SELECT board_channel_id, threshold::int, emoji,
                COALESCE(excluded_channel_ids, '{}'::bigint[])
         FROM starboard_config WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// Upsert a starrer row. Mirrors the builder's `add_starrer`.
async fn add_starrer(
    pool: &Pool<Postgres>,
    guild_id: i64,
    channel_id: i64,
    message_id: i64,
    user_id: i64,
) {
    sqlx::query(
        "INSERT INTO starboard_starrers
             (guild_id, source_channel_id, source_message_id, user_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, source_message_id, user_id) DO NOTHING",
    )
    .bind(guild_id)
    .bind(channel_id)
    .bind(message_id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Remove a starrer row (absent rows are a no-op). Mirrors `remove_starrer`.
async fn remove_starrer(pool: &Pool<Postgres>, guild_id: i64, message_id: i64, user_id: i64) {
    sqlx::query(
        "DELETE FROM starboard_starrers
         WHERE guild_id = $1 AND source_message_id = $2 AND user_id = $3",
    )
    .bind(guild_id)
    .bind(message_id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Absolute recount from the DB-tracked distinct starrers. Mirrors
/// the builder's `recount_starrers`: the count source of truth, never a
/// blind increment.
async fn recount(pool: &Pool<Postgres>, guild_id: i64, message_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM starboard_starrers
         WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(guild_id)
    .bind(message_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Insert-or-select a board post row: concurrent racers converge on one row.
/// Mirrors the builder's `insert_or_select_starboard_post`.
async fn insert_or_select_post(
    pool: &Pool<Postgres>,
    guild_id: i64,
    channel_id: i64,
    message_id: i64,
    board_channel_id: i64,
    board_message_id: i64,
) -> (i64, String) {
    sqlx::query(
        "INSERT INTO starboard_posts
             (guild_id, source_channel_id, source_message_id, board_channel_id,
              board_message_id, star_count, state)
         VALUES ($1, $2, $3, $4, $5, 0, 'posted')
         ON CONFLICT (guild_id, source_message_id) DO NOTHING",
    )
    .bind(guild_id)
    .bind(channel_id)
    .bind(message_id)
    .bind(board_channel_id)
    .bind(board_message_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_as(
        "SELECT board_message_id, state::text FROM starboard_posts
         WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(guild_id)
    .bind(message_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

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
        // Threat-model F6: tests opt into `LocalOnly` explicitly rather than
        // inheriting the process `TWO_DATABASE_TLS` value.
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "starboard_store_test_{}_{}",
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
        // Only schemas created by this fixture; never shared tables or roles.
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

/// True when `err` is a Postgres unique-violation (SQLSTATE 23505).
fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "23505")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_defaults_board_off_until_channel_set() {
    // AC2/AC5: a fresh guild has no row; a default row carries threshold 3,
    // the star emoji, a NULL board channel (feature off until set) and no
    // exclusions.
    let f = Fixture::new().await;
    assert_eq!(get_config(&f.pool, GUILD).await, None);
    sqlx::query("INSERT INTO starboard_config (guild_id) VALUES ($1)")
        .bind(GUILD)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        get_config(&f.pool, GUILD).await,
        Some((None, 3, "⭐".to_owned(), vec![])),
        "defaults must be threshold 3, star emoji, unset board, no exclusions"
    );
    let stamped: bool = sqlx::query_scalar(
        "SELECT updated_at IS NOT NULL FROM starboard_config WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(stamped, "config writes must stamp updated_at");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_crud_round_trip() {
    // AC3: full config lifecycle — insert, update every field, delete.
    let f = Fixture::new().await;
    sqlx::query(
        "INSERT INTO starboard_config
             (guild_id, board_channel_id, threshold, emoji,
              excluded_channel_ids, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(GUILD)
    .bind(BOARD_CHANNEL)
    .bind(5_i16)
    .bind("🌟")
    .bind(vec![CHANNEL])
    .bind("admin-user")
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        get_config(&f.pool, GUILD).await,
        Some((Some(BOARD_CHANNEL), 5, "🌟".to_owned(), vec![CHANNEL]))
    );
    sqlx::query(
        "UPDATE starboard_config
         SET board_channel_id = NULL, threshold = 1, emoji = '⭐',
             excluded_channel_ids = '{}'::bigint[], updated_by = 'other-admin'
         WHERE guild_id = $1",
    )
    .bind(GUILD)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        get_config(&f.pool, GUILD).await,
        Some((None, 1, "⭐".to_owned(), vec![]))
    );
    let author: String =
        sqlx::query_scalar("SELECT updated_by FROM starboard_config WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(author, "other-admin".to_owned());
    sqlx::query("DELETE FROM starboard_config WHERE guild_id = $1")
        .bind(GUILD)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(get_config(&f.pool, GUILD).await, None);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn config_one_row_per_guild() {
    // AC2: PK on guild_id — a duplicate insert fails, an upsert updates.
    let f = Fixture::new().await;
    sqlx::query("INSERT INTO starboard_config (guild_id, threshold) VALUES ($1, 3)")
        .bind(GUILD)
        .execute(&f.pool)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO starboard_config (guild_id) VALUES ($1)")
        .bind(GUILD)
        .execute(&f.pool)
        .await
        .unwrap_err();
    assert!(
        is_unique_violation(&err),
        "duplicate guild config must be a unique violation, got {err:?}"
    );
    sqlx::query(
        "INSERT INTO starboard_config (guild_id, threshold) VALUES ($1, 7)
         ON CONFLICT (guild_id) DO UPDATE SET threshold = excluded.threshold",
    )
    .bind(GUILD)
    .execute(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        get_config(&f.pool, GUILD).await,
        Some((None, 7, "⭐".to_owned(), vec![]))
    );
    let pk: String = sqlx::query_scalar(
        "SELECT string_agg(a.attname, ',' ORDER BY k.ord)
         FROM pg_constraint c
         JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
         JOIN pg_class t ON t.oid = c.conrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE c.contype = 'p' AND t.relname = 'starboard_config'
           AND n.nspname = current_schema()",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(pk, "guild_id", "config PK must be exactly (guild_id)");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn starrer_upsert_remove_and_recount() {
    // AC3/AC4: upsert/remove + absolute recount; gateway replay (duplicate
    // upsert) and duplicate remove never move the count.
    let f = Fixture::new().await;
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 0);
    for user in [USER_A, USER_B, USER_C] {
        add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, user).await;
    }
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 3);
    // Replay: the same adds arrive again (gateway resume) — still 3.
    for user in [USER_A, USER_B, USER_C] {
        add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, user).await;
    }
    assert_eq!(
        recount(&f.pool, GUILD, MESSAGE).await,
        3,
        "replayed adds must not double-count"
    );
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
        "starrer rows must carry the source channel"
    );
    remove_starrer(&f.pool, GUILD, MESSAGE, USER_B).await;
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 2);
    // Duplicate remove of an absent row is a no-op.
    remove_starrer(&f.pool, GUILD, MESSAGE, USER_B).await;
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 2);
    add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_B).await;
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 3);
    // Other messages/guilds recount independently.
    add_starrer(&f.pool, GUILD, CHANNEL, OTHER_MESSAGE, USER_A).await;
    add_starrer(&f.pool, OTHER_GUILD, CHANNEL, MESSAGE, USER_A).await;
    assert_eq!(recount(&f.pool, GUILD, MESSAGE).await, 3);
    assert_eq!(recount(&f.pool, GUILD, OTHER_MESSAGE).await, 1);
    assert_eq!(recount(&f.pool, OTHER_GUILD, MESSAGE).await, 1);
    let stamped: bool =
        sqlx::query_scalar("SELECT bool_and(created_at IS NOT NULL) FROM starboard_starrers")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(stamped, "starrer rows must stamp created_at");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn starrer_pk_is_guild_message_user() {
    // AC2: PK (guild_id, source_message_id, user_id) — raw duplicates fail,
    // neighbouring keys insert fine.
    let f = Fixture::new().await;
    add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_A).await;
    let err = sqlx::query(
        "INSERT INTO starboard_starrers
             (guild_id, source_channel_id, source_message_id, user_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(MESSAGE)
    .bind(USER_A)
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert!(
        is_unique_violation(&err),
        "duplicate starrer must be a unique violation, got {err:?}"
    );
    add_starrer(&f.pool, GUILD, CHANNEL, OTHER_MESSAGE, USER_A).await;
    add_starrer(&f.pool, GUILD, CHANNEL, MESSAGE, USER_B).await;
    add_starrer(&f.pool, OTHER_GUILD, CHANNEL, MESSAGE, USER_A).await;
    let pk: String = sqlx::query_scalar(
        "SELECT string_agg(a.attname, ',' ORDER BY k.ord)
         FROM pg_constraint c
         JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true
         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
         JOIN pg_class t ON t.oid = c.conrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE c.contype = 'p' AND t.relname = 'starboard_starrers'
           AND n.nspname = current_schema()",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        pk, "guild_id,source_message_id,user_id",
        "starrer PK must be exactly (guild_id, source_message_id, user_id)"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn post_insert_or_select_race_yields_one_row() {
    // AC3: concurrent ReactionAdd events race insert-or-select; exactly one
    // post row survives and every racer reads the winner's row.
    let f = Fixture::new().await;
    let mut handles = Vec::new();
    for racer in 0..8_i64 {
        let pool = f.pool.clone();
        handles.push(tokio::spawn(async move {
            insert_or_select_post(
                &pool,
                GUILD,
                CHANNEL,
                MESSAGE,
                BOARD_CHANNEL,
                9_000_000_000_000_000 + racer,
            )
            .await
        }));
    }
    let mut seen = Vec::new();
    for handle in handles {
        seen.push(handle.await.unwrap());
    }
    assert!(
        seen.iter().all(|row| row == &seen[0]),
        "every racer must read the same winning row, got {seen:?}"
    );
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
async fn post_unique_per_guild_message() {
    // AC2: UNIQUE(guild_id, source_message_id) — one board post per source
    // message ever; other guilds/messages insert fine.
    let f = Fixture::new().await;
    insert_or_select_post(&f.pool, GUILD, CHANNEL, MESSAGE, BOARD_CHANNEL, 9_001).await;
    let err = sqlx::query(
        "INSERT INTO starboard_posts
             (guild_id, source_channel_id, source_message_id, board_channel_id,
              board_message_id, star_count, state)
         VALUES ($1, $2, $3, $4, $5, 0, 'posted')",
    )
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(MESSAGE)
    .bind(BOARD_CHANNEL)
    .bind(9_002_i64)
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert!(
        is_unique_violation(&err),
        "duplicate post must be a unique violation, got {err:?}"
    );
    insert_or_select_post(&f.pool, OTHER_GUILD, CHANNEL, MESSAGE, BOARD_CHANNEL, 9_003).await;
    insert_or_select_post(&f.pool, GUILD, CHANNEL, OTHER_MESSAGE, BOARD_CHANNEL, 9_004).await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM starboard_posts")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 3);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn post_state_lifecycle_columns() {
    // AC2/AC3: board_message_id stays NULL until posted; star_count and the
    // posted/below_threshold/source_deleted states round-trip.
    let f = Fixture::new().await;
    sqlx::query(
        "INSERT INTO starboard_posts
             (guild_id, source_channel_id, source_message_id, board_channel_id,
              star_count, state)
         VALUES ($1, $2, $3, $4, 0, 'posted')",
    )
    .bind(GUILD)
    .bind(CHANNEL)
    .bind(MESSAGE)
    .bind(BOARD_CHANNEL)
    .execute(&f.pool)
    .await
    .unwrap();
    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT board_message_id FROM starboard_posts
         WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(GUILD)
    .bind(MESSAGE)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        pending, None,
        "board_message_id must be nullable until posted"
    );
    for (board_message_id, star_count, state) in [
        (Some(9_101_i64), 3_i64, "posted"),
        (Some(9_101), 1, "below_threshold"),
        (Some(9_101), 1, "source_deleted"),
    ] {
        sqlx::query(
            "UPDATE starboard_posts
             SET board_message_id = $3, star_count = $4, state = $5
             WHERE guild_id = $1 AND source_message_id = $2",
        )
        .bind(GUILD)
        .bind(MESSAGE)
        .bind(board_message_id)
        .bind(star_count)
        .bind(state)
        .execute(&f.pool)
        .await
        .unwrap();
        let row: (Option<i64>, i64, String) = sqlx::query_as(
            "SELECT board_message_id, star_count::bigint, state::text FROM starboard_posts
             WHERE guild_id = $1 AND source_message_id = $2",
        )
        .bind(GUILD)
        .bind(MESSAGE)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (board_message_id, star_count, state.to_owned()),
            "post row must round-trip state {state}"
        );
    }
    let stamped: bool = sqlx::query_scalar(
        "SELECT created_at IS NOT NULL AND updated_at IS NOT NULL
         FROM starboard_posts WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(GUILD)
    .bind(MESSAGE)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(stamped, "post rows must stamp created_at/updated_at");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires authorized TEST_DATABASE_URL"]
async fn starboard_surface_is_exactly_three_tables() {
    // AC5: PR-1 is tables only — exactly config/starrers/posts, no posting or
    // runtime tables smuggled in early.
    let f = Fixture::new().await;
    let mut tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = current_schema() AND tablename LIKE 'starboard%'
         ORDER BY tablename",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    tables.sort();
    assert_eq!(
        tables,
        vec![
            "starboard_config".to_owned(),
            "starboard_posts".to_owned(),
            "starboard_starrers".to_owned(),
        ],
        "PR-1 must create exactly the three starboard tables"
    );
    f.finish().await;
}
