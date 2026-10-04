//! Read-only Next-window delta report for rollback decisions (TOG-12022).
//!
//! `docs/cutover.md` notes that the forward upsert is not a rollback delta
//! exporter: rollback needs the full captured Next delta measured from the
//! `T_f` baseline. This module supplies the read-only measurement. A reverse
//! import stays out of scope.
//!
//! For every bot-owned table the report counts rows inserted or updated after
//! `T_f`, using that table's timestamp column(s). Timestamp storage is mixed
//! across the migration chain (`timestamptz` in the cutover chain, ISO-8601
//! UTC text in the store chain), so every recency predicate casts to
//! `timestamptz`: the cast is a no-op on real timestamps and parses the
//! canonical text format. A table with no usable timestamp column is reported
//! as `unmeasurable` with a reason, never silently skipped; a table absent
//! from the database is reported as `missing`.
//!
//! All reads run inside one `REPEATABLE READ, READ ONLY` transaction (see
//! [`crate::legacy_verify::read_only_transaction`]), so the per-table counts
//! share a single snapshot. Nothing here writes: migrations are never applied
//! by this path (the binary opens the database with `migrations_off`).

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;

/// Report format version, bumped only for breaking JSON changes.
pub const REPORT_VERSION: u32 = 1;

/// Maximum rows exported per table by [`export_delta`]. A rollback window is
/// small by construction; an unbounded export would let a mis-set `--since`
/// spill the whole database into a file.
pub const MAX_EXPORT_ROWS_PER_TABLE: u64 = 50_000;

/// How a table's recency is measured.
#[derive(Debug, Clone, Copy)]
pub enum TableMeasure {
    /// Timestamp columns that record the row's last write. One column uses a
    /// plain comparison; several use `GREATEST` over null-tolerant casts, so
    /// projection tables (e.g. `members`) catch updates to any column.
    Columns(&'static [&'static str]),
    /// No usable timestamp column; the reason names the gap.
    Unmeasurable(&'static str),
}

/// One bot-owned table and how its Next-window delta is measured.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub table: &'static str,
    pub measure: TableMeasure,
}

