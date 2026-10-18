//! Reverse import for rollback-delta exports (TOG-29726).
//!
//! [`crate::rollback_delta::export_delta`] measures the post-`T_f` Next-window
//! delta and writes it as NDJSON (`{"table": ..., "row": ...}` per line). This
//! module replays such a file into a target database: the `rollback-delta
//! import` subcommand's file validation, row-count plan and single-transaction
//! apply.
//!
//! Safety shape (mirrors `legacy_copy`): the default is a dry run that never
//! opens a database — the plan is computed from the file alone. `--apply`
//! prints the same plan with `"mode": "apply"` before any mutation and
//! requires the live-target opt-in (`--allow-live-target`, enforced via
//! [`crate::legacy_copy::options::guarded_target`]) before connecting. File
//! validation (malformed lines, unknown tables, per-table row-cap overruns)
//! runs offline and refuses before any write. The apply itself runs inside
//! one transaction, so a late failure rolls back instead of half-importing.
//!
//! Rows are written with `jsonb_populate_record`, which maps the exported
//! `to_jsonb` object back onto the table's columns by name (unknown keys are
//! ignored, missing keys become NULL). Rows replay in backup
//! [`two_bot_core::backup::DUMP_TABLES`] restore order (parents before
//! children), stably, so a file listing a child before its parent still
//! applies. Inserts use bare `ON CONFLICT DO NOTHING`, so re-running an
//! identical import is idempotent: conflicting rows keep their existing
//! values and are reported as skipped, never silently applied. Owned serial
//! sequences for imported tables advance past the imported maxima in the same
//! transaction. Skipped rows are classified by containment (`target @>
//! incoming`): an identical row is an idempotent re-run, while a skipped row
//! with no identical match is divergent (a pre-existing row with different
//! values keeps the target values). Divergent rows are reported per table with
//! their keys and fail the CLI (exit 3) unless `--acknowledge-divergent` is
//! passed. `lfg_roles` has no timestamp column but round-trips as a parent
//! join: the exporter emits the roles of every exported `lfg_posts` row, so a
//! post-freeze post imports with its roles and signups instead of aborting on
//! the missing parent.

use std::collections::BTreeMap;

use sqlx::PgPool;
use thiserror::Error;
use two_bot_core::backup::DUMP_TABLES;

use crate::rollback_delta::{
    TableMeasure, MAX_EXPORT_ROWS_PER_TABLE, PARENT_JOINED_TABLES, TABLE_SPECS,
};

/// Plan format version, bumped only for breaking JSON changes.
pub const IMPORT_PLAN_VERSION: u32 = 1;

/// One validated export line: the target table and its full row object.
#[derive(Debug, Clone)]
pub struct ImportRow {
    pub table: String,
    pub row: serde_json::Value,
}

/// File-validation failure. Each variant names its class on purpose: the CLI
/// surfaces these messages on stderr with exit 2 before any write.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImportError {
    /// A line is not a `{"table", "row"}` object with an object row.
    #[error("malformed import file at line {line}: expected a {{\"table\", \"row\"}} object with an object row")]
    Malformed { line: usize },
    /// A table the exporter cannot emit (not a measured rollback-delta table).
    #[error("unknown table '{table}': not a table rollback-delta export can emit")]
    UnknownTable { table: String },
    /// More rows for one table than the exporter ever writes per table.
    #[error("row cap exceeded for table '{table}': more than {MAX_EXPORT_ROWS_PER_TABLE} rows")]
    RowCap { table: String },
}

