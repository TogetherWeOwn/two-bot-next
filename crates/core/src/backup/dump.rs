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

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};
use sqlx::{PgPool, Row};
use thiserror::Error;

use super::dump_file::{
    finish_gzip, inspect, is_dump_table, new_encoder, write_line, DumpContents, DumpError,
    DumpManifest, DumpTableInfo, DUMP_TABLES, DUMP_VERSION,
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

/// Stable read order, so two dumps of an unchanged database are comparable.
/// (Port of legacy `orderFor`.)
fn order_for(table: &str, columns: &[String]) -> String {
    let order = match table {
        "events" => "id",
        "members" => "guild_id, member_id",
        "invite_snapshots" => "guild_id, code",
        "operational_audit_log" => "entry_id",
        "moderation_warnings" => "created_at, id",
        "moderation_scheduled_unbans" => "execute_at, request_id",
        "moderation_audit" => "created_at, request_id",
        "moderation_lockdowns" => "guild_id, channel_id",
        "moderation_idempotency" => "guild_id, idempotency_key",
        "containment_events" => "occurred_at, audit_entry_id",
        "containment_incidents" => "started_at, id",
        "join_risk_flags" => "joined_at, event_id",
        "automation_commands" => "guild_id, name",
        "scheduled_messages" => "guild_id, id",
        "sticky_messages" => "guild_id, channel_id",
        "automation_audit_log" => "created_at, id",
        "tickets" => "created_at, id",
        "ticket_transcripts" => "created_at, ticket_id",
        "automod_violations" => "guild_id, user_id",
        "automod_processed_messages" => "guild_id, message_id",
        "self_role_audit" => "created_at, event_id",
        "self_role_panel_claims" => "guild_id, member_id, panel_id",
        _ => "",
    };
    if order.is_empty() {
        columns.first().cloned().unwrap_or_else(|| "1".to_owned())
    } else {
        order.to_owned()
    }
}

/// Postgres caps a statement at 65535 bound parameters. Stay well under.
fn batch_size_for(column_count: usize) -> i64 {
    (60_000 / column_count.max(1) as i64).max(1)
}

/// Mark a dynamically built statement as manually audited (sqlx 0.9 gate).
///
/// Audit, in one place so every interpolation site shares it: each
/// interpolated identifier is either an [`is_dump_table`] allowlist member
/// (re-checked at every call site, including the restore path that reads
/// table names out of the file) or came from the target database itself
/// (`information_schema` / `pg_attribute`) and is double-quote-escaped;
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
    // The manifest records the source's applied migrations for diagnosing
    // an old backup. Only an ABSENT record table (SQLSTATE 42P01) falls back
    // to an empty list — a fresh source whose migration runner has not
    // created its ledger yet, matching legacy's `.catch(() => [])`. Any other
    // failure propagates: silently writing a manifest that claims nothing was
    // applied would lie to whoever diagnoses the restore (PR #11 review).
    let migrations: Vec<(String,)> =
        match sqlx::query_as("SELECT id::text FROM schema_migrations ORDER BY id")
            .fetch_all(&mut *tx)
            .await
        {
            Ok(rows) => rows,
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("42P01") => {
                Vec::new()
            }
            Err(e) => {
                return Err(DbDumpError::Refused(format!(
                    "cannot read schema_migrations: {e}"
                )));
            }
        };

    let manifest = DumpManifest {
        kind: "manifest".to_owned(),
        version: DUMP_VERSION,
        created_at: unix_now_iso(),
        tables,
        events_sequence: seq.0,
        schema_migrations: migrations.into_iter().map(|(id,)| id).collect(),
    };

    let mut enc = new_encoder();
    write_line(
        &mut enc,
        &serde_json::to_value(&manifest).expect("manifest serialises"),
    )?;

    let mut rows: u64 = 0;
    for table in &manifest.tables {
        let quoted: Vec<String> = table
            .columns
            .iter()
            .map(|c| format!("\"{}\"::text", c.replace('"', "\"\"")))
            .collect();
        let select_list = quoted.join(", ");
        let order = order_for(&table.name, &table.columns);
        let batch = batch_size_for(table.columns.len());
        let mut offset: i64 = 0;
        loop {
            // Table/column names are gated; values travel as bound params only
            // on the restore side. Here the only interpolation is identifiers.
            let page = sqlx::query(audited(format!(
                "SELECT {select_list} FROM {} ORDER BY {order} LIMIT {batch} OFFSET {offset}",
                table.name
            )))
            .fetch_all(&mut *tx)
            .await?;
            if page.is_empty() {
                break;
            }
            let n = page.len() as i64;
            for row in page {
                let mut data = Map::with_capacity(table.columns.len());
                for (i, col) in table.columns.iter().enumerate() {
                    let raw: Option<String> = row.try_get(i).map_err(|e| {
                        DbDumpError::Refused(format!("column {col}: cannot read as text: {e}"))
                    })?;
                    data.insert(col.clone(), raw.map(Value::String).unwrap_or(Value::Null));
                }
                write_line(
                    &mut enc,
                    &serde_json::json!({"kind": "row", "table": table.name, "data": data}),
                )?;
                rows += 1;
            }
            offset += n;
        }
    }
    tx.commit().await?;

    write_line(&mut enc, &serde_json::json!({"kind": "end", "rows": rows}))?;
    let bytes = finish_gzip(enc)?;
    std::fs::write(out_path, bytes).map_err(DumpError::from)?;
    Ok(manifest)
}

/// Post-restore verification: rows actually in each table, columns the dump
/// had that the target does not, and whether every count matched.
#[derive(Debug)]
pub struct RestoreReport {
    pub manifest: DumpManifest,
    pub restored: BTreeMap<String, u64>,
    pub dropped_columns: BTreeMap<String, Vec<String>>,
    pub ok: bool,
}

/// Replace the contents of the bot-owned tables with a dump.
///
/// Destructive by design: the tables are truncated first, so a restore
/// produces the database as it was, not a merge. It runs in one transaction,
/// so a failure part way through leaves the target exactly as it was rather
/// than half-wiped — the state you least want to discover during a recovery.
pub async fn restore(pool: &PgPool, in_path: &Path) -> Result<RestoreReport, DbDumpError> {
    let contents: DumpContents = inspect(in_path)?;
    let manifest = contents.manifest;
    let mut dropped_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let mut tx = pool.begin().await?;
    // RESTART IDENTITY so the sequence does not carry over from whatever was
    // in the target before; it is set explicitly below.
    sqlx::query(audited(format!(
        "TRUNCATE {} RESTART IDENTITY",
        DUMP_TABLES.join(", ")
    )))
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
            .map(|(c, _)| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let batch = batch_size_for(kept.len()) as usize;
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
                    // `inspect` refused every non-string/non-null cell before
                    // the transaction opened, so `as_str` here only maps
                    // JSON null to SQL NULL — it can never silently wipe a
                    // value (PR #11 review; see `inspect_bytes`).
                    let value = row.get(*col).and_then(Value::as_str).map(str::to_owned);
                    params.push(value);
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

    // Put the id sequence back past the restored high-water mark, or the
    // first write after the restore collides with a row we just put back.
    sqlx::query(
        "SELECT setval(pg_get_serial_sequence('events', 'id'), \
         GREATEST((SELECT COALESCE(MAX(id), 0) FROM events), 1), \
         (SELECT COUNT(*) FROM events) > 0)",
    )
    .execute(&mut *tx)
    .await?;
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
        ok,
    })
}
