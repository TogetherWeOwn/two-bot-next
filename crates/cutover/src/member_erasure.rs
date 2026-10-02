//! Guild-scoped erasure plan shared by the CLI and schema coverage tests.
//!
//! The embedded plan is reviewed code, never operator input. Count and delete
//! use the same predicate; a row matching several identity columns counts once.

use serde::Deserialize;
use sqlx::{PgConnection, PgPool};

#[derive(Debug, Deserialize)]
pub struct ErasurePlan {
    pub tables: Vec<TablePlan>,
    pub allowlist: Vec<ColumnException>,
}

#[derive(Debug, Deserialize)]
pub struct TablePlan {
    pub table: String,
    pub columns: Vec<String>,
    /// Constant SQL with $1 = guild, $2 = member. Includes indirect scope for
    /// child tables that have no guild_id of their own.
    pub predicate: String,
}

#[derive(Debug, Deserialize)]
pub struct ColumnException {
    pub table: String,
    pub column: String,
    pub reason: String,
}

#[must_use]
pub fn plan() -> ErasurePlan {
    serde_json::from_str(include_str!("member_erasure_plan.json"))
        .expect("embedded erasure plan must be valid JSON")
}

#[derive(Debug, Clone, Copy)]
pub enum ErasureMode<'a> {
    DryRun,
    Execute { actor: &'a str },
}

#[derive(Debug, PartialEq, Eq)]
pub struct TableCount {
    pub table: String,
    pub rows: i64,
}

/// Naming tripwire for identity columns, including legacy aliases. Semantic
/// review is still needed for arbitrary new names or identities inside JSON.
#[must_use]
pub fn is_identity_column(column: &str) -> bool {
    column == "user_id"
        || column.ends_with("_user_id")
        || column == "member_id"
        || column.ends_with("_member_id")
        || column.ends_with("_by")
        || column.ends_with("_author_id")
        || matches!(
            column,
            "actor"
                | "actor_id"
                | "author_id"
                | "owner_id"
                | "creator_id"
                | "inviter_id"
                | "moderator_id"
                | "target_id"
                | "subject_id"
                | "claimant_id"
                | "assignee_id"
                | "opener_id"
                | "requester_id"
        )
}

/// Check the actual migrated schema, not migration-text grep. Also refuse a
/// stale plan/exception (renamed or missing columns) before any deletion.
pub async fn schema_gaps(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, column_name FROM information_schema.columns
         WHERE table_schema = current_schema() ORDER BY table_name, ordinal_position",
    )
    .fetch_all(conn)
    .await?;
    let plan = plan();
    let mut gaps = Vec::new();
    for (table, column) in &columns {
        if is_identity_column(column)
            && !plan
                .tables
                .iter()
                .any(|entry| entry.table == *table && entry.columns.contains(column))
            && !plan.allowlist.iter().any(|entry| {
                entry.table == *table && entry.column == *column && !entry.reason.trim().is_empty()
            })
        {
            gaps.push(format!("uncovered identity: {table}.{column}"));
        }
    }
    for entry in &plan.tables {
        for column in &entry.columns {
            if !columns.contains(&(entry.table.clone(), column.clone())) {
                gaps.push(format!("stale plan: {}.{column}", entry.table));
            }
        }
    }
    for entry in &plan.allowlist {
        if entry.reason.trim().is_empty()
            || !columns.contains(&(entry.table.clone(), entry.column.clone()))
        {
            gaps.push(format!(
                "stale/undocumented exception: {}.{}",
                entry.table, entry.column
            ));
        }
    }
    Ok(gaps)
}