/// Every table in `sql/database_role_matrix.sql` (`kind = 'table'`) or the
/// backup allowlist ([`two_bot_core::backup::DUMP_TABLES`]), plus the two
/// feed tables the role-matrix assertion does not cover yet. Keep this list
/// exhaustive: the coverage test fails on any matrix/allowlist table missing
/// here, and on any entry here that appears in neither list (outside the two
/// documented feed tables).
pub const TABLE_SPECS: &[TableSpec] = &[
    TableSpec { table: "announcements_audit_log", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "audit_kill_switch", measure: TableMeasure::Columns(&["engaged_at"]) },
    TableSpec { table: "automation_audit_log", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "automation_commands", measure: TableMeasure::Columns(&["updated_at"]) },
    // Flag flips (matched/mutation_started/counted/released) happen inside the
    // claim's lifetime, so a delta claim shows up via claimed_at/completed_at.
    TableSpec { table: "automod_delivery_claims", measure: TableMeasure::Columns(&["claimed_at", "completed_at"]) },
    TableSpec { table: "automod_processed_messages", measure: TableMeasure::Columns(&["processed_at"]) },
    TableSpec { table: "automod_violations", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "community_facts", measure: TableMeasure::Columns(&["recorded_at"]) },
    TableSpec { table: "community_scorecard_alerts", measure: TableMeasure::Columns(&["created_at"]) },
    // Retry reservation (TOG-11145): next_attempt_at is a BIGINT epoch-ms
    // backoff deadline, not a write time, and has no timestamptz cast.
    TableSpec {
        table: "community_scorecard_attempts",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; next_attempt_at is an epoch-ms retry deadline, and a completed week is visible via community_scorecard_runs.generated_at",
        ),
    },
    TableSpec { table: "community_scorecard_runs", measure: TableMeasure::Columns(&["generated_at"]) },
    TableSpec { table: "community_stream_heartbeats", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "containment_events", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "containment_incidents", measure: TableMeasure::Columns(&["started_at"]) },
    // Same shape as guild_counters: one read-time per count, no updated_at.
    TableSpec { table: "counter_snapshots", measure: TableMeasure::Columns(&["human_member_count_at", "online_count_at"]) },
    TableSpec { table: "event_rsvps", measure: TableMeasure::Columns(&["responded_at"]) },
    TableSpec { table: "events", measure: TableMeasure::Columns(&["recorded_at"]) },
    TableSpec { table: "feed_deliveries", measure: TableMeasure::Columns(&["first_seen_at"]) },
    TableSpec { table: "feed_relays", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "gateway_boot_directives", measure: TableMeasure::Columns(&["armed_at", "consumed_at"]) },
    TableSpec {
        table: "gateway_onboarding_jobs",
        measure: TableMeasure::Unmeasurable(
            "transient restart-recovery queue with no timestamp column (occurred_at_ms is a gateway event time); rows hold no durable member state",
        ),
    },
    TableSpec { table: "gateway_sessions", measure: TableMeasure::Columns(&["updated_at"]) },
    // Collector read-times ride the same row write, so their maximum is the
    // row's last write.
    TableSpec { table: "guild_counters", measure: TableMeasure::Columns(&["human_member_count_at", "online_count_at"]) },
    TableSpec { table: "guild_settings", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "guild_settings_audit", measure: TableMeasure::Columns(&["at"]) },
    TableSpec {
        table: "guild_settings_revision",
        measure: TableMeasure::Unmeasurable(
            "monotonic revision counter with no timestamp column; changes are visible via guild_settings.updated_at",
        ),
    },
    TableSpec { table: "internal_action_log", measure: TableMeasure::Columns(&["created_at"]) },
    // One row per clock domain, rewritten on every burn; observed_at is the
    // DB instant that advanced the mark, so its maximum is the row's last write.
    TableSpec { table: "internal_clock_high_water", measure: TableMeasure::Columns(&["observed_at"]) },
    TableSpec { table: "internal_discord_events", measure: TableMeasure::Columns(&["claimed_at"]) },
    // Re-mapped keys rewrite updated_at without touching created_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec { table: "internal_event_keys", measure: TableMeasure::Columns(&["created_at", "updated_at"]) },
    TableSpec { table: "internal_idempotency", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "internal_nonces", measure: TableMeasure::Columns(&["burned_at"]) },
    // Retirements rewrite disabled_at without touching created_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec { table: "invite_campaigns", measure: TableMeasure::Columns(&["created_at", "disabled_at"]) },
    TableSpec { table: "invite_snapshots", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "join_risk_flags", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "level_import_runs", measure: TableMeasure::Columns(&["imported_at"]) },
    TableSpec {
        table: "level_role_rewards",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; configuration is replaced wholesale (see replace_role_rewards)",
        ),
    },
    TableSpec {
        table: "lfg_posts",
        measure: TableMeasure::Columns(&["created_at"]),
    },
    TableSpec {
        table: "lfg_roles",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; child rows are created and updated with the parent lfg_posts row",
        ),
    },
    TableSpec { table: "lfg_signups", measure: TableMeasure::Columns(&["joined_at"]) },
    TableSpec { table: "member_erasure_audit", measure: TableMeasure::Columns(&["erased_at"]) },
    TableSpec { table: "member_exclusions", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_levels", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_ranks", measure: TableMeasure::Columns(&["updated_at"]) },
    // Pure projection: any milestone/activity write must move the maximum.
    TableSpec {
        table: "members",
        measure: TableMeasure::Columns(&[
            "joined_at",
            "first_message_at",
            "third_message_at",
            "first_voice_at",
            "last_active_at",
            "left_at",
            "inactive_flagged_at",
            "gate_cleared_at",
        ]),
    },
    TableSpec { table: "moderation_audit", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec {
        table: "moderation_channel_executions",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; the execution fence row is written with its parent moderation_idempotency claim (measured via claimed_at)",
        ),
    },
    TableSpec { table: "moderation_idempotency", measure: TableMeasure::Columns(&["claimed_at"]) },
    TableSpec { table: "moderation_lockdowns", measure: TableMeasure::Columns(&["locked_at"]) },
    TableSpec { table: "moderation_member_bans", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "moderation_scheduled_unbans", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "moderation_warnings", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "operational_audit_log", measure: TableMeasure::Columns(&["occurred_at"]) },
    TableSpec { table: "presence_probe", measure: TableMeasure::Columns(&["observed_at"]) },
    TableSpec {
        table: "rank_ladder",
        measure: TableMeasure::Unmeasurable("seed reference data with no timestamp column"),
    },
    TableSpec { table: "rank_snapshots", measure: TableMeasure::Columns(&["snapshot_at"]) },
    TableSpec { table: "scheduled_events", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "scheduled_messages", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "self_role_audit", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec {
        table: "self_role_exchange_baselines",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; the baseline row is written with its event's exchange lifecycle (measured via self_role_exchanges.created_at)",
        ),
    },
    // Ticket creation, settlement and retirement each rewrite a different
    // column without touching the others, so the maximum is the row's last write.
    TableSpec {
        table: "self_role_exchanges",
        measure: TableMeasure::Columns(&["created_at", "completed_at", "retired_at"]),
    },
    TableSpec { table: "self_role_panel_claims", measure: TableMeasure::Columns(&["processing_expires_at"]) },
    TableSpec { table: "sticky_messages", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "ticket_transcripts", measure: TableMeasure::Columns(&["created_at"]) },
    // Status/close transitions rewrite these columns without touching
    // created_at, so the maximum across all three is the row's last write.
    TableSpec {
        table: "tickets",
        measure: TableMeasure::Columns(&["created_at", "closing_started_at", "closed_at"]),
    },
    TableSpec {
        table: "voice_creators",
        measure: TableMeasure::Unmeasurable(
            "no timestamp column; creator configuration is upserted in place (see PgRoomStore::add_creator)",
        ),
    },
    // V3 block list (0416): one row per blocked member, stamped on insert; an
    // unblock deletes the row, so only additions are measurable.
    TableSpec { table: "voice_room_blocks", measure: TableMeasure::Columns(&["created_at"]) },
    // Insert-once creation snapshot plus V2 ownership handoffs, whose
    // timestamp lives in `owner_touched_at` (migration 0412), V3 `/name`
    // custom-name changes, stamped in `name_touched_at` (migration 0414), and V3
    // privacy writes, stamped in `privacy_touched_at` (migration 0416).
    TableSpec {
        table: "voice_rooms",
        measure: TableMeasure::Columns(&["created_at", "owner_touched_at", "name_touched_at", "privacy_touched_at"]),
    },
    // Capture both accepted creates and post-baseline bindings/rollbacks of
    // reservations created before the baseline (migration 0417).
    TableSpec { table: "voice_create_reservations", measure: TableMeasure::Columns(&["created_at", "settled_at"]) },
    TableSpec { table: "voice_owner_grants", measure: TableMeasure::Columns(&["touched_at"]) },
    // Same insert-once shape as voice_rooms (PgRoomStore::add_companion never
    // overwrites the creation snapshot).
    TableSpec { table: "voice_text_companions", measure: TableMeasure::Columns(&["created_at"]) },
    // V11b configuration tables (0229): replaced wholesale by
    // PgVoiceConfigStore::apply, so none carries a write timestamp.
    TableSpec {
        table: "voice_channel_templates",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_game_aliases",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_random_lists",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_random_list_choices",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_logging",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_logging_mention_members",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_logging_mention_roles",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_guild_settings",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_command_roles",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_command_role_members",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)"),
    },
    TableSpec {
        table: "voice_logging_settings",
        measure: TableMeasure::Unmeasurable(
            "mutable per-guild settings row with no timestamp column; no member IDs",
        ),
    },
    TableSpec {
        table: "voice_access_controls",
        measure: TableMeasure::Unmeasurable(
            "mutable per-guild settings row with no timestamp column; no member IDs",
        ),
    },
    TableSpec {
        table: "web_contract_meta",
        measure: TableMeasure::Unmeasurable("singleton contract row with no timestamp column"),
    },
    TableSpec { table: "xp_awards", measure: TableMeasure::Columns(&["occurred_at"]) },
    TableSpec { table: "xp_cooldowns", measure: TableMeasure::Columns(&["last_awarded_at"]) },
];

