//! Member privacy opt-out: atomic purge plus guild-scoped suppression plus
//! exactly one audit receipt, and the per-guild analytics toggles.
//!
//! An opt-out deletes the member's event-store rows for the opting guild
//! only — `events` (with the reviewed `member_erasure` predicate, so rows
//! that merely mention the snowflake in serialized metadata go too),
//! the `members` projection row, `invite_snapshots` rows attributing
//! invites to them, and `community_facts` rows (actor, metadata,
//! source-event and idempotency matches from the same reviewed plan) —
//! then installs the `(guild_id, member_id)` suppression
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
//! lock timeout and a 15-second statement timeout. Opt-out and opt-in share
//! a transaction-scoped advisory lock per pair. An already-suppressed pair
//! skips the purge locks and writes no new receipt; a first opt-out still
//! writes a receipt even when there are no stored rows to purge.

use sqlx::{Postgres, Transaction};

use crate::db::CutoverDb;
use crate::member_erasure::{plan, TableCount};

/// The event-store tables an opt-out purges, in count/delete order.
/// `community_facts` joins the set so an opted-out member's message, voice
/// and join facts leave with the funnel rows; its predicate is the reviewed
/// erasure-plan entry for that table.
const PURGE_TABLES: [&str; 4] = ["events", "members", "invite_snapshots", "community_facts"];

fn valid_identity(guild: &str, member: &str) -> bool {
    crate::is_snowflake(guild)
        && crate::is_snowflake(member)
        && guild.parse::<u64>().is_ok_and(|id| id != 0)
        && member.parse::<u64>().is_ok_and(|id| id != 0)
}

fn valid_actor(actor: &str) -> bool {
    !actor.trim().is_empty() && actor.len() <= 128 && !actor.chars().any(char::is_control)
}

const SUPPRESSED_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)";
const RECENT_OPT_OUT_SQL: &str = "SELECT EXISTS(SELECT 1 FROM privacy_opt_out_audit
      WHERE guild_id = $1 AND member_id = $2
      AND opted_out_at > clock_timestamp() - make_interval(mins => $3))";

/// A decision made under the pair lock, not inferred from purged row counts.
#[derive(Debug)]
pub enum OptOutOutcome {
    OptedOut(Vec<TableCount>),
    AlreadyOptedOut,
    Cooldown,
}

/// Purge the member's event-store rows for the guild, install guild-scoped
/// suppression, and write exactly one audit receipt — all in one
/// transaction. Already-suppressed pairs return zero per-table counts and
/// write no receipt. Operator calls have no cooldown. Never puts the subject
/// ID, counts or content into the audit actor label: `actor` is
/// operator-supplied attribution (nonempty, at most 128 characters, no
/// control characters, never a secret).
pub async fn opt_out_member(
    db: &CutoverDb,
    guild: &str,
    member: &str,
    actor: &str,
) -> Result<Vec<TableCount>, sqlx::Error> {
    match opt_out(db, guild, member, actor, None).await? {
        OptOutOutcome::OptedOut(counts) => Ok(counts),
        OptOutOutcome::AlreadyOptedOut | OptOutOutcome::Cooldown => Ok(PURGE_TABLES
            .iter()
            .map(|table| TableCount {
                table: (*table).to_owned(),
                rows: 0,
            })
            .collect()),
    }
}

/// Member self-serve: the same atomic opt-out, with the existing receipt
/// cooldown checked after suppression, under the same lock as opt-in.
/// Callers must use the explicit outcome to choose their reply: an empty
/// first purge is not an already-opted-out result.
pub async fn opt_out_member_with_cooldown(
    db: &CutoverDb,
    guild: &str,
    member: &str,
    actor: &str,
    cooldown_minutes: i32,
) -> Result<OptOutOutcome, sqlx::Error> {
    if cooldown_minutes <= 0 {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out cooldown".into(),
        ));
    }
    opt_out(db, guild, member, actor, Some(cooldown_minutes)).await
}

async fn begin_member_change<'a>(
    db: &'a CutoverDb,
    guild: &str,
    member: &str,
) -> Result<Transaction<'a, Postgres>, sqlx::Error> {
    let mut tx = db.pool().begin().await?;
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    // Always take this before table/row locks, including when no suppression
    // row exists. Transaction scope releases it on commit, error or cancel.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("privacy:{guild}:{member}"))
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn opt_out(
    db: &CutoverDb,
    guild: &str,
    member: &str,
    actor: &str,
    cooldown_minutes: Option<i32>,
) -> Result<OptOutOutcome, sqlx::Error> {
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
    let mut tx = begin_member_change(db, guild, member).await?;
    let suppressed: bool = sqlx::query_scalar(SUPPRESSED_SQL)
        .bind(guild)
        .bind(member)
        .fetch_one(&mut *tx)
        .await?;
    if suppressed {
        tx.commit().await?;
        return Ok(OptOutOutcome::AlreadyOptedOut);
    }
    if let Some(minutes) = cooldown_minutes {
        let recent: bool = sqlx::query_scalar(RECENT_OPT_OUT_SQL)
            .bind(guild)
            .bind(member)
            .bind(minutes)
            .fetch_one(&mut *tx)
            .await?;
        if recent {
            tx.commit().await?;
            return Ok(OptOutOutcome::Cooldown);
        }
    }
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
    Ok(OptOutOutcome::OptedOut(counts))
}

