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
    adjudicate_message_award, clamp_leaderboard_limit, level_for_xp, normalize_role_rewards,
    season_id_for_ms, seasonal_decay_new_weeks, seasonal_xp_after_decay_weeks,
    voice_xp_for_duration, LeaderboardEntry, LevelProfile, LevelRoleReward, MessageAwardInput,
    RewardConfigError, XpAward, XpSource, AWARD_COOLDOWN_SECONDS, MAX_STORED_XP,
    MESSAGE_DAILY_CAP_XP, MESSAGE_SHORT_MIN_LEN, MESSAGE_XP, VOICE_DAILY_CAP_XP,
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

/// UTC day bounds `[start, end)` in epoch millis for `at_ms`: anti-gaming
/// daily caps are per guild+member+source UTC day (AC2).
fn utc_day_bounds(at_ms: i64) -> (i64, i64) {
    let start = at_ms.div_euclid(86_400_000) * 86_400_000;
    (start, start + 86_400_000)
}

/// Seconds since the last award from one source, if any (AC2 policy input).
/// Future-dated rows (clock skew) read as 0, i.e. cooldown-blocked. Reads
/// epoch millis out of Postgres (`EXTRACT EPOCH`, floored) rather than
/// parsing `timestamptz::text`, whose `"YYYY-MM-DD HH:MM:SS+ZZ"` rendering is
/// not the ISO form [`parse_iso_millis`] accepts.
async fn seconds_since_last_award(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    source: XpSource,
    at_ms: i64,
) -> Result<Option<u64>, LevelingStoreError> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT FLOOR(EXTRACT(EPOCH FROM last_awarded_at) * 1000)::bigint
         FROM xp_cooldowns WHERE guild_id = $1 AND member_id = $2 AND source = $3",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(source.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(last_ms,)| (at_ms - last_ms).max(0) as u64 / 1000))
}

/// Today's (UTC day of the award) granted-award count and XP sum for one
/// source: the daily-counter inputs for the anti-gaming policy (AC2).
/// Derived from the `xp_awards` audit trail on every award, so no second
/// table can drift from it. Zero-XP audit rows inflate the count but never
/// the total, and the ladder is already 0 past ten awards either way.
async fn day_counters(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    source: XpSource,
    day_start_iso: &str,
    day_end_iso: &str,
) -> Result<(u64, u64), LevelingStoreError> {
    let (count, total): (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), SUM(xp) FROM xp_awards
         WHERE guild_id = $1 AND member_id = $2 AND source = $3
           AND occurred_at >= $4::text::timestamptz
           AND occurred_at < $5::text::timestamptz",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(source.as_str())
    .bind(day_start_iso)
    .bind(day_end_iso)
    .fetch_one(pool)
    .await?;
    Ok((count as u64, total.unwrap_or(0) as u64))
}

/// Prior message awards from one channel in the trailing 10 minutes (AC2
/// burst input). `None` channels match only `NULL` rows
/// (`IS NOT DISTINCT FROM`).
async fn same_channel_recent(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    channel_id: Option<&str>,
    window_start_iso: &str,
) -> Result<u64, LevelingStoreError> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM xp_awards
         WHERE guild_id = $1 AND member_id = $2 AND source = 'message'
           AND channel_id IS NOT DISTINCT FROM $3
           AND occurred_at > $4::text::timestamptz",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(channel_id)
    .bind(window_start_iso)
    .fetch_one(pool)
    .await?;
    Ok(count as u64)
}