/// Feed tables predate the role-matrix assertion; they are measured here and
/// the coverage test names them explicitly so a future matrix addition cannot
/// hide behind this list.
pub const EXTRA_MEASURED_TABLES: &[&str] = &["feed_relays", "feed_deliveries"];

#[derive(Debug, Error)]
pub enum DeltaError {
    #[error("database error (details omitted)")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Usage(String),
    #[error("export row cap exceeded for table {0}")]
    ExportCap(String),
    #[error("cannot write export file")]
    ExportIo(#[from] std::io::Error),
}

/// `--since` validation failure.
#[derive(Debug, PartialEq, Eq)]
pub enum SinceError {
    Missing,
    Invalid,
    Future,
}

impl std::fmt::Display for SinceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinceError::Missing => write!(f, "--since <RFC3339> is required"),
            SinceError::Invalid => write!(f, "--since must be RFC3339 (e.g. 2026-09-30T12:00:00Z)"),
            SinceError::Future => write!(f, "--since must not be in the future"),
        }
    }
}

/// Validate the `T_f` baseline: present, RFC3339, not in the future.
pub fn parse_since(raw: Option<&str>) -> Result<time::OffsetDateTime, SinceError> {
    let raw = raw
        .filter(|s| !s.trim().is_empty())
        .ok_or(SinceError::Missing)?;
    let since = time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|_| SinceError::Invalid)?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = time::OffsetDateTime::from_unix_timestamp(now_secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    if since > now {
        return Err(SinceError::Future);
    }
    Ok(since)
}

