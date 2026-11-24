//! Guild-scoped erasure plan shared by the CLI and schema coverage tests.
//!
//! The embedded plan is reviewed code, never operator input. Count and delete
//! use the same predicate; a row matching several identity columns counts once.
//!
//! One row is deliberately never selected: a `moderation_idempotency` request
//! an operator reconciled and released (`done` / `operator_released`). Its
//! Discord effect was never proven, so that row is the replay fence that stops a
//! delayed delivery of the same key from claiming afresh and repeating the
//! mutation. It retains guild/key scope, action, terminal state/outcome, generic
//! result, timestamps and fixed non-identity `claim_token` / `request_hash`
//! markers. The caller-supplied key may itself be a member snowflake; that
//! identity-bearing exception remains because changing it would let a delayed
//! delivery claim afresh. The linked `moderation_audit` row, including matching
//! key and free-text reason, remains covered by the erasure manifest.

use serde::Deserialize;
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Transaction};

pub(crate) const SCHEMA_GAP_REFUSAL: &str = "erasure schema coverage incomplete";
pub(crate) const REPLAY_GUARD_REFUSAL: &str = "unresolved erasure replay guard";
const HISTORY_GAP: &str = "uncovered identity: backfill_history_messages.author_id";

// The scoped role plan assigns Store tables and their ledger to the public
// schema owner. Bind to those catalog identities without needing ledger rows.
const HISTORY_RELATION: &str = "
    SELECT h.oid::bigint,
           l.oid IS NOT NULL OR counts.oid IS NOT NULL,
           COALESCE(
               pg_catalog.current_schema() = 'public'
               AND h.relkind = 'r' AND h.relpersistence = 'p'
               AND h.relam = (SELECT oid FROM pg_catalog.pg_am WHERE amname = 'heap')
               AND h.relowner = n.nspowner AND h.relowner = l.relowner
               AND l.relkind = 'r' AND l.relpersistence = 'p'
               AND NOT h.relrowsecurity AND NOT h.relforcerowsecurity
               AND pg_catalog.has_schema_privilege(n.oid, 'USAGE')
               AND pg_catalog.has_table_privilege(h.oid, 'SELECT')
               AND NOT EXISTS (SELECT FROM pg_catalog.pg_inherits
                               WHERE inhparent = h.oid OR inhrelid = h.oid)
               AND ARRAY(SELECT CASE
                       WHEN a.attnotnull AND NOT a.attisdropped AND NOT a.atthasdef
                       AND a.atttypid = CASE WHEN a.attnum = 4
                           THEN 'pg_catalog.timestamptz'::pg_catalog.regtype
                           ELSE 'pg_catalog.text'::pg_catalog.regtype END
                       THEN a.attname::text END
                   FROM pg_catalog.pg_attribute a
                   WHERE a.attrelid = h.oid AND a.attnum > 0 ORDER BY a.attnum)
                   = ARRAY['message_id', 'channel_id', 'author_id', 'sent_at']
               AND EXISTS (SELECT FROM pg_catalog.pg_constraint
                           WHERE conrelid = h.oid AND contype = 'p'
                             AND conkey = ARRAY[1::smallint]),
               false)
    FROM pg_catalog.pg_namespace n
    LEFT JOIN pg_catalog.pg_class h
      ON h.relnamespace = n.oid AND h.relname = 'backfill_history_messages'
    LEFT JOIN pg_catalog.pg_class l
      ON l.relnamespace = n.oid AND l.relname = '_two_bot_migrations'
    LEFT JOIN pg_catalog.pg_class counts
      ON counts.relnamespace = n.oid AND counts.relname = 'backfill_history_counts'
    WHERE n.nspname = 'public'";

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
        || column.ends_with("_creator_id")
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
                | "holder_id"
        )
}

