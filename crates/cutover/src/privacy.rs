//! Member privacy opt-out: atomic purge plus guild-scoped suppression plus
//! exactly one audit receipt, and the per-guild analytics toggles.
//!
//! An opt-out deletes the member's event-store rows for the opting guild
//! only — `events` (with the reviewed `member_erasure` predicate, so rows
//! that merely mention the snowflake in serialized metadata go too),
//! the `members` projection row, and `invite_snapshots` rows attributing
//! invites to them — then installs the `(guild_id, member_id)` suppression
//! row that makes every gated write seam drop future per-user persistence
//! for that pair. The transaction also writes exactly one
//! `privacy_opt_out_audit` receipt (scope plus operator actor plus database
//! timestamp). Any count mismatch, suppression failure or receipt failure
//! rolls the whole transaction back, so a purge can never commit without
//! its suppression and receipt.
//!
//! Scope is deliberately narrower than `erase_member`: moderation,
//! automation, audit, XP/level, ticket, voice-room, onboarding and settings
//! rows are untouched. Those tables serve safety, delivery and
//! accountability rather than analytics; wiping them on an analytics
//! opt-out would blind moderation and break delivery receipts. A member who
//! needs full removal uses the erasure path (`docs/privacy.md`); the two
//! control-plane tombstones this service writes (suppression + receipt) are
//! allowlisted there because deleting suppression would silently resume the
//! persistence the member refused.
//!
//! Like `erase_member`, execute takes bounded locks before counting so a
//! concurrent write cannot slip between count and delete, with a 5-second
//! lock timeout and a 15-second statement timeout. Repeating an opt-out is
//! safe: the suppression install is idempotent and each invocation writes
//! its own receipt for the purge it performed (possibly zero rows).

use crate::db::CutoverDb;
use crate::member_erasure::{plan, TableCount};

/// The event-store tables an opt-out purges, in count/delete order.
const PURGE_TABLES: [&str; 3] = ["events", "members", "invite_snapshots"];

fn valid_identity(guild: &str, member: &str) -> bool {
    crate::is_snowflake(guild)
        && crate::is_snowflake(member)
        && guild.parse::<u64>().is_ok_and(|id| id != 0)
        && member.parse::<u64>().is_ok_and(|id| id != 0)
}

fn valid_actor(actor: &str) -> bool {
    !actor.trim().is_empty() && actor.len() <= 128 && !actor.chars().any(char::is_control)
}

/// Purge the member's event-store rows for the guild, install guild-scoped
/// suppression, and write exactly one audit receipt — all in one
/// transaction. Returns the per-table purged counts. Never puts the subject
/// ID, counts or content into the audit actor label: `actor` is
/// operator-supplied attribution (nonempty, at most 128 characters, no
/// control characters, never a secret).
pub async fn opt_out_member(
    db: &CutoverDb,
    guild: &str,
    member: &str,
    actor: &str,
) -> Result<Vec<TableCount>, sqlx::Error> {
    if !valid_identity(guild, member) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out identity".into(),
        ));
    }
    if !valid_actor(actor) {
        return Err(sqlx::Error::InvalidArgument("invalid opt-out actor".into()));
    }
    // The purge reuses the reviewed erasure predicates verbatim so the
    // guild+identity matching cannot drift from the manifest.
    let reviewed = plan();
    let mut predicates = Vec::with_capacity(PURGE_TABLES.len());
    for table in PURGE_TABLES {
        let entry = reviewed.tables.iter().find(|entry| entry.table == table);
        let Some(entry) = entry else {
            return Err(sqlx::Error::InvalidArgument(
                "opt-out purge table missing from erasure plan".into(),
            ));
        };
        predicates.push((table, entry.predicate.clone()));
    }
    let mut tx = db.pool().begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    // One stable ordering for all opt-outs, audit table included so a
    // missing/denied receipt insert cannot lead to an unrecorded commit.
    let mut locked: Vec<&str> = Vec::with_capacity(PURGE_TABLES.len() + 2);
    locked.extend(PURGE_TABLES.iter().copied());
    locked.push("privacy_opt_outs");
    locked.push("privacy_opt_out_audit");
    locked.sort_unstable();
    let lock = format!(
        "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
        locked.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(lock))
        .execute(&mut *tx)
        .await?;
    let mut counts = Vec::with_capacity(predicates.len());
    for (table, predicate) in &predicates {
        let sql = format!("SELECT count(*) FROM {table} WHERE {predicate}");
        let rows = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
            .bind(guild)
            .bind(member)
            .fetch_one(&mut *tx)
            .await?;
        counts.push(TableCount {
            table: (*table).to_owned(),
            rows,
        });
    }
    for ((table, predicate), count) in predicates.iter().zip(&counts) {
        let sql = format!("DELETE FROM {table} WHERE {predicate}");
        let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(guild)
            .bind(member)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if deleted != count.rows as u64 {
            return Err(sqlx::Error::InvalidArgument(
                "opt-out count mismatch".into(),
            ));
        }
    }
    sqlx::query(
        "INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(guild)
    .bind(member)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO privacy_opt_out_audit (guild_id, member_id, actor, opted_out_at)
         VALUES ($1, $2, $3, clock_timestamp())",
    )
    .bind(guild)
    .bind(member)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(counts)
}