/// Quote a static identifier. Table/column names are compile-time constants,
/// so this only ever fails on a programming error; still, never interpolate
/// an unquoted name (`at` is a reserved word).
fn quote(name: &str) -> Result<String, DeltaError> {
    if name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || !name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
    {
        return Err(DeltaError::Usage(format!("invalid identifier: {name}")));
    }
    Ok(format!("\"{name}\""))
}

/// `recency > T_f` for one spec's columns. Single column: plain comparison.
/// Several: `GREATEST` over null-tolerant casts so a NULL column never sinks
/// the row (`GREATEST` returns NULL if any argument is NULL, hence `COALESCE`
/// to `-infinity`).
fn recency_predicate(spec: &TableSpec) -> Result<String, DeltaError> {
    let TableMeasure::Columns(columns) = spec.measure else {
        return Err(DeltaError::Usage(format!(
            "table {} is unmeasurable",
            spec.table
        )));
    };
    if columns.is_empty() {
        return Err(DeltaError::Usage(format!(
            "table {} has no columns",
            spec.table
        )));
    }
    let cast = |column: &str| -> Result<String, DeltaError> {
        Ok(format!("{}::timestamptz", quote(column)?))
    };
    let expr = if columns.len() == 1 {
        cast(columns[0])?
    } else {
        let arms = columns
            .iter()
            .map(|c| Ok(format!("COALESCE({}, '-infinity'::timestamptz)", cast(c)?)))
            .collect::<Result<Vec<_>, DeltaError>>()?
            .join(", ");
        format!("GREATEST({arms})")
    };
    Ok(format!("{expr} > $1::timestamptz"))
}

