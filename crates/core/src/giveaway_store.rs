//! sqlx store for giveaways: definitions, entries, claim-lease draws.
//!
//! Slice 1 of the giveaways design (timed entry, fair draw, reroll), behind
//! the crate's `db` feature so pure-domain unit tests never need a driver.
//! Validation, eligibility and the draw itself live in [`crate::giveaway`];
//! this module only moves rows. Tables:
//! `crates/cutover/migrations/0460_giveaways.sql` (fixtures run this chain)
//! and the `crates/store/migrations/0461_giveaways.sql` copy the boot runner
//! applies under its own ledger.
//!
//! Concurrency contract (the design's §6 restart recovery): `claim_due`
//! leases at most one due giveaway with a single atomic `UPDATE …
//! RETURNING`, `FOR UPDATE SKIP LOCKED` letting parallel tickers divide the
//! queue without blocking. A claimed row carries `claim_token`/`claimed_at`;
//! only the claim holder may `mark_drawn` or `release_claim`, and an expired
//! lease is reclaimable by passing an older `reclaim_before` cutoff. The
//! draw row's PK (`guild_id`, `giveaway_id`, `draw_no`) makes a second draw
//! of the same round impossible even if two workers race past the lease:
//! the loser's `insert_draw` fails with a 23505 unique violation. Entry
//! writes are `ON CONFLICT DO NOTHING`, so a double-click keeps the first
//! `entered_at` and reports `inserted = false`.

use sqlx::types::time::OffsetDateTime;
use sqlx::{Pool, Postgres};

/// One `giveaways` row: the definition plus the draw-ticker lease columns.
/// `status` is the [`crate::giveaway::GiveawayStatus`] wire value; transitions
/// are validated by [`crate::giveaway::transition`] before
/// [`set_giveaway_status`] persists them.
///
/// `Debug` is manual: `draw_nonce` predicts every winner of an open giveaway
/// and `claim_token` is the draw-ticker lease capability, so both stay out
/// of diagnostics while every other field remains available.
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct GiveawayRow {
    pub guild_id: String,
    pub id: String,
    pub prize: String,
    pub channel_id: String,
    pub message_id: String,
    pub created_by: String,
    pub winners_count: i32,
    pub ends_at: OffsetDateTime,
    pub status: String,
    pub requirements_json: serde_json::Value,
    pub max_entries: i32,
    pub draw_nonce: String,
    pub claim_token: Option<String>,
    pub claimed_at: Option<OffsetDateTime>,
    pub announced_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl std::fmt::Debug for GiveawayRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiveawayRow")
            .field("guild_id", &self.guild_id)
            .field("id", &self.id)
            .field("prize", &self.prize)
            .field("channel_id", &self.channel_id)
            .field("message_id", &self.message_id)
            .field("created_by", &self.created_by)
            .field("winners_count", &self.winners_count)
            .field("ends_at", &self.ends_at)
            .field("status", &self.status)
            .field("requirements_json", &self.requirements_json)
            .field("max_entries", &self.max_entries)
            .field("draw_nonce", &"[REDACTED]")
            .field(
                "claim_token",
                &self.claim_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("claimed_at", &self.claimed_at)
            .field("announced_at", &self.announced_at)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// One `giveaway_entries` row. `weight` is reserved for bonus entries and is
/// always 1 until a later slice draws weighted lots.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct GiveawayEntryRow {
    pub guild_id: String,
    pub giveaway_id: String,
    pub user_id: String,
    pub weight: i32,
    pub entered_at: OffsetDateTime,
}

/// One `giveaway_draws` row: the committed replay inputs (`eligible_hash`,
/// `seed`) plus the winners in draw order. `reroll_kind` is NULL for the
/// first draw, `full` or `single:<user id>` for rerolls.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct GiveawayDrawRow {
    pub guild_id: String,
    pub giveaway_id: String,
    pub draw_no: i32,
    pub drawn_at: OffsetDateTime,
    pub eligible_count: i32,
    pub eligible_hash: String,
    pub seed: String,
    pub winner_ids: serde_json::Value,
    pub reroll_kind: Option<String>,
}