/// Apply-time failure. Database details are withheld; the CLI prints only
/// these redacted messages. The constraint variant names only the failing
/// table and constraint (both static database identifiers), never URLs or row
/// values, so the operator can bisect the NDJSON without a blind retry.
#[derive(Debug, Error)]
pub enum ImportApplyError {
    #[error("database error (details omitted)")]
    Database(#[from] sqlx::Error),
    #[error("import into '{table}' violates {constraint} (parent row missing or already changed; see docs/cutover.md)")]
    ConstraintViolation { table: String, constraint: String },
    #[error("invalid table name in validated import")]
    InvalidTable(String),
}

/// True when the exporter can emit rows for `table`: a measured table (one
/// with timestamp columns) or a [`PARENT_JOINED_TABLES`] parent (emitted only
/// alongside its exported parent), never any other unmeasurable or absent one.
#[must_use]
pub fn is_exportable_table(table: &str) -> bool {
    if PARENT_JOINED_TABLES.contains(&table) {
        return true;
    }
    TABLE_SPECS
        .iter()
        .any(|spec| spec.table == table && matches!(spec.measure, TableMeasure::Columns(_)))
}

/// Quote a validated table name. Names reaching the apply path come from
/// [`parse_import`], which already restricts them to [`is_exportable_table`]
/// (compile-time-known constants); this is defense in depth, mirroring
/// `rollback_delta`'s quoting.
fn quote(name: &str) -> Result<String, ImportApplyError> {
    if name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || !name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
    {
        return Err(ImportApplyError::InvalidTable(name.to_owned()));
    }
    Ok(format!("\"{name}\""))
}

/// Validate a whole export file offline: every line must be a
/// `{"table", "row"}` object with an object row, every table must be one the
/// exporter can emit, and no table may exceed
/// [`MAX_EXPORT_ROWS_PER_TABLE`] rows. Blank lines are skipped (the exporter
/// terminates every line, including the last, with `\n`); line numbers are
/// physical 1-based file lines. Returns the rows in file order.
pub fn parse_import(contents: &str) -> Result<Vec<ImportRow>, ImportError> {
    let mut rows = Vec::new();
    let mut per_table: BTreeMap<String, u64> = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|_| ImportError::Malformed { line: line_no })?;
        let table = value
            .get("table")
            .and_then(serde_json::Value::as_str)
            .ok_or(ImportError::Malformed { line: line_no })?;
        let row = value
            .get("row")
            .filter(|row| row.is_object())
            .ok_or(ImportError::Malformed { line: line_no })?;
        if !is_exportable_table(table) {
            return Err(ImportError::UnknownTable {
                table: table.to_owned(),
            });
        }
        let count = per_table.entry(table.to_owned()).or_insert(0);
        *count += 1;
        if *count > MAX_EXPORT_ROWS_PER_TABLE {
            return Err(ImportError::RowCap {
                table: table.to_owned(),
            });
        }
        rows.push(ImportRow {
            table: table.to_owned(),
            row: row.clone(),
        });
    }
    Ok(rows)
}

/// The row-count plan printed before any mutation: `{"version": 1, "mode":
/// ..., "tables": [{"table", "rows"}], "total_rows"}`. Only tables with at
/// least one row appear, in first-seen file order.
#[must_use]
pub fn import_plan(mode: &str, rows: &[ImportRow]) -> serde_json::Value {
    let mut per_table: Vec<(String, u64)> = Vec::new();
    for row in rows {
        match per_table.iter_mut().find(|(table, _)| *table == row.table) {
            Some((_, count)) => *count += 1,
            None => per_table.push((row.table.clone(), 1)),
        }
    }
    let total_rows: u64 = per_table.iter().map(|(_, count)| count).sum();
    serde_json::json!({
        "version": IMPORT_PLAN_VERSION,
        "mode": mode,
        "tables": per_table
            .iter()
            .map(|(table, count)| serde_json::json!({"table": table, "rows": count}))
            .collect::<Vec<_>>(),
        "total_rows": total_rows,
    })
}

/// Per-table apply counts, in first-seen apply order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOutcome {
    pub table: String,
    pub inserted: u64,
    pub skipped: u64,
    pub divergent: u64,
}

/// Outcome of [`apply_import`]: rows actually inserted vs rows skipped as
/// already present (`ON CONFLICT DO NOTHING`). `inserted + skipped` is always
/// the number of import rows processed. `divergent` is the subset of skipped
/// rows with no identical match in the target (`target @> incoming` fails):
/// a first run that drops updates vs an identical re-run. `divergent_keys`
/// names up to [`MAX_DIVERGENT_KEYS`] `table:key` entries for the runbook's
/// manual-reconciliation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub inserted: u64,
    pub skipped: u64,
    pub divergent: u64,
    pub per_table: Vec<TableOutcome>,
    pub divergent_keys: Vec<String>,
}

