//! Live dump/restore against Postgres (needs the crate `db` feature).
//!
//! Port of the database half of legacy `src/store/dump.ts`.
//!
//! ## Value encoding
//!
//! Legacy stored native JSON values per cell. This port stores Postgres
//! text-output form (`SELECT "col"::text`, one `Option<String>` per cell)
//! with the column type recorded per table in the manifest, and restores
//! with a `$n::type` cast per column. Postgres text output → input is the
//! canonical round trip for every type the bot owns (text, integers,
//! timestamptz, booleans, json/jsonb, bytea hex, arrays, enums, intervals),
//! so fidelity holds without per-type Rust decoding, and the dump reader
//! never interpolates untrusted bytes into SQL — only table/column names,
//! which are gated by [`is_dump_table`] and re-checked at interpolation.
//!
//! ## Snapshot isolation
//!
//! The dump runs in a single `REPEATABLE READ` transaction: every table is
//! read as of the same instant, so the bot does not have to be stopped to
//! take a backup.
//!
//! ## Allocators
//!
//! Sequences are not transactional, so the dump records each serial/identity
//! column's high-water mark in the manifest's `sequences`. Restore sets every
//! restored table's allocator to the highest of the target's own position,
//! the restored rows and that mark: it never rewinds, so an ID handed out
//! before the restore is never handed out again (cutover allocator gate).

use std::collections::BTreeMap;
use std::path::Path;

use futures_util::TryStreamExt;
use serde_json::{Map, Value};
use sqlx::{PgPool, Row};
use thiserror::Error;

use super::dump_file::{
    cell_input, inspect, is_dump_table, DumpContents, DumpError, DumpManifest, DumpTableInfo,
    DumpWriter, DUMP_TABLES, DUMP_VERSION,
};
use super::guild_config::unix_now_iso;

/// Database-side dump/restore failure.
#[derive(Debug, Error)]
pub enum DbDumpError {
    #[error("dump file: {0}")]
    File(#[from] DumpError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Refused(String),
}

/// Postgres caps a statement at 65535 bound parameters. Stay well under.
///
/// This bounds multi-row INSERT pages on the *restore* path only. It must
/// never size the *dump* SELECT: a bind-parameter ceiling is not a memory
/// budget, and buffering a whole page before the bounded writer sees any row
/// lets multi-megabyte source cells exhaust memory before the decoded-line
/// refusal (TOG-9970 finding 5). The dump streams row-by-row instead.
fn batch_size_for(column_count: usize) -> usize {
    (60_000 / column_count.max(1) as i64).max(1) as usize
}

/// Mark a dynamically built statement as manually audited (sqlx 0.9 gate).
///
/// Audit, in one place so every interpolation site shares it: each
/// interpolated identifier is either an [`is_dump_table`] allowlist member
/// (re-checked at every call site, including the restore path that reads
/// table names out of the file) or came from the target database itself
/// (`information_schema` / `pg_attribute` names, double-quote-escaped by
/// [`ident`]; `pg_get_serial_sequence` output, which Postgres quotes);
/// limits/offsets are integers; row values travel only as bound parameters.
/// File-supplied *values* never reach this function.
fn audited(sql: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql)
}

/// Live columns + `format_type()` spellings, in ordinal order.
async fn columns_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<Vec<(String, String)>, DbDumpError> {
    let rows = sqlx::query(
        "SELECT column_name, format_type(atttypid, atttypmod) AS type_name \
         FROM information_schema.columns \
         JOIN pg_attribute ON attrelid = (quote_ident(table_schema) || '.' || quote_ident(table_name))::regclass \
           AND attname = column_name \
         WHERE table_schema = current_schema() AND table_name = $1 \
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("column_name"),
                r.get::<String, _>("type_name"),
            )
        })
        .collect())
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Primary-key columns in key order: the stable read order, so two dumps of
/// an unchanged database are comparable. Every owned table has one.
async fn primary_key_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<Vec<String>, DbDumpError> {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname::text FROM pg_index i \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
         WHERE i.indrelid = to_regclass(format('%I.%I', current_schema(), $1::text)) \
           AND i.indisprimary \
         ORDER BY array_position(i.indkey::int2[], a.attnum)",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await?;
    if columns.is_empty() {
        return Err(DbDumpError::Refused(format!(
            "table {table} has no primary key, so it has no stable dump order"
        )));
    }
    Ok(columns)
}