/// Whether the pair has a purge receipt younger than `cooldown_minutes`.
/// Read-only inspection, not an authorization guard: member self-serve uses
/// `opt_out_member_with_cooldown` to make the decision under the pair lock.
/// Returns false when no receipt exists or the youngest receipt is older
/// than the window. Read-only: no locks taken.
pub async fn has_recent_opt_out(
    db: &CutoverDb,
    guild: &str,
    member: &str,
    cooldown_minutes: i32,
) -> Result<bool, sqlx::Error> {
    if !valid_identity(guild, member) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out identity".into(),
        ));
    }
    if cooldown_minutes <= 0 {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out cooldown".into(),
        ));
    }
    let recent: bool = sqlx::query_scalar(RECENT_OPT_OUT_SQL)
        .bind(guild)
        .bind(member)
        .bind(cooldown_minutes)
        .fetch_one(db.pool())
        .await?;
    Ok(recent)
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
    let mut tx = begin_member_change(db, guild, member).await?;
    let removed =
        sqlx::query("DELETE FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(guild)
            .bind(member)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    tx.commit().await?;
    Ok(removed == 1)
}

/// Whether the member currently suppresses analytics persistence for the
/// guild (a `privacy_opt_outs` row exists for the pair). Read-only: the
/// `/privacy status` self-serve reply uses this, never the raw table.
pub async fn is_opted_out(db: &CutoverDb, guild: &str, member: &str) -> Result<bool, sqlx::Error> {
    if !valid_identity(guild, member) {
        return Err(sqlx::Error::InvalidArgument(
            "invalid opt-out identity".into(),
        ));
    }
    let exists: bool = sqlx::query_scalar(SUPPRESSED_SQL)
        .bind(guild)
        .bind(member)
        .fetch_one(db.pool())
        .await?;
    Ok(exists)
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

    #[tokio::test]
    async fn opt_in_obeys_pair_lock_and_recovers_after_lock_timeout() {
        let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure the privacy pair-lock database test"
                );
                eprintln!("SKIP privacy pair-lock test: TWO_TEST_DATABASE_URL not set");
                return;
            }
        };
        let fixture =
            two_bot_testsupport::TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
                .await
                .expect("disposable migrated database");
        let db = CutoverDb::from_pool(
            fixture
                .independent_pool()
                .await
                .expect("independent service pool"),
        );
        let guild = "1545644954272137297";
        let member = "123456789012345678";
        opt_out_member(&db, guild, member, "operator:pair-lock-test")
            .await
            .expect("commit suppression before taking the external lock");

        // Hold only the advisory lock: a row/table lock could also block the
        // old pool-level DELETE and hide an unserialized opt-in regression.
        // Spell out the key instead of calling the helper under test.
        let mut holder = fixture.pool().begin().await.expect("external transaction");
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("privacy:{guild}:{member}"))
            .execute(&mut *holder)
            .await
            .expect("hold the target pair lock");
        let other_guild = opt_back_in(&db, "1545644954272137298", member).await;
        let other_member = opt_back_in(&db, guild, "123456789012345679").await;
        let blocked = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            opt_back_in(&db, guild, member),
        )
        .await;
        let still_suppressed = is_opted_out(&db, guild, member).await;
        holder.rollback().await.expect("release the external lock");
        let removed = opt_back_in(&db, guild, member).await;
        let already_enabled = opt_back_in(&db, guild, member).await;
        let suppressed_after_release = is_opted_out(&db, guild, member).await;
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM privacy_opt_out_audit")
            .fetch_one(db.pool())
            .await
            .expect("read purge receipts");

        // Tear down before outcome assertions, including on the no-lock mutant.
        db.close().await;
        fixture.close().await.expect("close disposable database");
        assert!(
            matches!(other_guild, Ok(false)),
            "guild is part of the lock key"
        );
        assert!(
            matches!(other_member, Ok(false)),
            "member is part of the lock key"
        );
        assert!(
            matches!(blocked, Ok(Err(sqlx::Error::Database(ref error)))
                if error.code().as_deref() == Some("55P03")),
            "opt-in must fail with lock_timeout while its pair lock is held"
        );
        assert!(
            matches!(still_suppressed, Ok(true)),
            "timeout must not remove suppression"
        );
        assert!(
            matches!(removed, Ok(true)),
            "opt-in succeeds after lock release"
        );
        assert!(
            matches!(already_enabled, Ok(false)),
            "repeat opt-in removes nothing"
        );
        assert!(matches!(suppressed_after_release, Ok(false)));
        assert_eq!(receipts, 1, "opt-in never adds a purge receipt");
    }

    #[tokio::test]
    async fn invalid_cooldown_refuses_before_connecting() {
        // No connection is opened: argument validation must finish first.
        let db = CutoverDb::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy_with(sqlx::postgres::PgConnectOptions::new()),
        );
        for minutes in [0, -1] {
            let result = opt_out_member_with_cooldown(
                &db,
                "1545644954272137297",
                "123456789012345678",
                "member:self-serve",
                minutes,
            )
            .await;
            assert!(matches!(result, Err(sqlx::Error::InvalidArgument(_))));
        }
        db.close().await;
    }

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