/// Cap on reported divergent keys: the count is always exact, only the key
/// list is bounded so a wide rollback cannot flood operator output.
pub const MAX_DIVERGENT_KEYS: usize = 50;

impl ApplyOutcome {
    /// Total rows processed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.inserted + self.skipped
    }
}

/// Best-effort human key for a divergent row: stable identifiers first, then a
/// truncated JSON snapshot. Only static column names and row values the
/// operator already holds in the export file; never connection details.
#[must_use]
pub fn row_key_hint(row: &serde_json::Value) -> String {
    let str_field = |name: &str| row.get(name).and_then(serde_json::Value::as_str);
    if let Some(id) = str_field("id") {
        return format!("id={id}");
    }
    match (
        str_field("lfg_id"),
        str_field("user_id"),
        str_field("role_key"),
    ) {
        (Some(lfg), Some(user), _) => return format!("lfg_id={lfg} user_id={user}"),
        (Some(lfg), None, Some(role)) => return format!("lfg_id={lfg} role_key={role}"),
        _ => {}
    }
    if let (Some(g), Some(m)) = (str_field("guild_id"), str_field("member_id")) {
        return format!("guild_id={g} member_id={m}");
    }
    if let Some(ticket) = str_field("ticket_id") {
        return format!("ticket_id={ticket}");
    }
    if let (Some(g), Some(c)) = (str_field("guild_id"), str_field("channel_id")) {
        return format!("guild_id={g} channel_id={c}");
    }
    if let Some(g) = str_field("guild_id") {
        return format!("guild_id={g}");
    }
    let flat = serde_json::to_string(row).unwrap_or_default();
    let mut flat: String = flat.chars().filter(|c| !c.is_whitespace()).collect();
    flat.truncate(80);
    flat
}

/// Restore rank of an exportable table: its position in the backup
/// [`DUMP_TABLES`] order (parents before children). Tables outside the
/// allowlist sort last.
#[must_use]
pub fn import_table_order(table: &str) -> usize {
    DUMP_TABLES
        .iter()
        .position(|t| *t == table)
        .unwrap_or(usize::MAX)
}

/// Rows in FK-safe apply order: a stable sort by [`import_table_order`], so
/// file order is preserved among rows for the same table.
#[must_use]
pub fn ordered_rows(rows: &[ImportRow]) -> Vec<&ImportRow> {
    let mut ordered: Vec<&ImportRow> = rows.iter().collect();
    ordered.sort_by_key(|row| import_table_order(&row.table));
    ordered
}

