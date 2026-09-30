//! Postgres leveling runtime: XP awards, profiles, leaderboard, rewards.
//!
//! Ports the `LevelingService` write/read paths of legacy two-bot
//! `src/leveling/service.ts` (`awardMessage`/`awardVoice`/`profile`/
//! `leaderboard`/`roleRewards`/`replaceRoleRewards`) over sqlx. Pure decisions
//! (curves, cooldown arithmetic, reward planning, reply text) live in
//! [`crate::leveling`]; this module only binds values, runs the award
//! transaction, and maps rows. Import (`importMee6`) stays with TOG-9882 in
//! `two-bot-cutover`, which owns these tables — this module reuses them and
//! never re-imports.
//!
//! Enabled only by the crate `db` feature so unit tests for pure domain logic
//! never need a Postgres driver or a live database.
//!
//! Transaction shape (legacy `award`): one transaction holding the
//! atomic cooldown claim (`INSERT … ON CONFLICT DO UPDATE … WHERE
//! last_awarded_at <= cutoff RETURNING`), then the XP upsert guarded by the
//! ceiling (`… WHERE member_levels.xp <= MAX - amount RETURNING xp`), then the
//! `xp_awards` audit row. A lost cooldown race or a zero amount returns the
//! current award with nothing written; a ceiling rejection rolls the whole
//! transaction back — including the cooldown claim — so a rejected award
//! never consumes cooldown (legacy golden test). Amounts above the ceiling
//! are rejected the same way instead of underflowing the guard.
//!
//! Everything takes `&PgPool` (`Copy`, reusable across the sequential
//! queries inside one call): reads are single-connection statements and the
//! two writers (`award`, `replace_role_rewards`) open their own transaction.
//! Statements stay literals; only values ride bind parameters (sqlx 0.9
//! audits dynamic SQL).
//!
//! Deliberately out of scope: the S4 interaction router (no command handling
//! here), the REST executor (role writes arrive as
//! [`RewardRolePlan`](crate::leveling::RewardRolePlan) data),
//! and the S3 `LevelingHook` bridge — that seam is synchronous while Postgres
//! access is async, so the bot-wiring slice owns the async bridge and calls
//! these primitives.

use sqlx::PgPool;

use super::funnel::{format_iso_millis, parse_iso_millis};
use super::leveling::{
    clamp_leaderboard_limit, level_for_xp, normalize_role_rewards, voice_minutes, LeaderboardEntry,
    LevelProfile, LevelRoleReward, RewardConfigError, XpAward, XpSource, AWARD_COOLDOWN_SECONDS,
    MAX_STORED_XP, MESSAGE_XP, VOICE_XP_PER_MINUTE,
};