/// Remove the guild-scoped suppression row (opt-back-in). Returns true when
/// a row was removed; afterwards the pair persists exactly as before the
/// opt-out. Re-enabling writes no purge receipt: the receipt table records
/// purges, one row per purge, and a re-enable purges nothing.
pub async fn opt_back_in(db: &CutoverDb, guild: &str, member: &str) -> Result<bool, sqlx::Error> {
    if !valid_identity(guild, member) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out identity".into(),
        ));
    }
    let removed =
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(guild)
            .bind(member)
            .execute(db.pool())
            .await?
            .rows_affected();
    Ok(removed == 1)
}

/// The guild's analytics switches as `(presence, activity, voice)`. A guild
/// with no settings row has every category enabled: defaults match the
/// behavior from before the toggles existed.
pub async fn analytics_settings(
    db: &CutoverDb,
    guild: &str,
) -> Result<(bool, bool, bool), sqlx::Error> {
    if !crate::is_snowflake(guild) || guild.parse::<u64>().is_ok_and(|id| id == 0) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid analytics settings identity".into(),
        ));
    }
    let row: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT presence_enabled, activity_enabled, voice_enabled
         FROM guild_analytics_settings WHERE guild_id = $1",
    )
    .bind(guild)
    .fetch_optional(db.pool())
    .await?;
    Ok(row.unwrap_or((true, true, true)))
}

/// Replace the guild's analytics switches atomically. Each category is
/// independent: disabling presence never touches activity or voice.
pub async fn set_analytics_settings(
    db: &CutoverDb,
    guild: &str,
    presence: bool,
    activity: bool,
    voice: bool,
) -> Result<(), sqlx::Error> {
    if !crate::is_snowflake(guild) || guild.parse::<u64>().is_ok_and(|id| id == 0) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid analytics settings identity".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO guild_analytics_settings (guild_id, presence_enabled, activity_enabled, voice_enabled, updated_at)
         VALUES ($1, $2, $3, $4, clock_timestamp())
         ON CONFLICT (guild_id) DO UPDATE SET
           presence_enabled = EXCLUDED.presence_enabled,
           activity_enabled = EXCLUDED.activity_enabled,
           voice_enabled = EXCLUDED.voice_enabled,
           updated_at = clock_timestamp()",
    )
    .bind(guild)
    .bind(presence)
    .bind(activity)
    .bind(voice)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_tables_come_from_the_reviewed_plan() {
        let reviewed = plan();
        for table in PURGE_TABLES {
            let entry = reviewed
                .tables
                .iter()
                .find(|entry| entry.table == table)
                .unwrap_or_else(|| panic!("{table} must stay in the erasure plan"));
            assert!(entry.predicate.contains("$1") && entry.predicate.contains("$2"));
            assert!(!entry.predicate.contains(';'));
        }
    }

    #[test]
    fn identities_and_actors_validate_like_erasure() {
        assert!(valid_identity("1545644954272137297", "123456789012345678"));
        for (guild, member) in [
            ("", "123456789012345678"),
            ("1545644954272137297", ""),
            ("not-a-snowflake", "123456789012345678"),
            ("0", "123456789012345678"),
            ("1545644954272137297", "0"),
        ] {
            assert!(!valid_identity(guild, member), "{guild}/{member}");
        }
        assert!(valid_actor("operator:moderation-review"));
        for actor in ["", "   ", "has\ncontrol", &"x".repeat(129)] {
            assert!(!valid_actor(actor), "{actor:?}");
        }
    }
}