/// Advance every sequence owned by one of `tables` past its column maximum,
/// without ever moving a sequence backwards. Discovered via `pg_depend`, so
/// tables with no owned sequence (natural keys) and tables absent from a
/// partially migrated target are no-ops.
async fn advance_owned_sequences(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tables: &[&str],
) -> Result<(), ImportApplyError> {
    for table in tables {
        let owned: Vec<(String, String)> = sqlx::query_as(
            "SELECT s.relname::text, a.attname::text \
             FROM pg_depend d \
             JOIN pg_class s ON s.oid = d.objid \
             JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
             WHERE d.refobjid = to_regclass($1) AND d.deptype = 'a' AND s.relkind = 'S'",
        )
        .bind(*table)
        .fetch_all(&mut **tx)
        .await?;
        for (sequence, column) in &owned {
            let table_q = quote(table)?;
            let seq_q = quote(sequence)?;
            let col_q = quote(column)?;
            // `setval` with `is_called = false` makes the next `nextval`
            // return exactly the set value (1 on an empty table, not 2).
            let sql = format!(
                "SELECT setval('{seq_q}'::regclass, GREATEST(COALESCE((SELECT MAX({col_q}) FROM {table_q}), 0) + 1, (SELECT last_value + CASE WHEN is_called THEN 1 ELSE 0 END FROM {seq_q})), false)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}

/// Replay validated rows into the target inside one transaction. Each row is
/// mapped back onto its table's columns with `jsonb_populate_record` and
/// inserted with bare `ON CONFLICT DO NOTHING`, so an identical re-run
/// changes nothing. Rows apply in [`DUMP_TABLES`] parent-first order.
/// Returns inserted vs skipped counts plus the divergent subset: skipped rows
/// with no identical match (`target @> incoming` fails) keep the target
/// values and must be reconciled by hand. A failed insert names its table and
/// constraint instead of a redacted generic.
pub async fn apply_import(
    pool: &PgPool,
    rows: &[ImportRow],
) -> Result<ApplyOutcome, ImportApplyError> {
    let ordered = ordered_rows(rows);
    let mut tx = pool.begin().await?;
    // Serial writers behind this import read `events_id_seq`; hold the table
    // while its rows land so the post-insert reconciliation sees a stable
    // maximum. Only when the file carries events rows and the target has the
    // table (partially migrated targets may not).
    if rows.iter().any(|row| row.table == "events") {
        let events_exists: bool = sqlx::query_scalar("SELECT to_regclass('events') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
        if events_exists {
            sqlx::query("LOCK TABLE events IN SHARE ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
        }
    }
    let mut inserted: u64 = 0;
    let mut skipped: u64 = 0;
    let mut divergent: u64 = 0;
    let mut per_table: Vec<TableOutcome> = Vec::new();
    let mut divergent_keys: Vec<String> = Vec::new();
    for row in &ordered {
        let table = quote(&row.table)?;
        // Table names are validated statics; only the row document is bound.
        let sql = format!(
            "INSERT INTO {table} SELECT * FROM jsonb_populate_record(NULL::{table}, $1) ON CONFLICT DO NOTHING"
        );
        let affected = match sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&row.row)
            .execute(&mut *tx)
            .await
        {
            Ok(done) => done.rows_affected(),
            Err(sqlx::Error::Database(db_err)) => {
                let constraint = db_err
                    .constraint()
                    .unwrap_or("unknown constraint")
                    .to_owned();
                let table_name = db_err.table().unwrap_or(row.table.as_str()).to_owned();
                return Err(ImportApplyError::ConstraintViolation {
                    table: table_name,
                    constraint,
                });
            }
            Err(other) => return Err(ImportApplyError::Database(other)),
        };
        let entry = match per_table.iter_mut().find(|e| e.table == row.table) {
            Some(entry) => entry,
            None => {
                per_table.push(TableOutcome {
                    table: row.table.clone(),
                    inserted: 0,
                    skipped: 0,
                    divergent: 0,
                });
                per_table.last_mut().expect("just pushed")
            }
        };
        if affected > 0 {
            inserted += affected;
            entry.inserted += affected;
        } else {
            skipped += 1;
            entry.skipped += 1;
            // Containment classifies the skip: an identical row present means
            // an idempotent re-run; no identical match means the conflicting
            // row differs and the update was dropped.
            let identical_sql = format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE to_jsonb({table}.*) @> $1::jsonb)"
            );
            let identical: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(identical_sql))
                .bind(&row.row)
                .fetch_one(&mut *tx)
                .await?;
            if !identical {
                divergent += 1;
                entry.divergent += 1;
                if divergent_keys.len() < MAX_DIVERGENT_KEYS {
                    divergent_keys.push(format!("{}:{}", row.table, row_key_hint(&row.row)));
                }
            }
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in &ordered {
        if !seen.contains(&row.table.as_str()) {
            seen.push(&row.table);
        }
    }
    advance_owned_sequences(&mut tx, &seen).await?;
    tx.commit().await?;
    Ok(ApplyOutcome {
        inserted,
        skipped,
        divergent,
        per_table,
        divergent_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(table: &str, row: &str) -> String {
        format!("{{\"table\":\"{table}\",\"row\":{row}}}")
    }

    #[test]
    fn valid_file_parses_in_order_with_blank_lines_skipped() {
        let contents = format!(
            "{}\n\n{}\n   \n{}\n",
            line(
                "member_levels",
                r#"{"guild_id":"g","member_id":"m2","xp":30}"#
            ),
            line("members", r#"{"guild_id":"g","member_id":"m"}"#),
            line(
                "member_levels",
                r#"{"guild_id":"g","member_id":"m3","xp":45}"#
            ),
        );
        let rows = parse_import(&contents).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].table, "member_levels");
        assert_eq!(rows[1].table, "members");
        assert_eq!(rows[2].table, "member_levels");
    }

    #[test]
    fn malformed_lines_name_the_physical_line() {
        // Not JSON on line 2.
        let err =
            parse_import(&format!("{}\n{{broken\n", line("member_levels", "{}"))).unwrap_err();
        assert_eq!(err, ImportError::Malformed { line: 2 });
        assert!(err.to_string().contains("malformed import file"));
        assert!(err.to_string().contains("line 2"));

        // Missing row object.
        let err = parse_import("{\"table\":\"member_levels\"}\n").unwrap_err();
        assert_eq!(err, ImportError::Malformed { line: 1 });

        // Non-object row.
        let err = parse_import(&format!("{}\n", line("member_levels", "42"))).unwrap_err();
        assert_eq!(err, ImportError::Malformed { line: 1 });

        // Top-level array.
        let err = parse_import("[1,2]\n").unwrap_err();
        assert_eq!(err, ImportError::Malformed { line: 1 });

        // Non-string table.
        let err = parse_import("{\"table\":42,\"row\":{}}\n").unwrap_err();
        assert_eq!(err, ImportError::Malformed { line: 1 });
    }

    #[test]
    fn unknown_tables_refuse_with_the_table_name() {
        let err = parse_import(&format!("{}\n", line("no_such_table", "{}"))).unwrap_err();
        assert_eq!(
            err,
            ImportError::UnknownTable {
                table: "no_such_table".to_owned()
            }
        );
        assert!(err.to_string().contains("unknown table"));
        assert!(err.to_string().contains("no_such_table"));

        // Classified but never exported (unmeasurable) is outside the
        // exporter's vocabulary too.
        let err = parse_import(&format!("{}\n", line("rank_ladder", "{}"))).unwrap_err();
        assert_eq!(
            err,
            ImportError::UnknownTable {
                table: "rank_ladder".to_owned()
            }
        );
    }

    #[test]
    fn row_cap_counts_per_table() {
        let mut contents = String::new();
        for index in 0..MAX_EXPORT_ROWS_PER_TABLE {
            contents.push_str(&line("member_levels", &format!("{{\"n\":{index}}}")));
            contents.push('\n');
        }
        // Exactly at the cap parses.
        assert_eq!(
            parse_import(&contents).unwrap().len(),
            MAX_EXPORT_ROWS_PER_TABLE as usize
        );
        // One more row overruns.
        contents.push_str(&line("member_levels", "{\"n\":\"over\"}"));
        contents.push('\n');
        let err = parse_import(&contents).unwrap_err();
        assert_eq!(
            err,
            ImportError::RowCap {
                table: "member_levels".to_owned()
            }
        );
        assert!(err.to_string().contains("row cap exceeded"));
        assert!(err.to_string().contains("member_levels"));

        // The cap is per table: a full table plus one row elsewhere is fine.
        contents.clear();
        for index in 0..MAX_EXPORT_ROWS_PER_TABLE {
            contents.push_str(&line("member_levels", &format!("{{\"n\":{index}}}")));
            contents.push('\n');
        }
        contents.push_str(&line("members", "{\"n\":1}"));
        contents.push('\n');
        assert_eq!(
            parse_import(&contents).unwrap().len(),
            MAX_EXPORT_ROWS_PER_TABLE as usize + 1
        );
    }

    #[test]
    fn plan_counts_rows_per_table_in_first_seen_order() {
        let rows = parse_import(&format!(
            "{}\n{}\n{}\n",
            line("member_levels", "{}"),
            line("tickets", "{}"),
            line("member_levels", "{}"),
        ))
        .unwrap();
        let plan = import_plan("dry-run", &rows);
        assert_eq!(
            plan,
            serde_json::json!({
                "version": 1,
                "mode": "dry-run",
                "tables": [
                    {"table": "member_levels", "rows": 2},
                    {"table": "tickets", "rows": 1},
                ],
                "total_rows": 3,
            })
        );
        assert_eq!(import_plan("apply", &[])["total_rows"], 0);
    }

    #[test]
    fn quote_rejects_non_static_names() {
        assert!(quote("members").is_ok());
        assert!(quote("members; DROP TABLE x").is_err());
        assert!(quote("").is_err());
    }

    #[test]
    fn import_table_order_is_parent_first() {
        assert!(import_table_order("tickets") < import_table_order("ticket_transcripts"));
        assert!(import_table_order("feed_relays") < import_table_order("feed_deliveries"));
        assert!(
            import_table_order("internal_idempotency") < import_table_order("internal_action_log")
        );
        assert!(import_table_order("voice_rooms") < import_table_order("voice_room_blocks"));
        assert!(import_table_order("voice_rooms") < import_table_order("voice_join_grants"));
        assert!(
            import_table_order("moderation_idempotency")
                < import_table_order("moderation_channel_executions")
        );
        assert!(import_table_order("lfg_posts") < import_table_order("lfg_roles"));
        assert!(import_table_order("lfg_roles") < import_table_order("lfg_signups"));
        assert_eq!(import_table_order("no_such_table"), usize::MAX);
    }

    #[test]
    fn ordered_rows_replays_parents_before_children_stably() {
        // Child listed before its parents: the parent rows still apply first.
        let rows = parse_import(&format!(
            "{}\n{}\n{}\n{}\n",
            line("ticket_transcripts", "{}"),
            line("tickets", "{\"k\":\"b\"}"),
            line("tickets", "{\"k\":\"a\"}"),
            line("members", "{}"),
        ))
        .unwrap();
        let tables: Vec<&str> = ordered_rows(&rows)
            .iter()
            .map(|row| row.table.as_str())
            .collect();
        assert_eq!(
            tables,
            vec!["members", "tickets", "tickets", "ticket_transcripts"]
        );
        // Same-table file order is preserved (stable sort).
        let ordered = ordered_rows(&rows);
        assert_eq!(
            ordered[1].row,
            serde_json::json!({"k": "b"}),
            "first tickets row keeps file order"
        );

        let rows = parse_import(&format!(
            "{}\n{}\n{}\n",
            line("voice_room_blocks", "{}"),
            line("voice_join_grants", "{}"),
            line("voice_rooms", "{}"),
        ))
        .unwrap();
        let tables: Vec<&str> = ordered_rows(&rows)
            .iter()
            .map(|row| row.table.as_str())
            .collect();
        assert_eq!(
            tables,
            vec!["voice_rooms", "voice_room_blocks", "voice_join_grants"]
        );
    }

    #[test]
    fn apply_outcome_total_sums_inserted_and_skipped() {
        let outcome = ApplyOutcome {
            inserted: 2,
            skipped: 1,
            divergent: 1,
            per_table: vec![TableOutcome {
                table: "member_levels".to_owned(),
                inserted: 2,
                skipped: 1,
                divergent: 1,
            }],
            divergent_keys: vec!["member_levels:guild_id=g member_id=m".to_owned()],
        };
        assert_eq!(outcome.total(), 3);
        assert!(outcome.divergent <= outcome.skipped);
    }

    #[test]
    fn row_key_hint_prefers_stable_identifiers() {
        assert_eq!(
            row_key_hint(&serde_json::json!({"id": "t-1", "guild_id": "g"})),
            "id=t-1"
        );
        assert_eq!(
            row_key_hint(&serde_json::json!({"lfg_id": "p", "user_id": "u", "role_key": "r"})),
            "lfg_id=p user_id=u"
        );
        assert_eq!(
            row_key_hint(&serde_json::json!({"lfg_id": "p", "role_key": "r"})),
            "lfg_id=p role_key=r"
        );
        assert_eq!(
            row_key_hint(&serde_json::json!({"guild_id": "g", "member_id": "m"})),
            "guild_id=g member_id=m"
        );
        assert_eq!(
            row_key_hint(&serde_json::json!({"ticket_id": "t", "guild_id": "g"})),
            "ticket_id=t"
        );
    }

    #[test]
    fn lfg_roles_is_exportable_as_a_parent_join() {
        assert!(is_exportable_table("lfg_posts"));
        assert!(is_exportable_table("lfg_roles"));
        assert!(is_exportable_table("lfg_signups"));
        assert!(!is_exportable_table("rank_ladder"));
        assert!(!is_exportable_table("no_such_table"));
    }
}