/// The table's serial/identity column and its sequence, if it has one.
/// The manifest keeps one mark per table, so a second one is refused.
async fn serial_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<Option<(String, String)>, DbDumpError> {
    let mut found: Vec<(String, String)> = sqlx::query_as(
        "SELECT attname::text, seq FROM ( \
           SELECT a.attname, a.attnum, \
             pg_get_serial_sequence(format('%I.%I', current_schema(), $1::text), a.attname) AS seq \
           FROM pg_attribute a \
           WHERE a.attrelid = to_regclass(format('%I.%I', current_schema(), $1::text)) \
             AND a.attnum > 0 AND NOT a.attisdropped \
         ) s WHERE seq IS NOT NULL ORDER BY attnum",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await?;
    if found.len() > 1 {
        return Err(DbDumpError::Refused(format!(
            "table {table} has {} allocator columns; the manifest records one per table",
            found.len()
        )));
    }
    Ok(found.pop())
}

async fn count_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<i64, DbDumpError> {
    // Table name is gated by is_dump_table at every call site.
    let row: (i64,) = sqlx::query_as(audited(format!("SELECT COUNT(*) FROM {table}")))
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.0)
}

/// Write every bot-owned table to `out_path` as one consistent snapshot.
pub async fn dump(pool: &PgPool, out_path: &Path) -> Result<DumpManifest, DbDumpError> {
    let mut tx = pool.begin().await?;
    // Must be the first statement in the transaction, before any query.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;

    let mut tables: Vec<DumpTableInfo> = Vec::with_capacity(DUMP_TABLES.len());
    let mut orders: BTreeMap<&str, String> = BTreeMap::new();
    let mut sequences: BTreeMap<String, i64> = BTreeMap::new();
    for name in DUMP_TABLES {
        if !is_dump_table(name) {
            return Err(DbDumpError::Refused(format!("{name} is not a dump table")));
        }
        let cols = columns_of(&mut tx, name).await?;
        if cols.is_empty() {
            return Err(DbDumpError::Refused(format!(
                "table {name} does not exist in the target — run migrations first (S6 owns the schema)"
            )));
        }
        let count = count_of(&mut tx, name).await? as u64;
        let key = primary_key_of(&mut tx, name).await?;
        orders.insert(
            *name,
            key.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", "),
        );
        if let Some((column, seq)) = serial_of(&mut tx, name).await? {
            // pg_sequences reports NULL before first use (and without
            // privilege); the snapshot's own rows are then the floor.
            let (mark,): (i64,) = sqlx::query_as(audited(format!(
                "SELECT GREATEST( \
                   COALESCE((SELECT s.last_value FROM pg_sequences s \
                     JOIN pg_class c ON c.relname = s.sequencename \
                     JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = s.schemaname \
                     WHERE c.oid = $1::regclass), 0), \
                   (SELECT COALESCE(MAX({})::bigint, 0) FROM {name}))",
                ident(&column)
            )))
            .bind(&seq)
            .fetch_one(&mut *tx)
            .await?;
            sequences.insert((*name).to_owned(), mark);
        }
        tables.push(DumpTableInfo {
            name: (*name).to_owned(),
            columns: cols.iter().map(|(c, _)| c.clone()).collect(),
            column_types: cols.iter().map(|(_, t)| t.clone()).collect(),
            count,
        });
    }

    let seq: (i64,) = sqlx::query_as("SELECT COALESCE(MAX(id), 0) FROM events")
        .fetch_one(&mut *tx)
        .await?;
    // An absent ledger is valid on a fresh source. Check before selecting:
    // catching 42P01 inside this transaction would leave every later query
    // aborted (25P02). Errors from an existing ledger still propagate.
    let (has_ledger,): (bool,) =
        sqlx::query_as("SELECT to_regclass('schema_migrations') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    let migrations: Vec<(String,)> = if has_ledger {
        sqlx::query_as("SELECT id::text FROM schema_migrations ORDER BY id")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| DbDumpError::Refused(format!("cannot read schema_migrations: {e}")))?
    } else {
        Vec::new()
    };

    let manifest = DumpManifest {
        kind: "manifest".to_owned(),
        version: DUMP_VERSION,
        created_at: unix_now_iso(),
        tables,
        events_sequence: seq.0,
        schema_migrations: migrations.into_iter().map(|(id,)| id).collect(),
        sequences,
    };

    let mut writer = DumpWriter::new(out_path)?;
    writer.write_line(&serde_json::to_value(&manifest).expect("manifest serialises"))?;

    let mut rows: u64 = 0;
    for table in &manifest.tables {
        let quoted: Vec<String> = table
            .columns
            .iter()
            .map(|c| format!("{}::text", ident(c)))
            .collect();
        let select_list = quoted.join(", ");
        let order = &orders[table.name.as_str()];
        // Stream row-by-row: each row passes through the bounded writer
        // (8 MiB decoded-line cap, cumulative budgets) BEFORE the next row
        // is materialised. An early oversized row refuses before later rows
        // are read; a row-count ceiling never sizes this SELECT. Table/column
        // names are gated; no values are interpolated here.
        let mut stream = sqlx::query(audited(format!(
            "SELECT {select_list} FROM {} ORDER BY {order}",
            table.name
        )))
        .fetch(&mut *tx);
        while let Some(row) = stream.try_next().await? {
            let mut data = Map::with_capacity(table.columns.len());
            for (i, col) in table.columns.iter().enumerate() {
                let raw: Option<String> = row.try_get(i).map_err(|e| {
                    DbDumpError::Refused(format!("column {col}: cannot read as text: {e}"))
                })?;
                data.insert(col.clone(), raw.map(Value::String).unwrap_or(Value::Null));
            }
            writer.write_line(
                &serde_json::json!({"kind": "row", "table": table.name, "data": data}),
            )?;
            rows += 1;
        }
    }
    tx.commit().await?;

    writer.write_line(&serde_json::json!({"kind": "end", "rows": rows}))?;
    // Only a finished, synced archive accepted by restore's reader becomes a
    // final backup. Errors return before CLI retention or upload can run.
    writer.publish()?;
    Ok(manifest)
}

