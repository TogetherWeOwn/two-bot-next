//! Guild-scoped birthday persistence. Never log rows: month/day are private.

use sqlx::{PgPool, Postgres, Transaction};
use time::Date;

pub(crate) const MEMBER_CAP: i64 = 100;
pub(crate) const CHEER_CAP: i64 = 25;

#[derive(sqlx::FromRow)]
pub(crate) struct Profile {
    pub member_id: String,
    pub birth_month: i16,
    pub birth_day: i16,
}

#[derive(sqlx::FromRow, Default)]
pub(crate) struct Settings {
    pub channel_id: Option<String>,
    pub role_id: Option<String>,
    pub cheer: Option<String>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct Grant {
    pub member_id: String,
    pub role_id: String,
    pub celebration_date: Date,
}

/// Commands and sweeps take the same guild lock. A dropped transaction releases
/// it on cancellation; no session lock can leak into the connection pool.
/// The sweep uses this transaction only as a lock holder, committing its effect
/// intents on other connections BEFORE the wire. The lock holder writes no rows.
pub(crate) async fn lock(
    pool: &PgPool,
    guild: &str,
) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 455))")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub(crate) async fn settings(pool: &PgPool, guild: &str) -> Result<Settings, sqlx::Error> {
    Ok(
        sqlx::query_as(
            "SELECT channel_id, role_id, cheer FROM birthday_config WHERE guild_id = $1",
        )
        .bind(guild)
        .fetch_optional(pool)
        .await?
        .unwrap_or_default(),
    )
}

pub(crate) async fn configure(
    pool: &PgPool,
    guild: &str,
    settings: &Settings,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO birthday_config (guild_id, channel_id, role_id, cheer) VALUES ($1,$2,$3,$4) ON CONFLICT (guild_id) DO UPDATE SET channel_id = COALESCE(EXCLUDED.channel_id, birthday_config.channel_id), role_id = COALESCE(EXCLUDED.role_id, birthday_config.role_id), cheer = COALESCE(EXCLUDED.cheer, birthday_config.cheer)")
        .bind(guild).bind(&settings.channel_id).bind(&settings.role_id).bind(&settings.cheer)
        .execute(pool).await?;
    Ok(())
}

pub(crate) async fn set(
    pool: &PgPool,
    guild: &str,
    member: &str,
    month: i16,
    day: i16,
) -> Result<bool, sqlx::Error> {
    // Serialized by lock(): concurrent new registrations cannot exceed the cap;
    // members already enrolled can still correct their date at capacity.
    let changed = sqlx::query("INSERT INTO birthday_profiles (guild_id, member_id, birth_month, birth_day) SELECT $1,$2,$3,$4 WHERE (SELECT count(*) FROM birthday_profiles WHERE guild_id = $1) < $5 OR EXISTS (SELECT 1 FROM birthday_profiles WHERE guild_id = $1 AND member_id = $2) ON CONFLICT (guild_id, member_id) DO UPDATE SET birth_month = EXCLUDED.birth_month, birth_day = EXCLUDED.birth_day")
        .bind(guild).bind(member).bind(month).bind(day).bind(MEMBER_CAP)
        .execute(pool).await?.rows_affected();
    Ok(changed == 1)
}

pub(crate) async fn show(
    pool: &PgPool,
    guild: &str,
    member: &str,
) -> Result<Option<Profile>, sqlx::Error> {
    sqlx::query_as("SELECT member_id, birth_month, birth_day FROM birthday_profiles WHERE guild_id = $1 AND member_id = $2")
        .bind(guild).bind(member).fetch_optional(pool).await
}

pub(crate) async fn remove(pool: &PgPool, guild: &str, member: &str) -> Result<(), sqlx::Error> {
    // Replay receipts and pending grants deliberately outlive the opt-in row.
    sqlx::query("DELETE FROM birthday_profiles WHERE guild_id = $1 AND member_id = $2")
        .bind(guild)
        .bind(member)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn profiles(pool: &PgPool, guild: &str) -> Result<Vec<Profile>, sqlx::Error> {
    sqlx::query_as("SELECT member_id, birth_month, birth_day FROM birthday_profiles WHERE guild_id = $1 ORDER BY length(member_id), member_id LIMIT $2")
        .bind(guild).bind(MEMBER_CAP).fetch_all(pool).await
}

pub(crate) async fn grants(pool: &PgPool, guild: &str) -> Result<Vec<Grant>, sqlx::Error> {
    sqlx::query_as("SELECT member_id, role_id, celebration_date FROM birthday_role_grants WHERE guild_id = $1 ORDER BY length(member_id), member_id, role_id LIMIT 201")
        .bind(guild).fetch_all(pool).await
}

pub(crate) async fn track_grant(
    pool: &PgPool,
    guild: &str,
    member: &str,
    role: &str,
    date: Date,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO birthday_role_grants (guild_id, member_id, role_id, celebration_date) VALUES ($1,$2,$3,$4) ON CONFLICT (guild_id, member_id, role_id) DO UPDATE SET celebration_date = EXCLUDED.celebration_date")
        .bind(guild).bind(member).bind(role).bind(date).execute(pool).await?;
    Ok(())
}

pub(crate) async fn clear_grant(
    pool: &PgPool,
    guild: &str,
    member: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM birthday_role_grants WHERE guild_id = $1 AND member_id = $2 AND role_id = $3",
    )
    .bind(guild)
    .bind(member)
    .bind(role)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn claim_cheer(
    pool: &PgPool,
    guild: &str,
    member: &str,
    date: Date,
    channel: &str,
    nonce: &str,
) -> Result<bool, sqlx::Error> {
    // One guild lock spans cap, claim and wire; the committed claim survives
    // cancellation after send. Never reclaim an unconfirmed send by age.
    Ok(sqlx::query("INSERT INTO birthday_cheers (guild_id, member_id, celebration_date, channel_id, nonce) SELECT $1,$2,$3,$4,$5 WHERE (SELECT count(*) FROM birthday_cheers WHERE guild_id = $1 AND celebration_date = $3) < $6 ON CONFLICT (guild_id, member_id, celebration_date) DO NOTHING")
        .bind(guild).bind(member).bind(date).bind(channel).bind(nonce).bind(CHEER_CAP)
        .execute(pool).await?.rows_affected() == 1)
}

pub(crate) async fn has_uncertain(pool: &PgPool, guild: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM birthday_cheers WHERE guild_id = $1 AND message_id IS NULL)",
    )
    .bind(guild)
    .fetch_one(pool)
    .await
}

pub(crate) async fn posted(
    pool: &PgPool,
    guild: &str,
    member: &str,
    date: Date,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE birthday_cheers SET message_id = $4 WHERE guild_id = $1 AND member_id = $2 AND celebration_date = $3")
        .bind(guild).bind(member).bind(date).bind(message).execute(pool).await?;
    Ok(())
}
