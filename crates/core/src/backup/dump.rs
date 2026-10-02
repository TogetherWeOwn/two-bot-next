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

use futures_util::TryStreamExt;
use serde_json::{Map, Value};
use sqlx::{PgPool, Row};
use thiserror::Error;

use super::dump_file::{
    cell_input, inspect, is_destination_owned, is_dump_table, DumpContents, DumpError,
    DumpManifest, DumpTableInfo, DumpWriter, SequenceMark, DUMP_TABLES, DUMP_VERSION,
    OPTIONAL_LEGACY_TABLES,
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

/// Total, locale-independent order even for composite keys or tables without
/// a primary key. Text output is the archive's representation; equal sort keys
/// therefore mean identical archived rows (including JSON and binary columns).
fn order_for(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| format!("\"{}\"::text COLLATE \"C\"", c.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ")
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

/// Allocators not OWNED BY a column, keyed by the column they feed.
const STANDALONE_SEQUENCES: &[(&str, &str, &str)] = &[
    ("guild_settings", "version", "guild_settings_version_seq"),
    ("guild_settings", "cas_version", "guild_settings_cas_seq"),
];

/// `(column, sequence)` for every serial AND identity sequence owned by a
/// column of `table`, including columns not named `id`, from the catalog
/// rather than a hardcoded list or archive-supplied names. Standalone
/// allocators that exist follow.
/// https://www.postgresql.org/docs/16/functions-info.html
async fn table_sequences(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
) -> Result<Vec<(String, String)>, DbDumpError> {
    let mut sequences: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text, pg_get_serial_sequence(\
         quote_ident(table_schema) || '.' || quote_ident(table_name), column_name) \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 \
         AND pg_get_serial_sequence(\
         quote_ident(table_schema) || '.' || quote_ident(table_name), column_name) IS NOT NULL \
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await?;
    for (owner, column, sequence) in STANDALONE_SEQUENCES {
        if *owner != table {
            continue;
        }
        let (exists,): (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
            .bind(*sequence)
            .fetch_one(&mut **tx)
            .await?;
        if exists {
            sequences.push(((*column).to_owned(), (*sequence).to_owned()));
        }
    }
    Ok(sequences)
}

/// A sequence's definition and live position. Sequences are not MVCC: inside
/// the dump's snapshot this reads the current position, which is at or beyond
/// every value the snapshot can contain.
struct SequenceState {
    quoted: String,
    start: i64,
    increment: i64,
    min: i64,
    max: i64,
    last_value: i64,
    is_called: bool,
}

async fn sequence_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sequence: &str,
) -> Result<SequenceState, DbDumpError> {
    let (quoted, start, increment, min, max): (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT quote_ident(n.nspname) || '.' || quote_ident(c.relname), \
         s.seqstart, s.seqincrement, s.seqmin, s.seqmax \
         FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE s.seqrelid = $1::regclass",
    )
    .bind(sequence)
    .fetch_one(&mut **tx)
    .await?;
    // `quoted` is the catalog's own quote_ident spelling.
    let (last_value, is_called): (i64, bool) = sqlx::query_as(audited(format!(
        "SELECT last_value, is_called FROM {quoted}"
    )))
    .fetch_one(&mut **tx)
    .await?;
    Ok(SequenceState {
        quoted,
        start,
        increment,
        min,
        max,
        last_value,
        is_called,
    })
}

/// Restart transactionally rather than using setval (which survives rollback),
/// at the furthest of: the configured start, one step past the restored rows,
/// the archive's mark and the target's own position. A restore therefore never
/// reissues a value the source or the target already handed out, including
/// one whose row was deleted (the cutover allocator gate).
/// Catalog identifiers are quoted by Postgres; restart values are checked i64s.
/// https://www.postgresql.org/docs/16/sql-altersequence.html
async fn restart_sequence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    column: &str,
    sequence: &str,
    mark: Option<&SequenceMark>,
) -> Result<(), DbDumpError> {
    let state = sequence_state(tx, sequence).await?;
    let exhausted =
        || DbDumpError::Refused(format!("{table}.{column}: restored sequence exhausted"));
    let quoted_column = format!("\"{}\"", column.replace('"', "\"\""));
    let ascending = state.increment > 0;
    let aggregate = if ascending { "MAX" } else { "MIN" };
    let (edge,): (Option<i64>,) = sqlx::query_as(audited(format!(
        "SELECT {aggregate}({quoted_column})::bigint FROM {table}"
    )))
    .fetch_one(&mut **tx)
    .await?;
    let own = if state.is_called {
        state.last_value.checked_add(state.increment)
    } else {
        Some(state.last_value)
    };
    let mut candidates = vec![state.start, own.ok_or_else(exhausted)?];
    if let Some(edge) = edge {
        candidates.push(edge.checked_add(state.increment).ok_or_else(exhausted)?);
    }
    if let Some(mark) = mark {
        if mark.increment.signum() != state.increment.signum() {
            return Err(DbDumpError::Refused(format!(
                "{table}.{column}: archived sequence mark runs the other way from the target"
            )));
        }
        candidates.push(mark.next().ok_or_else(exhausted)?);
    }
    let next = if ascending {
        candidates.into_iter().max()
    } else {
        candidates.into_iter().min()
    }
    .ok_or_else(exhausted)?;
    if !(state.min..=state.max).contains(&next) {
        return Err(exhausted());
    }
    sqlx::query(audited(format!(
        "ALTER SEQUENCE {} RESTART WITH {next}",
        state.quoted
    )))
    .execute(&mut **tx)
    .await?;
    Ok(())
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
            if OPTIONAL_LEGACY_TABLES.contains(name) {
                continue;
            }
            return Err(DbDumpError::Refused(format!(
                "table {name} does not exist in the target — run migrations first (S6 owns the schema)"
            )));
        }
        let count = count_of(&mut tx, name).await? as u64;
        let cols: Vec<_> = cols
            .into_iter()
            .filter(|(c, _)| !is_destination_owned(name, c))
            .collect();
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

    // Rows alone cannot show values handed out and then deleted; the marks
    // let restore resume past them (the cutover allocator gate).
    let mut sequence_marks = Vec::new();
    for table in &tables {
        for (column, sequence) in table_sequences(&mut tx, &table.name).await? {
            let state = sequence_state(&mut tx, &sequence).await?;
            sequence_marks.push(SequenceMark {
                table: table.name.clone(),
                column,
                last_value: state.last_value,
                is_called: state.is_called,
                increment: state.increment,
            });
        }
    }

    let manifest = DumpManifest {
        kind: "manifest".to_owned(),
        version: DUMP_VERSION,
        created_at: unix_now_iso(),
        tables,
        events_sequence: seq.0,
        schema_migrations: migrations.into_iter().map(|(id,)| id).collect(),
        sequence_marks,
    };

    let mut writer = DumpWriter::new(out_path)?;
    writer.write_line(&serde_json::to_value(&manifest).expect("manifest serialises"))?;

    let mut rows: u64 = 0;
    for table in &manifest.tables {
        let quoted: Vec<String> = table
            .columns
            .iter()
            .map(|c| format!("\"{}\"::text", c.replace('"', "\"\"")))
            .collect();
        let select_list = quoted.join(", ");
        let order = order_for(&table.columns);
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
    /// Schema-required baseline rows initialized when absent from an old dump.
    pub initialized_tables: BTreeMap<String, u64>,
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
    let missing = manifest.missing_tables();
    if !missing.is_empty() {
        tracing::warn!(version = manifest.version, tables = ?missing, "old dump lacks tables; restore clears them (settings revision singleton resets to zero)");
    }
    let mut dropped_columns: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let mut tx = pool.begin().await?;
    let mut target_tables = Vec::new();
    for name in DUMP_TABLES {
        if columns_of(&mut tx, name).await?.is_empty() {
            let count = manifest
                .tables
                .iter()
                .find(|t| t.name == *name)
                .map_or(0, |t| t.count);
            if !OPTIONAL_LEGACY_TABLES.contains(name) || count != 0 {
                return Err(DbDumpError::Refused(format!(
                    "table {name} is absent from target; refusing to lose {count} archived rows — provision the matching schema first"
                )));
            }
        } else {
            target_tables.push(*name);
        }
    }
    // Lock before changing guards. Only two named application triggers are
    // suspended, never FK/check constraints or session_replication_role. ALTER
    // TABLE is transactional: error/cancellation rolls back data AND guards.
    // trg_guild_settings_version stays enabled: it allocates fresh CAS tokens.
    // https://www.postgresql.org/docs/16/sql-altertable.html
    sqlx::query(audited(format!(
        "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
        target_tables.join(", ")
    )))
    .execute(&mut *tx)
    .await?;
    let mut suspended = Vec::new();
    for (table, trigger) in [
        ("guild_settings", "trg_guild_settings_revision"),
        (
            "guild_settings_audit",
            "trg_guild_settings_audit_append_only",
        ),
    ] {
        let enabled: Option<(String,)> = sqlx::query_as(
            "SELECT tgenabled::text FROM pg_trigger \
             WHERE tgrelid = to_regclass($1) AND tgname = $2 AND NOT tgisinternal",
        )
        .bind(table)
        .bind(trigger)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((state,)) = enabled {
            sqlx::query(audited(format!(
                "ALTER TABLE {table} DISABLE TRIGGER {trigger}"
            )))
            .execute(&mut *tx)
            .await?;
            suspended.push((table, trigger, state));
        }
    }
    // A single explicit truncate covers all FK dependencies, without CASCADE
    // touching foreign/website tables. No RESTART IDENTITY: that would discard
    // the target's own high-water mark, which the restarts below preserve.
    sqlx::query(audited(format!("TRUNCATE {}", target_tables.join(", "))))
        .execute(&mut *tx)
        .await?;
    // Cooldowns are intentionally not archived; discard stale target throttles.
    if !columns_of(&mut tx, "xp_cooldowns").await?.is_empty() {
        sqlx::query("TRUNCATE xp_cooldowns")
            .execute(&mut *tx)
            .await?;
    }
    if manifest.sequence_marks.is_empty() {
        tracing::warn!(version = manifest.version, "archive carries no sequence marks; allocators resume past the restored rows and the target's own position only");
    }
    // Destination-owned columns (CAS tokens) are allocated by the inserts
    // below. Move their allocator past the archive's mark first, so no restored
    // row receives a token a client of the source may still hold.
    for &table in &target_tables {
        for (column, sequence) in table_sequences(&mut tx, table).await? {
            if is_destination_owned(table, &column) {
                restart_sequence(
                    &mut tx,
                    table,
                    &column,
                    &sequence,
                    manifest.sequence_mark(table, &column),
                )
                .await?;
            }
        }
    }

    // Never trust manifest order: a legacy or reordered file can put children
    // before parents. The same allowlist controls dump and restore FK order.
    for name in DUMP_TABLES {
        let Some(table) = manifest.tables.iter().find(|t| t.name == *name) else {
            continue;
        };
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
        // Destination-owned columns are allocated by the target, so an archived
        // value is skipped rather than restored or reported as dropped.
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
            .filter(|(c, _)| {
                target_names.contains(&c.as_str()) && !is_destination_owned(&table.name, c)
            })
            .collect();
        let dropped: Vec<String> = table
            .columns
            .iter()
            .filter(|c| {
                !target_names.contains(&c.as_str()) && !is_destination_owned(&table.name, c)
            })
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
        let batch = batch_size_for(kept.len());
        for page in rows.chunks(batch) {
            // One multi-row INSERT per page: VALUES ($1::t1, $2::t2), ...
            // Identity GENERATED ALWAYS columns need explicit-value restore too.
            // https://www.postgresql.org/docs/16/sql-insert.html
            let mut sql = format!(
                "INSERT INTO {} ({quoted}) OVERRIDING SYSTEM VALUE VALUES ",
                table.name
            );
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

    // v3 predates the required singleton. Clearing stale target data is not
    // enough: settings writes would subsequently fail without this baseline.
    // Never synthesize application settings/history or replace an archived row.
    let mut initialized_tables = BTreeMap::new();
    if missing.contains(&"guild_settings_revision") {
        let columns = columns_of(&mut tx, "guild_settings_revision").await?;
        if columns.iter().any(|(name, _)| name == "singleton")
            && columns.iter().any(|(name, _)| name == "revision")
        {
            sqlx::query(
                "INSERT INTO guild_settings_revision (singleton, revision) VALUES (TRUE, 0)",
            )
            .execute(&mut *tx)
            .await?;
            initialized_tables.insert("guild_settings_revision".to_owned(), 1);
        }
    }

    // Preserve origin/always/replica/disabled modes, not just enabled vs disabled.
    // https://www.postgresql.org/docs/16/catalog-pg-trigger.html
    for (table, trigger, state) in suspended {
        let action = match state.as_str() {
            "O" => "ENABLE",
            "A" => "ENABLE ALWAYS",
            "R" => "ENABLE REPLICA",
            "D" => "DISABLE",
            _ => return Err(DbDumpError::Refused("unknown trigger state".to_owned())),
        };
        sqlx::query(audited(format!(
            "ALTER TABLE {table} {action} TRIGGER {trigger}"
        )))
        .execute(&mut *tx)
        .await?;
    }

    // Every other allocator (owned serial/identity and the standalone
    // settings-version sequence) resumes past the restored rows, the
    // archive's mark and the target's own position.
    for &table in &target_tables {
        for (column, sequence) in table_sequences(&mut tx, table).await? {
            if !is_destination_owned(table, &column) {
                restart_sequence(
                    &mut tx,
                    table,
                    &column,
                    &sequence,
                    manifest.sequence_mark(table, &column),
                )
                .await?;
            }
        }
    }
    tx.commit().await?;

    let mut restored = BTreeMap::new();
    let mut ok = true;
    for table in &target_tables {
        let row: (i64,) = sqlx::query_as(audited(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await?;
        let count = row.0 as u64;
        let expected = manifest
            .tables
            .iter()
            .find(|t| t.name == *table)
            .map_or_else(
                || initialized_tables.get(*table).copied().unwrap_or(0),
                |t| t.count,
            );
        if count != expected {
            ok = false;
        }
        restored.insert((*table).to_owned(), count);
    }

    Ok(RestoreReport {
        manifest,
        restored,
        dropped_columns,
        initialized_tables,
        ok,
    })
}