/// Leveling runtime failure: invalid input vs database error.
#[derive(Debug, thiserror::Error)]
pub enum LevelingStoreError {
    #[error("occurred_at must be an ISO-8601 timestamp, got {0:?}")]
    InvalidTimestamp(String),
    #[error("invalid reward configuration: {0}")]
    InvalidRewards(#[from] RewardConfigError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

fn validate_at(at_iso: &str) -> Result<i64, LevelingStoreError> {
    parse_iso_millis(at_iso).ok_or_else(|| LevelingStoreError::InvalidTimestamp(at_iso.to_owned()))
}

/// Core award path (legacy private `award`): cooldown-claim → XP upsert →
/// `xp_awards` row, all in one transaction. `amount == 0` short-circuits to
/// the current award before any write (legacy early return), and
/// amounts above the ceiling are rejected without consuming cooldown.
pub async fn award(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    source: XpSource,
    amount: u64,
    at_iso: &str,
    channel_id: Option<&str>,
) -> Result<XpAward, LevelingStoreError> {
    let at_ms = validate_at(at_iso)?;
    // Legacy `service.ts` normalizes both timestamps through
    // `new Date(ms).toISOString()` before comparing or persisting. Bind that
    // same millisecond-normalized form everywhere: persisting the raw input
    // would store sub-millisecond instants the cutoff arithmetic (which
    // truncates to millis) cannot see, rejecting awards exactly 60 s apart.
    let at_norm = format_iso_millis(at_ms);
    if amount == 0 || amount > MAX_STORED_XP {
        return current_award(pool, guild_id, member_id).await;
    }
    let mut tx = pool.begin().await?;

    // Atomic cooldown claim: the INSERT wins only when no row exists or the
    // stored timestamp is at least a full cooldown old (legacy `<=`
    // boundary — exactly 60s fires again). Concurrent first awards for one
    // member serialize on the row; the loser sees no RETURNING row.
    let cutoff_iso = format_iso_millis(at_ms - AWARD_COOLDOWN_SECONDS as i64 * 1000);
    let claimed: Option<(String,)> = sqlx::query_as(
        "INSERT INTO xp_cooldowns (guild_id, member_id, source, last_awarded_at)
         VALUES ($1, $2, $3, $4::text::timestamptz)
         ON CONFLICT (guild_id, member_id, source) DO UPDATE
           SET last_awarded_at = excluded.last_awarded_at
         WHERE xp_cooldowns.last_awarded_at <= $5::text::timestamptz
         RETURNING last_awarded_at::text",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(source.as_str())
    .bind(&at_norm)
    .bind(cutoff_iso)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        tx.rollback().await?;
        return current_award(pool, guild_id, member_id).await;
    }

    let message_xp = if source == XpSource::Message {
        amount
    } else {
        0
    };
    let voice_xp = amount - message_xp;
    let row: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO member_levels
           (guild_id, member_id, xp, message_xp, voice_xp, imported_xp, updated_at)
         VALUES ($1, $2, $3, $4, $5, 0, $6::text::timestamptz)
         ON CONFLICT (guild_id, member_id) DO UPDATE SET
           xp = member_levels.xp + excluded.xp,
           message_xp = member_levels.message_xp + excluded.message_xp,
           voice_xp = member_levels.voice_xp + excluded.voice_xp,
           updated_at = excluded.updated_at
         WHERE member_levels.xp <= $7
         RETURNING xp",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(amount as i64)
    .bind(message_xp as i64)
    .bind(voice_xp as i64)
    .bind(&at_norm)
    .bind((MAX_STORED_XP - amount) as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((total,)) = row else {
        // Ceiling rejection: roll back so the cooldown claim is not consumed.
        tx.rollback().await?;
        return current_award(pool, guild_id, member_id).await;
    };
    sqlx::query(
        "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at, channel_id)
         VALUES ($1, $2, $3, $4, $5::text::timestamptz, $6)",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(source.as_str())
    .bind(amount as i64)
    .bind(&at_norm)
    .bind(channel_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let total_xp = total as u64;
    // The upsert serializes message/voice awards on member_levels. Derive
    // the previous level from its RETURNING row, not an earlier SELECT that
    // could predate a concurrent award and duplicate a level-up effect.
    let previous_level = level_for_xp(total_xp - amount);
    let level = level_for_xp(total_xp);
    Ok(XpAward {
        awarded: amount,
        total_xp,
        level,
        previous_level,
        leveled_up: level > previous_level,
    })
}

/// Award message XP (legacy `awardMessage`: always [`MESSAGE_XP`]).
pub async fn award_message(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    at_iso: &str,
    channel_id: Option<&str>,
) -> Result<XpAward, LevelingStoreError> {
    award(
        pool,
        guild_id,
        member_id,
        XpSource::Message,
        MESSAGE_XP,
        at_iso,
        channel_id,
    )
    .await
}

/// Award voice XP for a session duration (legacy `awardVoice`: whole minutes
/// × [`VOICE_XP_PER_MINUTE`]; sub-minute sessions award nothing and touch no
/// cooldown row).
pub async fn award_voice(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    duration_seconds: u64,
    at_iso: &str,
    channel_id: Option<&str>,
) -> Result<XpAward, LevelingStoreError> {
    let amount = voice_minutes(duration_seconds) * VOICE_XP_PER_MINUTE;
    award(
        pool,
        guild_id,
        member_id,
        XpSource::Voice,
        amount,
        at_iso,
        channel_id,
    )
    .await
}

/// Current award view with nothing granted (legacy `currentAward`).
pub async fn current_award(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
) -> Result<XpAward, LevelingStoreError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT xp FROM member_levels WHERE guild_id = $1 AND member_id = $2")
            .bind(guild_id)
            .bind(member_id)
            .fetch_optional(pool)
            .await?;
    let total_xp = row.map(|(xp,)| xp as u64).unwrap_or(0);
    let level = level_for_xp(total_xp);
    Ok(XpAward {
        awarded: 0,
        total_xp,
        level,
        previous_level: level,
        leveled_up: false,
    })
}

/// Full read model for `/rank` (legacy `profile`): XP split, 1-based rank
/// with ties broken by member id ascending, member count, next-level floor.
/// A member with no row reads zero XP, ranked below higher-XP members.
///
/// One statement, so XP, rank and member count share a single snapshot: an
/// award committing between separate SELECTs would otherwise compare the new
/// rows against a stale XP bound and rank a sole member 2 of 1 (TOG-10359).
/// Wrapping the three reads in a default READ COMMITTED transaction would
/// not fix that — each statement would still see a fresh snapshot.
pub async fn profile(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
) -> Result<LevelProfile, LevelingStoreError> {
    let (xp, message_xp, voice_xp, imported_xp, rank, member_count): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT
           COALESCE(m.xp, 0),
           COALESCE(m.message_xp, 0),
           COALESCE(m.voice_xp, 0),
           COALESCE(m.imported_xp, 0),
           (SELECT COUNT(*) FROM member_levels
            WHERE guild_id = $1
              AND (xp > COALESCE(m.xp, 0)
                   OR (xp = COALESCE(m.xp, 0) AND member_id < $2))),
           (SELECT COUNT(*) FROM member_levels WHERE guild_id = $1)
         FROM (SELECT 1) AS one
         LEFT JOIN member_levels m
           ON m.guild_id = $1 AND m.member_id = $2",
    )
    .bind(guild_id)
    .bind(member_id)
    .fetch_one(pool)
    .await?;
    let (xp, message_xp, voice_xp, imported_xp) = (
        xp as u64,
        message_xp as u64,
        voice_xp as u64,
        imported_xp as u64,
    );
    let level = level_for_xp(xp);
    Ok(LevelProfile {
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        xp,
        level,
        message_xp,
        voice_xp,
        imported_xp,
        rank: rank as u64 + 1,
        member_count: member_count as u64,
        next_level_xp: super::leveling::total_xp_for_level(level + 1),
    })
}

/// Leaderboard page (legacy `leaderboard`): XP descending, member id
/// ascending, 1-based page ranks. The limit clamps to 1–25
/// ([`clamp_leaderboard_limit`]).
pub async fn leaderboard(
    pool: &PgPool,
    guild_id: &str,
    limit: u64,
) -> Result<Vec<LeaderboardEntry>, LevelingStoreError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT member_id, xp FROM member_levels
         WHERE guild_id = $1 ORDER BY xp DESC, member_id ASC LIMIT $2",
    )
    .bind(guild_id)
    .bind(clamp_leaderboard_limit(limit) as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(index, (member_id, xp))| {
            let xp = xp as u64;
            LeaderboardEntry {
                member_id,
                xp,
                level: level_for_xp(xp),
                rank: index as u64 + 1,
            }
        })
        .collect())
}