/// Execute takes bounded locks before counting so concurrent inserts/updates
/// cannot slip between count and delete. Child tables precede parents in the
/// plan; execute rejects any count mismatch and rolls back, including audit.
/// Dry run is one read-only snapshot and emits no audit row.
pub async fn erase_member(
    pool: &PgPool,
    guild: &str,
    member: &str,
    mode: ErasureMode<'_>,
) -> Result<Vec<TableCount>, sqlx::Error> {
    if !crate::is_snowflake(guild)
        || !crate::is_snowflake(member)
        || guild.parse::<u64>().map_or(true, |id| id == 0)
        || member.parse::<u64>().map_or(true, |id| id == 0)
    {
        return Err(sqlx::Error::InvalidArgument(
            "invalid erasure identity".into(),
        ));
    }
    if let ErasureMode::Execute { actor } = mode {
        if actor.trim().is_empty() || actor.len() > 128 || actor.chars().any(char::is_control) {
            return Err(sqlx::Error::InvalidArgument("invalid erasure actor".into()));
        }
    }
    let mut tx = pool.begin().await?;
    if matches!(mode, ErasureMode::DryRun) {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '15s'")
        .execute(&mut *tx)
        .await?;
    let plan = plan();
    if matches!(mode, ErasureMode::Execute { .. }) {
        // One stable ordering for all erasures. Lock the audit table too so a
        // missing/denied audit insert cannot lead to an unaudited commit.
        let mut tables: Vec<&str> = plan
            .tables
            .iter()
            .map(|entry| entry.table.as_str())
            .collect();
        tables.push("member_erasure_audit");
        tables.sort_unstable();
        let lock = format!(
            "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
            tables.join(", ")
        );
        sqlx::query(sqlx::AssertSqlSafe(lock))
            .execute(&mut *tx)
            .await?;
    }
    if !schema_gaps(&mut tx).await?.is_empty() {
        return Err(sqlx::Error::InvalidArgument(
            "erasure schema coverage incomplete".into(),
        ));
    }
    let mut counts = Vec::new();
    for entry in &plan.tables {
        let sql = format!(
            "SELECT count(*) FROM {} WHERE {}",
            entry.table, entry.predicate
        );
        let rows = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
            .bind(guild)
            .bind(member)
            .fetch_one(&mut *tx)
            .await?;
        counts.push(TableCount {
            table: entry.table.clone(),
            rows,
        });
    }
    if let ErasureMode::Execute { actor } = mode {
        // An unresolved intent is a replay/side-effect safety guard, not stale
        // member data. Preserve it and fail the whole operation until resolved.
        for (table, unresolved_predicate) in [
            ("internal_idempotency", "state <> 'completed'"),
            ("moderation_idempotency", "state <> 'done'"),
            (
                "self_role_audit",
                "outcome NOT IN ('assigned', 'removed', 'switched', 'already_held', 'already_absent', 'rejected') OR unresolved_added_role_ids::jsonb <> '[]'::jsonb OR unresolved_removed_role_ids::jsonb <> '[]'::jsonb",
            ),
            (
                "self_role_panel_claims",
                "processing_expires_at > clock_timestamp()",
            ),
            // Only a settled claim (result recorded) is a receipt. Every
            // other state - fresh, started-uncertain, counted, released - must
            // keep its row or a gateway retry could sanction twice.
            ("automod_delivery_claims", "result_json IS NULL"),
        ] {
            let entry = plan
                .tables
                .iter()
                .find(|entry| entry.table == table)
                .expect("replay guards must be in the erasure plan");
            let sql = format!(
                "SELECT EXISTS (SELECT 1 FROM {table} WHERE ({}) AND ({unresolved_predicate}))",
                entry.predicate
            );
            let unresolved = sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(sql))
                .bind(guild)
                .bind(member)
                .fetch_one(&mut *tx)
                .await?;
            if unresolved {
                return Err(sqlx::Error::InvalidArgument(
                    "unresolved erasure replay guard".into(),
                ));
            }
        }
        for (entry, count) in plan.tables.iter().zip(&counts) {
            let sql = format!("DELETE FROM {} WHERE {}", entry.table, entry.predicate);
            let deleted = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(guild)
                .bind(member)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if deleted != count.rows as u64 {
                return Err(sqlx::Error::InvalidArgument(
                    "erasure count mismatch".into(),
                ));
            }
        }
        sqlx::query(
            "INSERT INTO member_erasure_audit (actor, erased_at) VALUES ($1, clock_timestamp())",
        )
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn reviewed_plan_has_unique_tables_and_reasoned_exceptions() {
        let plan = plan();
        let mut tables = BTreeSet::new();
        for entry in plan.tables {
            assert!(tables.insert(entry.table.clone()));
            assert!(entry
                .table
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert!(!entry.columns.is_empty());
            assert!(entry.predicate.contains("$1") && entry.predicate.contains("$2"));
            assert!(!entry.predicate.contains(';'));
            for column in entry.columns {
                assert!(
                    entry.predicate.contains(&column),
                    "{}.{}",
                    entry.table,
                    column
                );
            }
        }
        let mut exceptions = BTreeSet::new();
        for entry in plan.allowlist {
            assert!(!entry.reason.trim().is_empty());
            assert!(exceptions.insert((entry.table, entry.column)));
        }
    }

    #[test]
    fn new_user_columns_are_identity_candidates() {
        for column in [
            "user_id",
            "recipient_user_id",
            "member_id",
            "inviter_id",
            "matched_author_id",
            "created_by",
            "actor",
        ] {
            assert!(is_identity_column(column));
        }
        for column in ["guild_id", "role_id", "channel_id", "id"] {
            assert!(!is_identity_column(column));
        }
    }
}
