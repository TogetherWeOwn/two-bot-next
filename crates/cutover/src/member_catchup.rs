//! Member-list catch-up worker slice (TOG-22941, TOG-22852 slice).
//!
//! Slow, resumable catch-up of guild members the bot did not capture live.
//! The wire half rides the shared [`RestClient`]
//! as-is: one `GET /guilds/{id}/members` page per call, so every request
//! passes through the rate-limit client and its `requests()` counter.
//!
//! Privacy fence (hard, same as event-store S1): rows carry ids, timestamps
//! and enums only (`member_id`, `joined_at`, `pending`, `is_bot`). No
//! message bodies, no custom-status text, no payload columns.

use std::collections::HashSet;

use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};

use crate::rest::{RestClient, RestError};

/// One persisted member: ids/timestamps/enums only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemberRow {
    pub member_id: String,
    pub joined_at: Option<String>,
    pub pending: Option<bool>,
    pub is_bot: bool,
}

/// One fetched page: the rows plus the resume cursor.
#[derive(Debug, Clone)]
pub struct MemberCatchupPage {
    pub members: Vec<MemberRow>,
    pub aggregate_count: usize,
    pub next_after: Option<String>,
}

/// What an apply proved: aggregates count everyone, per-user rows skip
/// blocklisted ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyReport {
    pub aggregate_count: usize,
    pub per_user_rows: usize,
}

/// Fetch one member-list page through the shared rate-limit client.
///
/// `limit` is the caller's 1-1000 page size (Discord's bound); `after` is
/// the resume cursor carried verbatim onto the wire (`None` for the first
/// page). Every call issues exactly one wire request through `rest`, so
/// `rest.requests()` agrees with the wire count.
///
/// `next_after` is the last non-empty member id in the raw page, even when
/// that row is skipped for a malformed bot flag. `None` means the page has
/// no non-empty member ids (including the empty end-of-list page).
pub async fn fetch_member_page(
    rest: &RestClient,
    guild_id: Id<GuildMarker>,
    limit: u16,
    after: Option<String>,
) -> Result<MemberCatchupPage, RestError> {
    if !(1..=1000).contains(&limit) {
        return Err(RestError::Wire(
            "member limit out of the 1-1000 contract".to_owned(),
        ));
    }
    let after_id: Option<Id<UserMarker>> = match after {
        Some(cursor) => {
            let raw: u64 = cursor.parse().map_err(|_| {
                RestError::Wire("member after cursor is not a snowflake".to_owned())
            })?;
            Some(Id::new(raw))
        }
        None => None,
    };
    let raw = rest
        .fetch_members_page_json(guild_id, limit, after_id)
        .await?;
    let mut members = Vec::with_capacity(raw.len());
    let mut next_after = None;
    for value in &raw {
        let member_id = value
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or_default()
            .to_owned();
        if member_id.is_empty() {
            continue;
        }
        next_after = Some(member_id.clone());
        let joined_at = value
            .get("joined_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        let pending = value.get("pending").and_then(|v| v.as_bool());
        let is_bot = match value.get("user").and_then(|user| user.get("bot")) {
            // Discord omits this optional field for humans; malformed values
            // must not become persisted human classifications.
            None => false,
            Some(serde_json::Value::Bool(is_bot)) => *is_bot,
            Some(_) => {
                tracing::warn!(reason = "invalid_bot_flag", "member_catchup row skipped");
                continue;
            }
        };
        members.push(MemberRow {
            member_id,
            joined_at,
            pending,
            is_bot,
        });
    }
    let aggregate_count = members.len();
    Ok(MemberCatchupPage {
        members,
        aggregate_count,
        next_after,
    })
}

/// Apply one page: blocklisted ids are skipped for per-user rows while the
/// aggregate count keeps counting them. Idempotent via upsert, so a
/// crash-restart never duplicates rows.
pub async fn apply_member_page(
    pool: &sqlx::PgPool,
    guild_id: &str,
    page: &MemberCatchupPage,
    blocklist: &HashSet<String>,
) -> Result<ApplyReport, sqlx::Error> {
    let mut per_user_rows = 0usize;
    for row in &page.members {
        if blocklist.contains(&row.member_id) {
            continue;
        }
        sqlx::query(
            "INSERT INTO member_catchup_members
             (guild_id, member_id, joined_at, pending, is_bot)
             VALUES ($1, $2, $3::timestamptz, $4, $5)
             ON CONFLICT (guild_id, member_id) DO UPDATE SET
               joined_at = EXCLUDED.joined_at,
               pending = EXCLUDED.pending,
               is_bot = EXCLUDED.is_bot,
               updated_at = now()",
        )
        .bind(guild_id)
        .bind(&row.member_id)
        .bind(&row.joined_at)
        .bind(row.pending)
        .bind(row.is_bot)
        .execute(pool)
        .await?;
        per_user_rows += 1;
    }
    Ok(ApplyReport {
        aggregate_count: page.aggregate_count,
        per_user_rows,
    })
}

/// Persist the resume cursor (the last proven `after`, never zero) and
/// checkpoint the members frontier where the progress route reads it.
///
/// The per-guild cursor above is the worker's resume state; `GET
/// /ops/backfill/progress` reads only `backfill_cursors`, so this also
/// upserts the `members` family there (same SQL shape as
/// `two_bot_store::backfill_cursors::put_backfill_cursor`, inlined so this
/// crate keeps no store dependency). An empty or zero `after` is refused by
/// the tables' CHECKs and surfaces as a database error, never a persisted
/// zero restart.
pub async fn store_member_cursor(
    pool: &sqlx::PgPool,
    guild_id: &str,
    after: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO member_catchup_cursors (guild_id, after_cursor)
         VALUES ($1, $2)
         ON CONFLICT (guild_id) DO UPDATE SET
           after_cursor = EXCLUDED.after_cursor,
           updated_at = now()",
    )
    .bind(guild_id)
    .bind(after)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO backfill_cursors (family, cursor, updated_at)
         VALUES ('members', $1, now())
         ON CONFLICT (family) DO UPDATE
           SET cursor = excluded.cursor, updated_at = now()",
    )
    .bind(after)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the resume cursor, if the guild has checkpointed one.
pub async fn load_member_cursor(
    pool: &sqlx::PgPool,
    guild_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT after_cursor FROM member_catchup_cursors WHERE guild_id = $1")
        .bind(guild_id)
        .fetch_optional(pool)
        .await
}