/// Mirror a granted award into the seasonal projection (AC3): append-only
/// upsert paralleling the lifetime write, same ceiling guard shape. Runs
/// after the lifetime award commits; a crash between the two leaves lifetime
/// ahead, which no later award repairs — accepted for this slice (a backfill
/// owns historical gaps). New activity restarts the decay grace window, so
/// the upsert resets the `decayed_weeks` watermark to 0.
async fn upsert_seasonal(
    pool: &PgPool,
    guild_id: &str,
    season_id: &str,
    member_id: &str,
    amount: u64,
    at_norm: &str,
) -> Result<(), LevelingStoreError> {
    sqlx::query(
        "INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, updated_at)
         VALUES ($1, $2, $3, $4, $5::text::timestamptz)
         ON CONFLICT (guild_id, season_id, member_id) DO UPDATE SET
           seasonal_xp = member_season_xp.seasonal_xp + excluded.seasonal_xp,
           updated_at = excluded.updated_at,
           decayed_weeks = 0
         WHERE member_season_xp.seasonal_xp <= $6",
    )
    .bind(guild_id)
    .bind(season_id)
    .bind(member_id)
    .bind(amount as i64)
    .bind(at_norm)
    .bind((MAX_STORED_XP - amount) as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Leave the decision-marked 0-XP audit row for a policy-capped attempt
/// (AC2): capped attempts are observable — unlike cooldown rejections, which
/// write nothing — without granting XP or consuming extra state (no
/// cooldown, level or projection write).
async fn record_capped_attempt(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    source: XpSource,
    at_norm: &str,
    channel_id: Option<&str>,
    decision: &str,
) -> Result<XpAward, LevelingStoreError> {
    sqlx::query(
        "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at, channel_id, decision)
         VALUES ($1, $2, $3, 0, $4::text::timestamptz, $5, $6)",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(source.as_str())
    .bind(at_norm)
    .bind(channel_id)
    .bind(decision)
    .execute(pool)
    .await?;
    current_award(pool, guild_id, member_id).await
}

/// Award message XP (legacy `awardMessage` base [`MESSAGE_XP`]) through the
/// anti-gaming policy (AC2): the 15/5/0 daily ladder, the 150 XP UTC-day cap
/// and same-channel burst halving adjudicate every attempt via
/// [`adjudicate_message_award`]; granted awards mirror into the seasonal
/// projection (AC3). Lifetime XP, the MEE6 curve and role grants are
/// unchanged; capped attempts leave a decision-marked 0-XP audit row while
/// cooldown and ceiling rejections stay silent like legacy.
///
/// The store carries no message content, so short-message filtering cannot
/// apply here: attempts arrive as normal-length bodies and the content rule
/// lives in the pure policy, where the gateway (which owns content) can
/// supply it.
pub async fn award_message(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    at_iso: &str,
    channel_id: Option<&str>,
) -> Result<XpAward, LevelingStoreError> {
    let at_ms = validate_at(at_iso)?;
    let at_norm = format_iso_millis(at_ms);
    let current = current_award(pool, guild_id, member_id).await?;
    let since =
        seconds_since_last_award(pool, guild_id, member_id, XpSource::Message, at_ms).await?;
    let (day_start, day_end) = utc_day_bounds(at_ms);
    let (day_count, day_total) = day_counters(
        pool,
        guild_id,
        member_id,
        XpSource::Message,
        &format_iso_millis(day_start),
        &format_iso_millis(day_end),
    )
    .await?;
    let channel_recent = same_channel_recent(
        pool,
        guild_id,
        member_id,
        channel_id,
        &format_iso_millis(at_ms - 600_000),
    )
    .await?;
    let policy = adjudicate_message_award(MessageAwardInput {
        previous_xp: current.total_xp,
        daily_award_index: day_count as usize,
        day_total_xp: day_total,
        seconds_since_last_award: since,
        content_len: MESSAGE_SHORT_MIN_LEN,
        has_embed_or_attachment: false,
        same_channel_awards_in_10min: channel_recent as usize,
        headroom: MAX_STORED_XP - current.total_xp,
    });
    if policy.awarded > 0 {
        let out = award(
            pool,
            guild_id,
            member_id,
            XpSource::Message,
            policy.awarded,
            at_iso,
            channel_id,
        )
        .await?;
        if out.awarded > 0 {
            upsert_seasonal(
                pool,
                guild_id,
                &season_id_for_ms(at_ms),
                member_id,
                out.awarded,
                &at_norm,
            )
            .await?;
        }
        return Ok(out);
    }
    // Policy zero: cooldown and ceiling rejections stay silent like legacy
    // (no row, no state); cap and ladder exhaustion leave the
    // decision-marked 0-XP audit row.
    let cooldown_blocked = since.is_some_and(|s| s < AWARD_COOLDOWN_SECONDS);
    let ladder: u64 = if day_count < 5 {
        MESSAGE_XP
    } else if day_count < 10 {
        5
    } else {
        0
    };
    let burst = if channel_recent >= 2 {
        ladder / 2
    } else {
        ladder
    };
    let ceiling = burst > 0 && burst > MAX_STORED_XP - current.total_xp;
    if cooldown_blocked || ceiling {
        return current_award(pool, guild_id, member_id).await;
    }
    // The only other reachable zeros are the day cap and ladder exhaustion
    // (short-message cannot fire without content; halving never zeroes a
    // nonzero ladder rung).
    let decision = if day_total >= MESSAGE_DAILY_CAP_XP {
        "daily_cap"
    } else {
        "daily_ladder_exhausted"
    };
    record_capped_attempt(
        pool,
        guild_id,
        member_id,
        XpSource::Message,
        &at_norm,
        channel_id,
        decision,
    )
    .await
}

/// Award voice XP for a session duration (legacy `awardVoice`: whole minutes
/// × 5 XP per minute; sub-minute sessions award nothing and touch no
/// cooldown row) through the anti-gaming UTC-day cap (AC2, 120 XP):
/// attempts past the cap leave a decision-marked 0-XP audit row, and granted
/// awards mirror into the seasonal projection (AC3).
pub async fn award_voice(
    pool: &PgPool,
    guild_id: &str,
    member_id: &str,
    duration_seconds: u64,
    at_iso: &str,
    channel_id: Option<&str>,
) -> Result<XpAward, LevelingStoreError> {
    let base = voice_xp_for_duration(duration_seconds);
    if base == 0 {
        // Sub-minute sessions award nothing and touch no state (legacy).
        return award(
            pool,
            guild_id,
            member_id,
            XpSource::Voice,
            0,
            at_iso,
            channel_id,
        )
        .await;
    }
    let at_ms = validate_at(at_iso)?;
    let at_norm = format_iso_millis(at_ms);
    let since = seconds_since_last_award(pool, guild_id, member_id, XpSource::Voice, at_ms).await?;
    if since.is_some_and(|s| s < AWARD_COOLDOWN_SECONDS) {
        return current_award(pool, guild_id, member_id).await;
    }
    let (day_start, day_end) = utc_day_bounds(at_ms);
    let (_, day_total) = day_counters(
        pool,
        guild_id,
        member_id,
        XpSource::Voice,
        &format_iso_millis(day_start),
        &format_iso_millis(day_end),
    )
    .await?;
    let remaining = VOICE_DAILY_CAP_XP.saturating_sub(day_total);
    if remaining == 0 {
        return record_capped_attempt(
            pool,
            guild_id,
            member_id,
            XpSource::Voice,
            &at_norm,
            channel_id,
            "daily_cap",
        )
        .await;
    }
    let amount = base.min(remaining);
    let current = current_award(pool, guild_id, member_id).await?;
    if amount > MAX_STORED_XP - current.total_xp {
        // Ceiling rejection stays silent like legacy (no row, no state).
        return current_award(pool, guild_id, member_id).await;
    }
    let out = award(
        pool,
        guild_id,
        member_id,
        XpSource::Voice,
        amount,
        at_iso,
        channel_id,
    )
    .await?;
    if out.awarded > 0 {
        upsert_seasonal(
            pool,
            guild_id,
            &season_id_for_ms(at_ms),
            member_id,
            out.awarded,
            &at_norm,
        )
        .await?;
    }
    Ok(out)
}

/// Monday seasonal-decay tick (AC2): decay every seasonal row in
/// `(guild_id, season_id)` inactive more than 28 days as of `as_of_iso`, 10 %
/// per newly overdue week (multiplicative). The `decayed_weeks` watermark
/// records how many overdue weeks each row has already absorbed: a rerun of
/// the same tick applies zero weeks (no write, no audit row) and consecutive
/// Monday ticks apply exactly one week each, so the cumulative factor stays
/// 0.9^n instead of compounding past decay. Each decayed member gets one
/// `source='decay'` audit row carrying the decayed amount; lifetime XP is
/// never touched. Returns the number of members decayed.
pub async fn apply_seasonal_decay(
    pool: &PgPool,
    guild_id: &str,
    season_id: &str,
    as_of_iso: &str,
) -> Result<u64, LevelingStoreError> {
    let as_of_ms = validate_at(as_of_iso)?;
    let as_of_norm = format_iso_millis(as_of_ms);
    let rows: Vec<(String, i64, i32, i64)> = sqlx::query_as(
        "SELECT member_id, seasonal_xp, decayed_weeks,
                FLOOR(EXTRACT(EPOCH FROM updated_at) * 1000)::bigint
         FROM member_season_xp WHERE guild_id = $1 AND season_id = $2",
    )
    .bind(guild_id)
    .bind(season_id)
    .fetch_all(pool)
    .await?;
    let mut tx = pool.begin().await?;
    let mut decayed = 0u64;
    for (member_id, seasonal_xp, decayed_weeks, updated_ms) in rows {
        let days_inactive = (as_of_ms - updated_ms).max(0) as u64 / 86_400_000;
        let new_weeks = seasonal_decay_new_weeks(days_inactive, decayed_weeks.max(0) as u64);
        if new_weeks == 0 {
            continue;
        }
        let next = seasonal_xp_after_decay_weeks(seasonal_xp as u64, new_weeks);
        let lost = (seasonal_xp as u64).saturating_sub(next);
        if lost == 0 {
            continue;
        }
        let total_weeks =
            i32::try_from(decayed_weeks.max(0) as u64 + new_weeks).unwrap_or(i32::MAX);
        sqlx::query(
            "UPDATE member_season_xp SET seasonal_xp = $3, decayed_weeks = $4
             WHERE guild_id = $1 AND season_id = $2 AND member_id = $5",
        )
        .bind(guild_id)
        .bind(season_id)
        .bind(next as i64)
        .bind(total_weeks)
        .bind(&member_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO xp_awards (guild_id, member_id, source, xp, occurred_at, channel_id, decision)
             VALUES ($1, $2, 'decay', $3, $4::text::timestamptz, NULL, 'seasonal_decay')",
        )
        .bind(guild_id)
        .bind(&member_id)
        .bind(lost as i64)
        .bind(&as_of_norm)
        .execute(&mut *tx)
        .await?;
        decayed += 1;
    }
    tx.commit().await?;
    Ok(decayed)
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
/// A member with no row reads zero XP and no rank; stored zero-XP rows rank.
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
        Option<i64>,
        i64,
    ) = sqlx::query_as(
        "SELECT
           COALESCE(m.xp, 0),
           COALESCE(m.message_xp, 0),
           COALESCE(m.voice_xp, 0),
           COALESCE(m.imported_xp, 0),
           CASE WHEN m.member_id IS NULL THEN NULL
                ELSE (SELECT COUNT(*) + 1 FROM member_levels
                      WHERE guild_id = $1
                        AND (xp > m.xp OR (xp = m.xp AND member_id < $2)))
           END,
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
        rank: rank.map(|rank| rank as u64),
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