/// A new giveaway definition. The caller validates prize, winner count and
/// duration with [`crate::giveaway::validate_new_giveaway`], mints
/// `draw_nonce` with [`crate::giveaway::mint_draw_nonce`], and passes
/// `created_at` for both timestamps; the row starts `open` with no claim.
///
/// `Debug` is manual for the same reason as [`GiveawayRow`]: the pre-draw
/// nonce must never reach diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct NewGiveaway {
    pub guild_id: String,
    pub id: String,
    pub prize: String,
    pub channel_id: String,
    pub message_id: String,
    pub created_by: String,
    pub winners_count: i32,
    pub ends_at: OffsetDateTime,
    pub requirements_json: serde_json::Value,
    pub max_entries: i32,
    pub draw_nonce: String,
    pub created_at: OffsetDateTime,
}

impl std::fmt::Debug for NewGiveaway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewGiveaway")
            .field("guild_id", &self.guild_id)
            .field("id", &self.id)
            .field("prize", &self.prize)
            .field("channel_id", &self.channel_id)
            .field("message_id", &self.message_id)
            .field("created_by", &self.created_by)
            .field("winners_count", &self.winners_count)
            .field("ends_at", &self.ends_at)
            .field("requirements_json", &self.requirements_json)
            .field("max_entries", &self.max_entries)
            .field("draw_nonce", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A new draw row. The caller derives `seed` with
/// [`crate::giveaway::draw_seed`], commits the list with
/// [`crate::giveaway::eligible_hash`], selects winners with
/// [`crate::giveaway::draw`], and passes `draw_no + 1` past
/// [`latest_draw_no`] for rerolls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDraw {
    pub guild_id: String,
    pub giveaway_id: String,
    pub draw_no: i32,
    pub drawn_at: OffsetDateTime,
    pub eligible_count: i32,
    pub eligible_hash: String,
    pub seed: String,
    pub winner_ids: serde_json::Value,
    pub reroll_kind: Option<String>,
}

/// Insert one giveaway definition, returning the stored row.
pub async fn create_giveaway<'e>(
    pool: impl sqlx::Executor<'e, Database = Postgres>,
    input: &NewGiveaway,
) -> Result<GiveawayRow, sqlx::Error> {
    sqlx::query_as::<_, GiveawayRow>(
        "INSERT INTO giveaways
           (guild_id, id, prize, channel_id, message_id, created_by, winners_count,
            ends_at, status, requirements_json, max_entries, draw_nonce,
            created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'open', $9, $10, $11, $12, $12)
         RETURNING guild_id, id, prize, channel_id, message_id, created_by,
                   winners_count, ends_at, status, requirements_json, max_entries,
                   draw_nonce, claim_token, claimed_at, announced_at, created_at,
                   updated_at",
    )
    .bind(&input.guild_id)
    .bind(&input.id)
    .bind(&input.prize)
    .bind(&input.channel_id)
    .bind(&input.message_id)
    .bind(&input.created_by)
    .bind(input.winners_count)
    .bind(input.ends_at)
    .bind(&input.requirements_json)
    .bind(input.max_entries)
    .bind(&input.draw_nonce)
    .bind(input.created_at)
    .fetch_one(pool)
    .await
}