/// Stored reward ladder, ascending by level (legacy `roleRewards`).
pub async fn role_rewards(
    pool: &PgPool,
    guild_id: &str,
) -> Result<Vec<LevelRoleReward>, LevelingStoreError> {
    // `level` is INTEGER (INT4) per the legacy DDL; decode as i32.
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT level, role_id FROM level_role_rewards WHERE guild_id = $1 ORDER BY level ASC",
    )
    .bind(guild_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(level, role_id)| LevelRoleReward {
            level: level as u64,
            role_id,
        })
        .collect())
}

/// Replace the full reward configuration atomically (legacy
/// `replaceRoleRewards`): normalize (last row per level wins, ascending),
/// then delete-all + insert in one transaction. Invalid rows fail before any
/// write.
///
/// A transaction alone does not serialize two replacements: with an empty
/// ladder both writers finish `DELETE` before either `INSERT`s, and the
/// commits union two independent configurations (TOG-10359). Row locks
/// cannot cover absent rows, so take a stable per-guild advisory lock first.
/// The cutover import writer (`two-bot-cutover::replace_role_rewards`) takes
/// the same lock with the same key — both crates must keep the strings in
/// sync.
pub async fn replace_role_rewards(
    pool: &PgPool,
    guild_id: &str,
    rewards: &[LevelRoleReward],
) -> Result<(), LevelingStoreError> {
    let normalized = normalize_role_rewards(rewards)?;
    let mut tx = pool.begin().await?;
    // Same namespaced-key convention as `onboarding_store`: a hash collision
    // only serializes unrelated guilds, never merges ladders.
    let lock_key = format!("{guild_id}:level_role_rewards");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM level_role_rewards WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await?;
    for reward in &normalized {
        sqlx::query(
            "INSERT INTO level_role_rewards (guild_id, level, role_id) VALUES ($1, $2, $3)",
        )
        .bind(guild_id)
        .bind(reward.level as i64)
        .bind(&reward.role_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