/// Per-table delta entry in the JSON summary.
#[derive(Debug, Serialize)]
pub struct TableDelta {
    pub table: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The JSON summary: one entry per spec table, never silently skipped.
#[derive(Debug, Serialize)]
pub struct DeltaReport {
    pub version: u32,
    pub since: String,
    pub generated_at: String,
    pub tables: Vec<TableDelta>,
}

/// Count one measured table inside the caller's read-only transaction. A
/// missing table (partially migrated database) is reported, not an error.
async fn count_table(
    tx: &mut Transaction<'_, Postgres>,
    spec: &TableSpec,
    since: &str,
) -> Result<TableDelta, DeltaError> {
    let table = quote(spec.table)?;
    // Unqualified on purpose: it resolves through the same search_path as the
    // COUNT below, so schema-isolated test databases behave like production.
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(spec.table)
        .fetch_one(&mut **tx)
        .await?;
    if !exists {
        return Ok(TableDelta {
            table: spec.table.to_owned(),
            status: "missing".to_owned(),
            columns: None,
            count: None,
            reason: Some("table not present in this database".to_owned()),
        });
    }
    let predicate = recency_predicate(spec)?;
    // Table and column names are static constants validated by `quote`;
    // only the timestamp bound is bound.
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .fetch_one(&mut **tx)
        .await?;
    Ok(TableDelta {
        table: spec.table.to_owned(),
        status: "measured".to_owned(),
        columns: match spec.measure {
            TableMeasure::Columns(columns) => {
                Some(columns.iter().map(|c| (*c).to_owned()).collect())
            }
            TableMeasure::Unmeasurable(_) => None,
        },
        count: Some(count),
        reason: None,
    })
}

/// Build the full summary inside one read-only snapshot transaction.
pub async fn report(pool: &PgPool, since: &str) -> Result<DeltaReport, DeltaError> {
    report_at(pool, since, crate::cli::now_iso()).await
}

async fn report_at(
    pool: &PgPool,
    since: &str,
    generated_at: String,
) -> Result<DeltaReport, DeltaError> {
    let mut conn = pool.acquire().await?;
    let mut tx = crate::legacy_verify::read_only_transaction(&mut conn)
        .await
        .map_err(|e| match e {
            crate::legacy_verify::VerifyError::Database(inner) => DeltaError::Database(inner),
            other => DeltaError::Usage(other.to_string()),
        })?;
    let mut tables = Vec::with_capacity(TABLE_SPECS.len());
    for spec in TABLE_SPECS {
        match spec.measure {
            TableMeasure::Unmeasurable(reason) => tables.push(TableDelta {
                table: spec.table.to_owned(),
                status: "unmeasurable".to_owned(),
                columns: None,
                count: None,
                reason: Some(reason.to_owned()),
            }),
            TableMeasure::Columns(_) => tables.push(count_table(&mut tx, spec, since).await?),
        }
    }
    tx.rollback().await?;
    Ok(DeltaReport {
        version: REPORT_VERSION,
        since: since.to_owned(),
        generated_at,
        tables,
    })
}

/// Export every measured post-`T_f` row as NDJSON (`{"table": ..., "row": ...}`
/// per line) into the caller's read-only snapshot. Refuses when any table
/// exceeds [`MAX_EXPORT_ROWS_PER_TABLE`] rather than silently truncating.
pub async fn export_delta(
    pool: &PgPool,
    since: &str,
    writer: &mut dyn FnMut(String) -> std::io::Result<()>,
) -> Result<u64, DeltaError> {
    let mut conn = pool.acquire().await?;
    let mut tx = crate::legacy_verify::read_only_transaction(&mut conn)
        .await
        .map_err(|e| match e {
            crate::legacy_verify::VerifyError::Database(inner) => DeltaError::Database(inner),
            other => DeltaError::Usage(other.to_string()),
        })?;
    let mut exported: u64 = 0;
    for spec in TABLE_SPECS {
        let TableMeasure::Columns(_) = spec.measure else {
            continue;
        };
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(spec.table)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            continue;
        }
        let predicate = recency_predicate(spec)?;
        let table = quote(spec.table)?;
        let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(count_sql))
            .bind(since)
            .fetch_one(&mut *tx)
            .await?;
        if count as u64 > MAX_EXPORT_ROWS_PER_TABLE {
            return Err(DeltaError::ExportCap(spec.table.to_owned()));
        }
        if count == 0 {
            continue;
        }
        let row_sql = format!("SELECT to_jsonb(t.*)::text FROM {table} t WHERE {predicate}");
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(row_sql))
            .bind(since)
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let line = serde_json::json!({"table": spec.table, "row": serde_json::from_str::<serde_json::Value>(&row).unwrap_or(serde_json::Value::Null)}).to_string();
            writer(line)?;
            exported += 1;
        }
    }
    tx.rollback().await?;
    Ok(exported)
}