/// Check the actual migrated schema, not migration-text grep. Also refuse a
/// stale plan/exception (renamed or missing columns) before any deletion.
pub async fn schema_gaps(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    // information_schema.columns hides ungranted columns from the gateway.
    // A hidden identity table can still hold data that a receipt must cover.
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text
         FROM pg_catalog.pg_attribute a
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
           AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY c.relname, a.attnum",
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

/// Runtime coverage may discharge only the exact 0409 gap. The inspection
/// API above still reports it because the table has no guild attribution.
pub(crate) async fn privacy_coverage_gaps(
    tx: &mut Transaction<'_, Postgres>,
    dry_run: bool,
) -> Result<Vec<String>, sqlx::Error> {
    let mut gaps = schema_gaps(tx).await?;
    if !gaps.is_empty() && gaps != [HISTORY_GAP] {
        return Ok(gaps);
    }
    // A refused lock or read must not abort independent retention expiry.
    // Releasing this savepoint on execute keeps its locks until outer commit.
    let mut probe = tx.begin().await?;
    let covered = prove_empty_history(&mut probe, dry_run, !gaps.is_empty()).await;
    match covered {
        Ok(true) if !dry_run => probe.commit().await?,
        _ => probe.rollback().await?,
    }
    match covered {
        Ok(true) => gaps.clear(),
        Ok(false) | Err(sqlx::Error::Database(_)) => {
            if gaps.is_empty() {
                gaps.push(HISTORY_GAP.to_owned());
            }
        }
        Err(error) => return Err(error),
    }
    Ok(gaps)
}

async fn prove_empty_history(
    conn: &mut PgConnection,
    dry_run: bool,
    gap_found: bool,
) -> Result<bool, sqlx::Error> {
    let relation = sqlx::query_as::<_, (Option<i64>, bool, bool)>(HISTORY_RELATION)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((Some(oid), _, true)) = relation else {
        // Cutover-only installations have no Store footprint. A missing or
        // malformed 0409 table in a Store installation needs investigation.
        return Ok(!gap_found && relation == Some((None, false, false)));
    };
    let (lock, mode) = if dry_run {
        // Preview is read-only and uses its existing repeatable-read snapshot.
        sqlx::query("SET LOCAL row_security = off")
            .execute(&mut *conn)
            .await?;
        (
            "LOCK TABLE ONLY public.backfill_history_messages IN ACCESS SHARE MODE NOWAIT",
            "AccessShareLock",
        )
    } else {
        // The coverage scan can already have taken a snapshot. READ COMMITTED
        // gives the post-lock reads a fresh one, including writes committed
        // before SHARE was acquired. Refuse fixed-snapshot isolation.
        let fresh: bool = sqlx::query_scalar(
            "SELECT pg_catalog.current_setting('transaction_isolation') = 'read committed'",
        )
        .fetch_one(&mut *conn)
        .await?;
        if !fresh {
            return Ok(false);
        }
        (
            "LOCK TABLE ONLY public.backfill_history_messages IN SHARE MODE",
            "ShareLock",
        )
    };
    sqlx::query(lock).execute(&mut *conn).await?;
    let locked: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT FROM pg_catalog.pg_locks
         WHERE locktype = 'relation' AND pid = pg_catalog.pg_backend_pid()
           AND relation::bigint = $1 AND mode = $2 AND granted)",
    )
    .bind(oid)
    .bind(mode)
    .fetch_one(&mut *conn)
    .await?;
    let current = sqlx::query_as::<_, (Option<i64>, bool, bool)>(HISTORY_RELATION)
        .fetch_optional(&mut *conn)
        .await?;
    if !locked || current != relation {
        return Ok(false);
    }
    sqlx::query_scalar("SELECT NOT EXISTS (SELECT FROM ONLY public.backfill_history_messages)")
        .fetch_one(conn)
        .await
}

/// Guard fences shared by the per-member probe below and the guild-wide
/// probe used by the guild-purge path. Every table here is guild-keyed, so
/// the guild-wide probe scopes on `guild_id` alone.
const REPLAY_GUARDS: &[(&str, &str)] = &[
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
    // An in-flight or uncertain incident is the anti-nuke block and
    // receipt: removing it could release a block or let a retry
    // re-contain. Only settled incidents are erased.
    ("containment_incidents", "state IN ('containing', 'uncertain')"),
];

/// Shared unresolved replay-guard probe used by both `erase_member` and the
/// retention user-deletion path. Returns true when any guard fence is
/// unresolved for (guild, member): in-flight idempotency intents, unsettled
/// self-role effects/leases, unsettled automod claims, or active containment.
/// Callers must refuse deletion while this returns true, leaving rows untouched.
pub async fn has_unresolved_guards(
    conn: &mut PgConnection,
    guild: &str,
    member: &str,
) -> Result<bool, sqlx::Error> {
    let plan = plan();
    for &(table, unresolved_predicate) in REPLAY_GUARDS {
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
            .fetch_one(&mut *conn)
            .await?;
        if unresolved {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Guild-wide unresolved replay-guard probe for the guild-purge path.
/// Returns true when any guard fence is unresolved anywhere in the guild
/// (any member): in-flight idempotency intents, unsettled self-role
/// effects/leases, unsettled automod claims, or active containment. The purge
/// path must leave a guarded guild queued with rows untouched until the
/// guards settle: wiping an in-flight intent row could let a still-pending
/// request complete and double-apply on gateway retry, or release an active
/// containment block.
pub async fn has_unresolved_guild_guards(
    conn: &mut PgConnection,
    guild: &str,
) -> Result<bool, sqlx::Error> {
    let plan = plan();
    for &(table, unresolved_predicate) in REPLAY_GUARDS {
        plan.tables
            .iter()
            .find(|entry| entry.table == table)
            .expect("replay guards must be in the erasure plan");
        // Table names and predicates come from reviewed code, never caller input.
        // No member exclusions: any unresolved guard anywhere in the guild
        // defers the purge, including guards of members with a pending
        // user-deletion request. Those rows are replay fences, not stale data,
        // and the plan's per-member predicates go NULL on nullable columns, so
        // an exclusion would hide other members' guards (NOT NULL is NULL).
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {table} WHERE guild_id = $1 AND ({unresolved_predicate}))"
        );
        let unresolved = sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(sql))
            .bind(guild)
            .fetch_one(&mut *conn)
            .await?;
        if unresolved {
            return Ok(true);
        }
    }
    Ok(false)
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
    if !privacy_coverage_gaps(&mut tx, matches!(mode, ErasureMode::DryRun))
        .await?
        .is_empty()
    {
        return Err(sqlx::Error::InvalidArgument(SCHEMA_GAP_REFUSAL.into()));
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
        // Shared with the retention user-deletion path via has_unresolved_guards.
        if has_unresolved_guards(&mut tx, guild, member).await? {
            return Err(sqlx::Error::InvalidArgument(REPLAY_GUARD_REFUSAL.into()));
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
    fn holder_id_is_an_identity_candidate() {
        assert!(is_identity_column("holder_id"));
    }

    #[test]
    fn new_user_columns_are_identity_candidates() {
        for column in [
            "user_id",
            "recipient_user_id",
            "member_id",
            "inviter_id",
            "matched_author_id",
            "original_creator_id",
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
