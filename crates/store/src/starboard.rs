//! Starboard persistence only. No Discord calls, cached counters or activation.
//! Countable-star filtering and delivery admission belong to the caller.

use sqlx::PgPool;

/// Missing config is off; a newly created config has the same defaults.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StarboardConfig {
    pub board_channel_id: Option<i64>,
    pub threshold: i16,
    pub emoji: String,
    pub excluded_channel_ids: Vec<i64>,
    pub updated_by: Option<String>,
}

impl Default for StarboardConfig {
    fn default() -> Self {
        Self {
            board_channel_id: None,
            threshold: 3,
            emoji: "⭐".to_owned(),
            excluded_channel_ids: Vec::new(),
            updated_by: None,
        }
    }
}

/// One durable record per source message. A missing board message is pending,
/// not permission for every concurrent reader to send a Discord message.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StarboardPost {
    pub guild_id: i64,
    pub source_channel_id: i64,
    pub source_message_id: i64,
    pub board_channel_id: i64,
    pub board_message_id: Option<i64>,
    pub star_count: i64,
    pub state: String,
}

pub async fn get_starboard_config(
    pool: &PgPool,
    guild_id: i64,
) -> Result<Option<StarboardConfig>, sqlx::Error> {
    sqlx::query_as(
        "SELECT board_channel_id, threshold, emoji,
                COALESCE(excluded_channel_ids, '{}'::bigint[]) AS excluded_channel_ids,
                updated_by
         FROM starboard_config WHERE guild_id = $1",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await
}

/// Replace the guild config atomically; schema constraints validate thresholds.
pub async fn update_starboard_config(
    pool: &PgPool,
    guild_id: i64,
    config: &StarboardConfig,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO starboard_config
             (guild_id, board_channel_id, threshold, emoji, excluded_channel_ids, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (guild_id) DO UPDATE
         SET board_channel_id = excluded.board_channel_id,
             threshold = excluded.threshold, emoji = excluded.emoji,
             excluded_channel_ids = excluded.excluded_channel_ids,
             updated_by = excluded.updated_by, updated_at = clock_timestamp()",
    )
    .bind(guild_id)
    .bind(config.board_channel_id)
    .bind(config.threshold)
    .bind(&config.emoji)
    .bind(&config.excluded_channel_ids)
    .bind(&config.updated_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete only settings, not the guild's highlight history.
pub async fn delete_starboard_config(pool: &PgPool, guild_id: i64) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM starboard_config WHERE guild_id = $1")
            .bind(guild_id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Return whether this distinct starrer was newly inserted.
/// This alone is not a delivery fence: an old add after a remove needs the
/// caller's original delivery identity/order admission before reaching here.
pub async fn add_starrer(
    pool: &PgPool,
    guild_id: i64,
    channel_id: i64,
    message_id: i64,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
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
    .await?
    .rows_affected()
        == 1)
}

/// An absent starrer is a no-op, never a negative increment.
pub async fn remove_starrer(
    pool: &PgPool,
    guild_id: i64,
    message_id: i64,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "DELETE FROM starboard_starrers
         WHERE guild_id = $1 AND source_message_id = $2 AND user_id = $3",
    )
    .bind(guild_id)
    .bind(message_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Absolute DB count, not Discord's count or a cached increment.
pub async fn recount_starrers(
    pool: &PgPool,
    guild_id: i64,
    message_id: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM starboard_starrers
         WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(guild_id)
    .bind(message_id)
    .fetch_one(pool)
    .await
}

/// Concurrent insertions converge on the winner without overwriting its state.
/// Two statements intentionally use separate READ COMMITTED snapshots: an
/// INSERT/SELECT CTE could miss a winner that committed while INSERT waited.
/// There is no post deletion API; posting/editing needs a separate send claim.
pub async fn insert_or_select_starboard_post(
    pool: &PgPool,
    post: &StarboardPost,
) -> Result<StarboardPost, sqlx::Error> {
    sqlx::query(
        "INSERT INTO starboard_posts
             (guild_id, source_channel_id, source_message_id, board_channel_id,
              board_message_id, star_count, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (guild_id, source_message_id) DO NOTHING",
    )
    .bind(post.guild_id)
    .bind(post.source_channel_id)
    .bind(post.source_message_id)
    .bind(post.board_channel_id)
    .bind(post.board_message_id)
    .bind(post.star_count)
    .bind(&post.state)
    .execute(pool)
    .await?;
    sqlx::query_as(
        "SELECT guild_id, source_channel_id, source_message_id, board_channel_id,
                board_message_id, star_count, state
         FROM starboard_posts WHERE guild_id = $1 AND source_message_id = $2",
    )
    .bind(post.guild_id)
    .bind(post.source_message_id)
    .fetch_one(pool)
    .await
}