/// File-writing sink for [`export_delta`].
pub fn file_writer(file: std::fs::File) -> impl FnMut(String) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = file;
    move |line: String| {
        writeln!(file, "{line}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const MATRIX_SQL: &str = include_str!("../../../sql/database_role_matrix.sql");

    fn matrix_tables() -> BTreeSet<String> {
        MATRIX_SQL
            .lines()
            .filter(|l| l.trim_end().ends_with("'table'),"))
            .filter_map(|l| {
                let mut parts = l.split('\'');
                parts.next()?;
                let schema = parts.next()?;
                parts.next()?;
                let name = parts.next()?;
                (schema == "public").then(|| name.to_owned())
            })
            .collect()
    }

    #[test]
    fn since_requires_present_rfc3339_past() {
        assert_eq!(parse_since(None), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("")), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("  ")), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("yesterday")), Err(SinceError::Invalid));
        assert_eq!(
            parse_since(Some("2026-13-01T00:00:00Z")),
            Err(SinceError::Invalid)
        );
        assert_eq!(
            parse_since(Some("2999-01-01T00:00:00Z")),
            Err(SinceError::Future)
        );
        assert!(parse_since(Some("2026-09-30T12:00:00Z")).is_ok());
        assert!(parse_since(Some("2026-09-30T12:00:00+00:00")).is_ok());
    }

    #[test]
    fn every_matrix_and_allowlist_table_is_classified() {
        let specs: BTreeSet<String> = TABLE_SPECS.iter().map(|s| s.table.to_owned()).collect();
        // No duplicate specs.
        assert_eq!(specs.len(), TABLE_SPECS.len());
        let mut universe = matrix_tables();
        for table in two_bot_core::backup::DUMP_TABLES {
            universe.insert((*table).to_owned());
        }
        for table in EXTRA_MEASURED_TABLES {
            universe.insert((*table).to_owned());
        }
        let unclassified: Vec<_> = universe.difference(&specs).collect();
        assert!(
            unclassified.is_empty(),
            "unclassified tables would be silently skipped: {unclassified:?}"
        );
        // Nothing extra beyond the documented feed tables: a new measured
        // table must arrive via the matrix or the allowlist.
        let mut allowed_extra: BTreeSet<String> = EXTRA_MEASURED_TABLES
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        allowed_extra.extend(universe);
        let unexpected: Vec<_> = specs.difference(&allowed_extra).collect();
        assert!(unexpected.is_empty(), "unexpected specs: {unexpected:?}");
    }

    #[test]
    fn recency_predicates_cast_and_guard_nulls() {
        let single = TableSpec {
            table: "events",
            measure: TableMeasure::Columns(&["recorded_at"]),
        };
        assert_eq!(
            recency_predicate(&single).unwrap(),
            "\"recorded_at\"::timestamptz > $1::timestamptz"
        );
        let multi = TableSpec {
            table: "members",
            measure: TableMeasure::Columns(&["joined_at", "left_at"]),
        };
        assert_eq!(
            recency_predicate(&multi).unwrap(),
            "GREATEST(COALESCE(\"joined_at\"::timestamptz, '-infinity'::timestamptz), COALESCE(\"left_at\"::timestamptz, '-infinity'::timestamptz)) > $1::timestamptz"
        );
        assert!(recency_predicate(&TableSpec {
            table: "rank_ladder",
            measure: TableMeasure::Unmeasurable("seed data"),
        })
        .is_err());
        assert!(quote("guild_settings; DROP TABLE x").is_err());
        assert!(quote("at").is_ok());
    }
}