/// Read one giveaway definition, if it exists.
pub async fn get_giveaway(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
) -> Result<Option<GiveawayRow>, sqlx::Error> {
    sqlx::query_as::<_, GiveawayRow>(
        "SELECT guild_id, id, prize, channel_id, message_id, created_by, winners_count,
                ends_at, status, requirements_json, max_entries, draw_nonce,
                claim_token, claimed_at, announced_at, created_at, updated_at
           FROM giveaways WHERE guild_id = $1 AND id = $2",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .fetch_optional(pool)
    .await
}

/// Persist a lifecycle step the caller validated with
/// [`crate::giveaway::transition`]. Compare-and-set on `expected`: returns
/// `false` when the giveaway is unknown or no longer in the status the
/// caller transitioned from, so two racing lifecycle writers cannot
/// overwrite each other.
pub async fn set_giveaway_status(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
    status: &str,
    updated_at: OffsetDateTime,
    expected: &str,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE giveaways SET status = $3, updated_at = $4
          WHERE guild_id = $1 AND id = $2 AND status = $5",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(status)
    .bind(updated_at)
    .bind(expected)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Hard-delete a giveaway with its entries and draws, atomically. Returns
/// `false` when the giveaway is unknown (nothing is deleted then). Children
/// go first explicitly; the schema's composite foreign keys refuse a parent
/// delete while live children exist, so an entry or draw racing the delete
/// fails the whole transaction closed instead of being orphaned.
pub async fn delete_giveaway(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2")
        .bind(guild_id)
        .bind(giveaway_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2")
        .bind(guild_id)
        .bind(giveaway_id)
        .execute(&mut *tx)
        .await?;
    let deleted = sqlx::query("DELETE FROM giveaways WHERE guild_id = $1 AND id = $2")
        .bind(guild_id)
        .bind(giveaway_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(deleted == 1)
}

/// Record one entry, collapsing double-clicks on the entry PK. Returns
/// `true` on a first entry, `false` when the member already holds a row
/// (the original `entered_at` is kept).
pub async fn upsert_entry(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
    user_id: &str,
    entered_at: OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at)
         VALUES ($1, $2, $3, 1, $4)
         ON CONFLICT (guild_id, giveaway_id, user_id) DO NOTHING",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(user_id)
    .bind(entered_at)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// List every entry of a giveaway in entry order (ties break on user id, so
/// the order is stable). The draw re-sorts by user id before selecting.
pub async fn list_entries(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
) -> Result<Vec<GiveawayEntryRow>, sqlx::Error> {
    sqlx::query_as::<_, GiveawayEntryRow>(
        "SELECT guild_id, giveaway_id, user_id, weight, entered_at
           FROM giveaway_entries
          WHERE guild_id = $1 AND giveaway_id = $2
          ORDER BY entered_at, user_id",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .fetch_all(pool)
    .await
}

/// Atomically lease at most one due giveaway for the draw ticker.
/// `SKIP LOCKED` lets a second ticker take another row instead of blocking.
/// Due means `open` with `ends_at <= now`, unclaimed or holding an expired
/// lease (`claimed_at <= reclaim_before`); the claim stamps a fresh token
/// and `claimed_at = now`. Returns the leased row, or `None` when nothing is
/// due.
pub async fn claim_due_giveaway(
    pool: &Pool<Postgres>,
    guild_id: &str,
    now: OffsetDateTime,
    reclaim_before: OffsetDateTime,
    claim_token: &str,
) -> Result<Option<GiveawayRow>, sqlx::Error> {
    sqlx::query_as::<_, GiveawayRow>(
        "WITH due AS (
           SELECT guild_id, id FROM giveaways
            WHERE guild_id = $1 AND status = 'open' AND ends_at <= $2
              AND (claim_token IS NULL OR claimed_at <= $3)
            ORDER BY ends_at, id
            LIMIT 1
            FOR UPDATE SKIP LOCKED
         )
         UPDATE giveaways
            SET claim_token = $4, claimed_at = $2, updated_at = $2
          WHERE (guild_id, id) IN (SELECT guild_id, id FROM due)
            AND status = 'open' AND ends_at <= $2
         RETURNING guild_id, id, prize, channel_id, message_id, created_by,
                   winners_count, ends_at, status, requirements_json, max_entries,
                   draw_nonce, claim_token, claimed_at, announced_at, created_at,
                   updated_at",
    )
    .bind(guild_id)
    .bind(now)
    .bind(reclaim_before)
    .bind(claim_token)
    .fetch_optional(pool)
    .await
}

/// Release a claim after a retryable failure, so the next tick re-claims the
/// row. Only the claim holder clears the lease; returns `false` when the
/// token does not match (the giveaway is unknown, unclaimed, or re-leased).
/// Stamps `updated_at` like every other `giveaways` mutation, so the
/// rollback-delta recency measure keeps seeing the row's last write.
pub async fn release_claim(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
    claim_token: &str,
    updated_at: OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let released = sqlx::query(
        "UPDATE giveaways SET claim_token = NULL, claimed_at = NULL, updated_at = $4
          WHERE guild_id = $1 AND id = $2 AND claim_token = $3",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(claim_token)
    .bind(updated_at)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(released == 1)
}

/// Complete the draw phase: flip a claimed giveaway to `drawn` and release
/// the claim. Only the claim holder completes, and only while the giveaway
/// is still `open`: a late completion after a moderator paused or cancelled
/// the row returns `false` instead of resurrecting it as drawn. Prefer
/// [`complete_draw`], which inserts the draw row and runs this guarded
/// transition in one transaction; this stays as the guarded-transition
/// primitive for callers that already hold a draw row.
pub async fn mark_drawn(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
    claim_token: &str,
    updated_at: OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let completed = sqlx::query(
        "UPDATE giveaways
            SET status = 'drawn', claim_token = NULL, claimed_at = NULL, updated_at = $4
          WHERE guild_id = $1 AND id = $2 AND claim_token = $3 AND status = 'open'",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(claim_token)
    .bind(updated_at)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(completed == 1)
}

/// Complete the draw phase atomically: insert the draw row and flip the
/// claimed giveaway to `drawn` in one transaction, releasing the claim.
/// Returns the stored row on success, or `None` when the claim token is
/// stale or the giveaway is no longer `open`: the insert rolls back, so a
/// superseded worker or a moderator cancel/pause between claim and
/// completion leaves no orphan draw row behind. A racing worker's 23505 on
/// the draw PK still propagates as `Err`.
pub async fn complete_draw(
    pool: &Pool<Postgres>,
    input: &NewDraw,
    claim_token: &str,
    updated_at: OffsetDateTime,
) -> Result<Option<GiveawayDrawRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as::<_, GiveawayDrawRow>(
        "INSERT INTO giveaway_draws
           (guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
            eligible_hash, seed, winner_ids, reroll_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
                   eligible_hash, seed, winner_ids, reroll_kind",
    )
    .bind(&input.guild_id)
    .bind(&input.giveaway_id)
    .bind(input.draw_no)
    .bind(input.drawn_at)
    .bind(input.eligible_count)
    .bind(&input.eligible_hash)
    .bind(&input.seed)
    .bind(&input.winner_ids)
    .bind(input.reroll_kind.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    let completed = sqlx::query(
        "UPDATE giveaways
            SET status = 'drawn', claim_token = NULL, claimed_at = NULL, updated_at = $4
          WHERE guild_id = $1 AND id = $2 AND claim_token = $3 AND status = 'open'",
    )
    .bind(&input.guild_id)
    .bind(&input.giveaway_id)
    .bind(claim_token)
    .bind(updated_at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if completed == 1 {
        tx.commit().await?;
        Ok(Some(row))
    } else {
        tx.rollback().await?;
        Ok(None)
    }
}

/// Mark the announcement posted. Only a `drawn` row that still lacks a
/// stamp advances: NULL `announced_at` on a drawn row is the only signal an
/// announcement is still owed after a restart, so an early call on another
/// status or a second announcer returns `false` instead of hiding or
/// overwriting the marker.
pub async fn mark_announced(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
    announced_at: OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let marked = sqlx::query(
        "UPDATE giveaways SET announced_at = $3, updated_at = $3
          WHERE guild_id = $1 AND id = $2 AND status = 'drawn' AND announced_at IS NULL",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .bind(announced_at)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(marked == 1)
}

/// Insert one draw row, returning it. A plain insert, so two workers racing
/// the same round collide on the draw PK and the loser fails with a 23505
/// unique violation instead of double-drawing.
pub async fn insert_draw(
    pool: &Pool<Postgres>,
    input: &NewDraw,
) -> Result<GiveawayDrawRow, sqlx::Error> {
    sqlx::query_as::<_, GiveawayDrawRow>(
        "INSERT INTO giveaway_draws
           (guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
            eligible_hash, seed, winner_ids, reroll_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
                   eligible_hash, seed, winner_ids, reroll_kind",
    )
    .bind(&input.guild_id)
    .bind(&input.giveaway_id)
    .bind(input.draw_no)
    .bind(input.drawn_at)
    .bind(input.eligible_count)
    .bind(&input.eligible_hash)
    .bind(&input.seed)
    .bind(&input.winner_ids)
    .bind(input.reroll_kind.as_deref())
    .fetch_one(pool)
    .await
}

/// List every draw row of a giveaway in round order. Rerolls append rows;
/// the first draw row is never updated.
pub async fn get_draws(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
) -> Result<Vec<GiveawayDrawRow>, sqlx::Error> {
    sqlx::query_as::<_, GiveawayDrawRow>(
        "SELECT guild_id, giveaway_id, draw_no, drawn_at, eligible_count,
                eligible_hash, seed, winner_ids, reroll_kind
           FROM giveaway_draws
          WHERE guild_id = $1 AND giveaway_id = $2
          ORDER BY draw_no",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .fetch_all(pool)
    .await
}

/// The latest stored round number, or `None` before the first draw. A reroll
/// inserts the successor of this number.
pub async fn latest_draw_no(
    pool: &Pool<Postgres>,
    guild_id: &str,
    giveaway_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT MAX(draw_no) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(guild_id)
    .bind(giveaway_id)
    .fetch_one(pool)
    .await
}

/// Bound active definitions and every list/ticker batch for one guild.
pub const MAX_OPEN_GIVEAWAYS: i64 = 25;
/// Maximum number of distinct members admitted to one giveaway.
pub const MAX_GIVEAWAY_ENTRIES: i32 = 10_000;

/// Create and audit atomically, under a guild-scoped quota lock. An interaction
/// id used as the opaque id also makes duplicate create deliveries harmless.
pub async fn create_admitted(
    pool: &Pool<Postgres>,
    input: &NewGiveaway,
) -> Result<Option<GiveawayRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("giveaway-quota:{}", input.guild_id))
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaways WHERE guild_id = $1 AND status IN ('open', 'paused', 'drawing')",
    ).bind(&input.guild_id).fetch_one(&mut *tx).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM giveaways WHERE guild_id = $1 AND id = $2)",
    )
    .bind(&input.guild_id)
    .bind(&input.id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_OPEN_GIVEAWAYS || exists {
        return Ok(None);
    }
    let row = create_giveaway(&mut *tx, input).await?;
    surface_audit(
        &mut tx,
        &row,
        Some(&input.created_by),
        "giveaway.create",
        "ok",
    )
    .await?;
    tx.commit().await?;
    Ok(Some(row))
}

/// Attach a confirmed message once. Pending/uncertain sends are never retried
/// automatically, so a lost acknowledgement cannot create duplicate messages.
pub async fn attach_message(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("UPDATE giveaways SET message_id = $3, updated_at = now() WHERE guild_id = $1 AND id = $2 AND message_id = 'pending' AND status = 'open'")
        .bind(guild_id).bind(id).bind(message_id).execute(pool).await?.rows_affected() == 1)
}

/// Bounded active list, shared by the slash command and countdown supervisor.
pub async fn list_active(
    pool: &Pool<Postgres>,
    guild_id: &str,
) -> Result<Vec<GiveawayRow>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM giveaways WHERE guild_id = $1 AND status IN ('open', 'paused') ORDER BY ends_at, id LIMIT $2")
        .bind(guild_id).bind(MAX_OPEN_GIVEAWAYS).fetch_all(pool).await
}

/// Admission result; duplicate clicks are not writes and never refresh counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryAdmission {
    Entered,
    AlreadyEntered,
    Closed,
    Full,
    Ineligible,
    RateLimited,
}

/// Status, deadline, capacity, eligibility and uniqueness share the parent row
/// lock. Cancel and draw claims take the same lock via UPDATE, so no admission
/// can race past a lifecycle change or oversubscribe the final place.
pub async fn admit_entry(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    user_id: &str,
    standing: &crate::giveaway::MemberStanding,
    now: OffsetDateTime,
) -> Result<EntryAdmission, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<GiveawayRow> =
        sqlx::query_as("SELECT * FROM giveaways WHERE guild_id = $1 AND id = $2 FOR UPDATE")
            .bind(guild_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(row) = row else {
        return Ok(EntryAdmission::Closed);
    };
    if row.status != "open"
        || row.ends_at <= now
        || row.claim_token.is_some()
        || row.message_id == "pending"
    {
        return Ok(EntryAdmission::Closed);
    }
    // Role ids originate as typed, nonzero Discord Role options. Deserialize
    // the exact stored schema; never trust component-supplied requirements.
    let requirements: crate::giveaway::GiveawayRequirements =
        serde_json::from_value(row.requirements_json.clone())
            .map_err(|_| sqlx::Error::Protocol("invalid giveaway requirements".into()))?;
    if requirements.min_account_age_days > crate::giveaway::REQUIREMENT_MAX_AGE_DAYS
        || requirements.min_guild_age_days > crate::giveaway::REQUIREMENT_MAX_AGE_DAYS
        || requirements
            .roles
            .iter()
            .any(|id| id.parse::<u64>().is_err() || id == "0")
    {
        return Ok(EntryAdmission::Ineligible);
    }
    if crate::giveaway::adjudicate_eligibility(&requirements, standing)
        != crate::giveaway::Eligibility::Eligible
    {
        return Ok(EntryAdmission::Ineligible);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2 AND user_id = $3)")
        .bind(guild_id).bind(id).bind(user_id).fetch_one(&mut *tx).await?;
    if exists {
        return Ok(EntryAdmission::AlreadyEntered);
    }
    // A second, durable budget fences independent runtimes and restarts. Take
    // it after the giveaway lock consistently; no path takes these in reverse.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("giveaway-entry:{guild_id}:{user_id}"))
        .execute(&mut *tx)
        .await?;
    let writes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND user_id = $2 AND entered_at > $3")
        .bind(guild_id).bind(user_id).bind(now - time::Duration::minutes(1)).fetch_one(&mut *tx).await?;
    if writes >= 10 {
        return Ok(EntryAdmission::RateLimited);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(guild_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if count >= i64::from(row.max_entries) {
        return Ok(EntryAdmission::Full);
    }
    sqlx::query("INSERT INTO giveaway_entries (guild_id, giveaway_id, user_id, weight, entered_at) VALUES ($1, $2, $3, 1, $4) ON CONFLICT DO NOTHING")
        .bind(guild_id).bind(id).bind(user_id).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(EntryAdmission::Entered)
}

/// Count without loading the entrant identities into the request path.
pub async fn entry_count(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(guild_id)
    .bind(id)
    .fetch_one(pool)
    .await
}

/// Cancel and its audit commit together, exactly once for a live definition.
pub async fn cancel_audited(
    pool: &Pool<Postgres>,
    guild_id: &str,
    id: &str,
    actor: &str,
) -> Result<Option<GiveawayRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<GiveawayRow> = sqlx::query_as("UPDATE giveaways SET status = 'cancelled', claim_token = NULL, claimed_at = NULL, updated_at = now() WHERE guild_id = $1 AND id = $2 AND status IN ('open', 'paused') RETURNING *")
        .bind(guild_id).bind(id).fetch_optional(&mut *tx).await?;
    if let Some(row) = &row {
        surface_audit(&mut tx, row, Some(actor), "giveaway.cancel", "ok").await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// One durable marker per countdown bucket. Claim before REST: a crash or
/// rejected edit may miss a cosmetic refresh, but never causes an edit storm.
pub async fn claim_refresh(
    pool: &Pool<Postgres>,
    row: &GiveawayRow,
    threshold: i64,
) -> Result<bool, sqlx::Error> {
    let id = format!("giveaway.refresh:{}:{}:{threshold}", row.guild_id, row.id);
    Ok(sqlx::query("INSERT INTO automation_audit_log (id, guild_id, action, target_key, outcome, created_at) VALUES ($1, $2, 'giveaway.refresh', $3, 'claimed', $4) ON CONFLICT (id) DO NOTHING")
        .bind(id).bind(&row.guild_id).bind(&row.id).bind(OffsetDateTime::now_utc())
        .execute(pool).await?.rows_affected() == 1)
}

async fn surface_audit(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    row: &GiveawayRow,
    actor: Option<&str>,
    action: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(format!("{action}:{}:{}", row.guild_id, row.id)).bind(&row.guild_id).bind(actor)
        .bind(action).bind(&row.id).bind(outcome).bind(row.updated_at).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "unit-draw-nonce-must-not-leak";
    const TOKEN: &str = "unit-claim-token-must-not-leak";

    fn row() -> GiveawayRow {
        GiveawayRow {
            guild_id: "1545644954272137297".to_owned(),
            id: "gw01h455k".to_owned(),
            prize: "unit-prize".to_owned(),
            channel_id: "221813744719470592".to_owned(),
            message_id: "221813744719470593".to_owned(),
            created_by: "123456789012345678".to_owned(),
            winners_count: 1,
            ends_at: OffsetDateTime::UNIX_EPOCH,
            status: "open".to_owned(),
            requirements_json: serde_json::json!({}),
            max_entries: 1,
            draw_nonce: NONCE.to_owned(),
            claim_token: Some(TOKEN.to_owned()),
            claimed_at: None,
            announced_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn input() -> NewGiveaway {
        NewGiveaway {
            guild_id: "1545644954272137297".to_owned(),
            id: "gw01h455k".to_owned(),
            prize: "unit-prize".to_owned(),
            channel_id: "221813744719470592".to_owned(),
            message_id: "221813744719470593".to_owned(),
            created_by: "123456789012345678".to_owned(),
            winners_count: 1,
            ends_at: OffsetDateTime::UNIX_EPOCH,
            requirements_json: serde_json::json!({}),
            max_entries: 1,
            draw_nonce: NONCE.to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn debug_redacts_row_secrets_in_both_formats() {
        let rendered = format!("{:?}\n{:#?}", row(), row());
        assert!(!rendered.contains(NONCE), "draw nonce leaked: {rendered}");
        assert!(!rendered.contains(TOKEN), "claim token leaked: {rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(rendered.contains("unit-prize"), "{rendered}");
    }

    #[test]
    fn debug_redacts_input_nonce_in_both_formats() {
        let rendered = format!("{:?}\n{:#?}", input(), input());
        assert!(!rendered.contains(NONCE), "draw nonce leaked: {rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(rendered.contains("unit-prize"), "{rendered}");
    }
}
