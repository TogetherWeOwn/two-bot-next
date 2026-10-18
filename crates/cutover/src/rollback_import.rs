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
//! ignored, missing keys become NULL). Inserts use bare `ON CONFLICT DO
//! NOTHING`, so re-running an identical import is idempotent: conflicting
//! rows keep their existing values. Divergent-value conflicts and serial-PK
//! sequence advancement are deliberately out of scope.

use std::collections::BTreeMap;

use sqlx::PgPool;
use thiserror::Error;

use crate::rollback_delta::{TableMeasure, MAX_EXPORT_ROWS_PER_TABLE, TABLE_SPECS};

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
/// these redacted messages.
#[derive(Debug, Error)]
pub enum ImportApplyError {
    #[error("database error (details omitted)")]
    Database(#[from] sqlx::Error),
    #[error("invalid table name in validated import")]
    InvalidTable(String),
}

/// True when the exporter can emit rows for `table`: a measured table (one
/// with timestamp columns), never an unmeasurable or absent one.
#[must_use]
pub fn is_exportable_table(table: &str) -> bool {
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
    let mut per_table: BTreeMap<&str, u64> = BTreeMap::new();
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
        let count = per_table.entry(table).or_insert(0);
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

/// Replay validated rows into the target inside one transaction. Each row is
/// mapped back onto its table's columns with `jsonb_populate_record` and
/// inserted with bare `ON CONFLICT DO NOTHING`, so an identical re-run
/// changes nothing. Returns the number of import rows processed.
pub async fn apply_import(pool: &PgPool, rows: &[ImportRow]) -> Result<u64, ImportApplyError> {
    let mut tx = pool.begin().await?;
    for row in rows {
        let table = quote(&row.table)?;
        // Table names are validated statics; only the row document is bound.
        let sql = format!(
            "INSERT INTO {table} SELECT * FROM jsonb_populate_record(NULL::{table}, $1) ON CONFLICT DO NOTHING"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&row.row)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(rows.len() as u64)
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
}