/// Post-restore verification: rows actually in each table, columns the dump
/// had that the target does not, and whether every count matched.
#[derive(Debug)]
pub struct RestoreReport {
    pub manifest: DumpManifest,
    pub restored: BTreeMap<String, u64>,
    pub dropped_columns: BTreeMap<String, Vec<String>>,
    /// Restored table -> its allocator's high-water mark after the restore
    /// (the next default ID is above it). Tables without one are absent.
    pub allocators: BTreeMap<String, i64>,
    pub ok: bool,
}

/// Replace the contents of the dump's tables with the dump.
///
/// Destructive by design: the tables are truncated first, so a restore
/// produces their data as it was, not a merge. Tables outside the dump (see
/// [`super::dump_file::NOT_DUMPED`]) are left alone. It runs in one
/// transaction, so a failure part way through leaves the target exactly as it
/// was rather than half-wiped — the state you least want to discover during a
/// recovery. The target schema must already exist: this never migrates.
pub async fn restore(pool: &PgPool, in_path: &Path) -> Result<RestoreReport, DbDumpError> {
    let contents: DumpContents = inspect(in_path)?;
    let manifest = contents.manifest;
    let mut dropped_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut allocators: BTreeMap<String, i64> = BTreeMap::new();
    let names: Vec<String> = manifest.tables.iter().map(|t| t.name.clone()).collect();
    // Re-checked before any of these names is interpolated below.
    if let Some(name) = names.iter().find(|n| !is_dump_table(n)) {
        return Err(DbDumpError::Refused(format!(
            "manifest table {name:?} is not a dump table"
        )));
    }

    let mut tx = pool.begin().await?;
    let absent: Vec<String> = sqlx::query_scalar(
        "SELECT t FROM unnest($1::text[]) AS t \
         WHERE to_regclass(format('%I.%I', current_schema(), t)) IS NULL ORDER BY t",
    )
    .bind(&names)
    .fetch_all(&mut *tx)
    .await?;
    if !absent.is_empty() {
        return Err(DbDumpError::Refused(format!(
            "target lacks {}: migrate it first (restore never creates tables; \
             a legacy-format dump restores only into a legacy-shaped schema)",
            absent.join(", ")
        )));
    }
    // A table outside the dump that references one inside would block the
    // TRUNCATE (or, with CASCADE, be silently emptied). Name it instead.
    let orphaned: Vec<(String, String, String)> = sqlx::query_as(
        "WITH dumped AS ( \
           SELECT to_regclass(format('%I.%I', current_schema(), t)) AS rel \
           FROM unnest($1::text[]) AS t) \
         SELECT c.conrelid::regclass::text, c.confrelid::regclass::text, c.conname::text \
         FROM pg_constraint c \
         WHERE c.contype = 'f' \
           AND c.confrelid IN (SELECT rel FROM dumped) \
           AND c.conrelid NOT IN (SELECT rel FROM dumped) \
         ORDER BY 1, 3",
    )
    .bind(&names)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((from, to, constraint)) = orphaned.first() {
        return Err(DbDumpError::Refused(format!(
            "{from} references {to} ({constraint}) but is not in this dump; \
             refusing rather than orphaning or emptying it"
        )));
    }
    // No RESTART IDENTITY: allocators only move forward, set below.
    sqlx::query(audited(format!("TRUNCATE {}", names.join(", "))))
        .execute(&mut *tx)
        .await?;

    for table in &manifest.tables {
        // Re-checked at the point of interpolation, not just at parse time:
        // this is the line that builds SQL from file-supplied text.
        if !is_dump_table(&table.name) {
            return Err(DbDumpError::Refused(format!(
                "manifest table {:?} is not a dump table",
                table.name
            )));
        }
        let empty = Vec::new();
        let rows = contents.buffers.get(&table.name).unwrap_or(&empty);
        if rows.is_empty() {
            continue;
        }
        let target: Vec<(String, String)> = columns_of(&mut tx, &table.name).await?;
        let target_names: Vec<&str> = target.iter().map(|(c, _)| c.as_str()).collect();
        let target_types: BTreeMap<&str, &str> = target
            .iter()
            .map(|(c, t)| (c.as_str(), t.as_str()))
            .collect();
        // Dumps written by legacy two-bot carry no column types; the
        // target's own type is the safe fallback (text parses everywhere).
        let kept: Vec<(&String, &str)> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c,
                    table
                        .column_types
                        .get(i)
                        .map(String::as_str)
                        .unwrap_or("text"),
                )
            })
            .filter(|(c, _)| target_names.contains(&c.as_str()))
            .collect();
        let dropped: Vec<String> = table
            .columns
            .iter()
            .filter(|c| !target_names.contains(&c.as_str()))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            dropped_columns.insert(table.name.clone(), dropped);
        }
        if kept.is_empty() {
            return Err(DbDumpError::Refused(format!(
                "{}: no columns in common with the target",
                table.name
            )));
        }

        let quoted = kept
            .iter()
            .map(|(c, _)| ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let batch = batch_size_for(kept.len());
        for page in rows.chunks(batch) {
            // One multi-row INSERT per page: VALUES ($1::t1, $2::t2), ...
            let mut sql = format!("INSERT INTO {} ({quoted}) VALUES ", table.name);
            let mut params: Vec<Option<String>> = Vec::with_capacity(page.len() * kept.len());
            let mut placeholders: Vec<String> = Vec::with_capacity(page.len());
            let mut index = 1;
            for row in page {
                let mut tuple = Vec::with_capacity(kept.len());
                for (col, _dump_type) in &kept {
                    // Cast to the TARGET's type: the target schema wins over
                    // whatever the dump was taken from.
                    let target_type = target_types.get(col.as_str()).copied().unwrap_or("text");
                    tuple.push(format!("${index}::{target_type}"));
                    index += 1;
                    let cell = row.get(*col).ok_or_else(|| {
                        DbDumpError::Refused(format!("{}: missing cell {col}", table.name))
                    })?;
                    params.push(cell_input(
                        cell,
                        target_type,
                        !table.column_types.is_empty(),
                    )?);
                }
                placeholders.push(format!("({})", tuple.join(", ")));
            }
            sql.push_str(&placeholders.join(", "));
            let mut query = sqlx::query(audited(sql));
            for param in params {
                query = query.bind(param);
            }
            query.execute(&mut *tx).await?;
        }
    }

    // Put every allocator past the restored rows, or the first write after
    // the restore collides with a row we just put back. Never below the
    // target's own position or the dump's mark: an ID already handed out
    // (and perhaps referenced outside this database) is not reissued.
    for name in &names {
        let Some((column, seq)) = serial_of(&mut tx, name).await? else {
            continue;
        };
        let mut mark = manifest.sequences.get(name).copied().unwrap_or(0);
        if name == "events" {
            mark = mark.max(manifest.events_sequence);
        }
        let (high,): (i64,) = sqlx::query_as(audited(format!(
            "SELECT GREATEST( \
               (SELECT CASE WHEN is_called THEN last_value ELSE last_value - 1 END FROM {seq}), \
               (SELECT COALESCE(MAX({})::bigint, 0) FROM {name}), \
               $1::bigint)",
            ident(&column)
        )))
        .bind(mark)
        .fetch_one(&mut *tx)
        .await?;
        if high >= 1 {
            sqlx::query("SELECT setval($1::regclass, $2, true)")
                .bind(&seq)
                .bind(high)
                .execute(&mut *tx)
                .await?;
        }
        allocators.insert(name.clone(), high);
    }
    tx.commit().await?;

    let mut restored = BTreeMap::new();
    let mut ok = true;
    for table in &manifest.tables {
        if !is_dump_table(&table.name) {
            return Err(DbDumpError::Refused(format!(
                "manifest table {:?} is not a dump table",
                table.name
            )));
        }
        let row: (i64,) = sqlx::query_as(audited(format!("SELECT COUNT(*) FROM {}", table.name)))
            .fetch_one(pool)
            .await?;
        let count = row.0 as u64;
        if count != table.count {
            ok = false;
        }
        restored.insert(table.name.clone(), count);
    }

    Ok(RestoreReport {
        manifest,
        restored,
        dropped_columns,
        allocators,
        ok,
    })
}
