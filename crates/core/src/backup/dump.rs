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
/// `sources` are every restored (table, column) allocated from `sequence`; the
/// first names it in refusals and supplies the archive mark. It resumes past
/// the edge across all of them.
/// <https://www.postgresql.org/docs/16/sql-altersequence.html>
async fn restart_sequence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sources: &[(&str, &str)],
    sequence: &str,
    mark: Option<&SequenceMark>,
) -> Result<(), DbDumpError> {
    let state = sequence_state(tx, sequence).await?;
    // The first source names the allocator in refusals and supplies the
    // archive mark; later sources only widen the restored edge (0113 draws
    // retry tickets from the ban-ownership sequence without owning a column).
    let (table, column) = sources[0];
    let exhausted =
        || DbDumpError::Refused(format!("{table}.{column}: restored sequence exhausted"));
    let ascending = state.increment > 0;
    let aggregate = if ascending { "MAX" } else { "MIN" };
    let mut edge: Option<i64> = None;
    for (source_table, source_column) in sources {
        let quoted_column = format!("\"{}\"", source_column.replace('"', "\"\""));
        let (value,): (Option<i64>,) = sqlx::query_as(audited(format!(
            "SELECT {aggregate}({quoted_column})::bigint FROM {source_table}"
        )))
        .fetch_one(&mut **tx)
        .await?;
        edge = match (edge, value) {
            (Some(a), Some(b)) if ascending => Some(a.max(b)),
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
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
    // The cutover ledger is the one restore gates behavior on (see the 0469
    // replay): only a source that had not applied 0469 holds authoritative
    // voice restrictions in the legacy tables. Same absent-ledger care as
    // above; only successful applications count as applied.
    let (has_cutover_ledger,): (bool,) =
        sqlx::query_as("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    let cutover_migrations: Vec<(i64,)> = if has_cutover_ledger {
        sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| DbDumpError::Refused(format!("cannot read _sqlx_migrations: {e}")))?
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
        cutover_migrations: cutover_migrations
            .into_iter()
            .map(|(version,)| version)
            .collect(),
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
    /// Old v3 has no acceptance/generation evidence. Never infer acceptance.
    pub missing_member_ban_ownership: bool,
    /// All executable expiry rows made non-executable for reconciliation.
    pub quarantined_unbans: u64,
    /// Legacy `voice_command_roles` rows consumed by the restore-time replay
    /// of the 0469 translation into `voice_access_controls.command_roles`.
    /// Zero when the archive held no legacy restrictions.
    pub translated_voice_command_roles: u64,
    /// Legacy `voice_command_role_members` rows consumed by the same replay.
    pub translated_voice_command_role_members: u64,
    /// Guilds whose creation switch or guild-wide command role the
    /// restore-time replay of the 0470 translation tightened in
    /// `voice_access_controls`, from values only the legacy
    /// `voice_guild_settings` row carried. Zero when the archive proves 0470
    /// was applied or the two stores already agreed.
    pub translated_voice_creation_gates: u64,
    /// `voice_access_controls` rows the 0469/0470 replays inserted for
    /// guilds whose restrictions only the legacy stores held.
    pub voice_access_controls_added: u64,
    pub ok: bool,
}

impl RestoreReport {
    /// Rows the count check expects in `table` after this restore: the
    /// manifest count, adjusted for rows restore itself initialized or
    /// moved. Retired archive tables are consumed without restoring them;
    /// the 0469/0470 translations can add authority guild rows.
    pub fn expected_count(&self, table: &str) -> u64 {
        let archived = self
            .manifest
            .tables
            .iter()
            .find(|t| t.name == table)
            .map_or_else(
                || self.initialized_tables.get(table).copied().unwrap_or(0),
                |t| t.count,
            );
        match table {
            "voice_command_roles" | "voice_command_role_members" => 0,
            "voice_access_controls" => archived + self.voice_access_controls_added,
            _ => archived,
        }
    }
}

/// Historical archive adapter only: these relations no longer exist after
/// 0476. Decode and validate their 0229 row shape without creating, querying or
/// writing a legacy relation. A post-0469 mirror is stale, never authority.
#[derive(Default)]
struct LegacyVoiceRestrictions {
    by_guild: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    commands: u64,
    members: u64,
}

fn legacy_voice_restrictions(
    contents: &DumpContents,
) -> Result<LegacyVoiceRestrictions, DbDumpError> {
    let mut commands = BTreeMap::new();
    let mut members = std::collections::BTreeSet::new();
    for (table, is_member) in [
        ("voice_command_roles", false),
        ("voice_command_role_members", true),
    ] {
        let text_encoded = contents
            .manifest
            .tables
            .iter()
            .find(|info| info.name == table)
            .is_some_and(|info| !info.column_types.is_empty());
        for row in contents.buffers.get(table).into_iter().flatten() {
            let field = |name: &str, snowflake: bool| -> Result<String, DbDumpError> {
                let value = row.get(name).ok_or_else(|| {
                    DbDumpError::Refused(format!("{table}: missing historical voice field {name}"))
                })?;
                let value = cell_input(value, "text", text_encoded)?.ok_or_else(|| {
                    DbDumpError::Refused(format!("{table}: null historical voice field {name}"))
                })?;
                // Match the frozen 0229 CHECKs, including its 20-digit bound.
                let valid = if snowflake {
                    !value.is_empty()
                        && value.len() <= 20
                        && !value.starts_with('0')
                        && value.bytes().all(|byte| byte.is_ascii_digit())
                } else {
                    !value.trim_matches(' ').is_empty()
                };
                if !valid {
                    return Err(DbDumpError::Refused(format!(
                        "{table}: invalid historical voice field {name}"
                    )));
                }
                Ok(value)
            };
            let guild = field("guild_id", true)?;
            let command = field("command", false)?;
            let duplicate = if is_member {
                !members.insert((guild, command, field("role_id", true)?))
            } else {
                commands.insert((guild, command), Vec::new()).is_some()
            };
            if duplicate {
                return Err(DbDumpError::Refused(format!(
                    "{table}: duplicate historical voice key"
                )));
            }
        }
    }
    if contents.manifest.applied_cutover(469) {
        return Ok(LegacyVoiceRestrictions::default());
    }
    let mut result = LegacyVoiceRestrictions {
        commands: commands.len() as u64,
        members: members.len() as u64,
        ..LegacyVoiceRestrictions::default()
    };
    for (guild, command, role) in members {
        // 0229 has no FK: orphan member rows were ignored by 0469 too.
        if let Some(roles) = commands.get_mut(&(guild, command)) {
            roles.push(role);
        }
    }
    for ((guild, command), mut roles) in commands {
        if crate::voice_access::is_voice_command(&command) {
            roles.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
            result
                .by_guild
                .entry(guild)
                .or_default()
                .insert(command, roles);
        }
    }
    Ok(result)
}

/// Replace the contents of the bot-owned tables with a dump.
///
/// Requires a target without moderation history: overwriting destination
/// evidence could forget a post-backup PUT/DELETE or newer permanent ban.
/// Other bot-owned tables are replaced, not merged. One transaction protects
/// refusal/rollback, and every imported executable expiry is quarantined:
/// snapshot acceptance is historical evidence, not current Discord ownership.
pub async fn restore(pool: &PgPool, in_path: &Path) -> Result<RestoreReport, DbDumpError> {
    let contents: DumpContents = inspect(in_path)?;
    let legacy_voice = legacy_voice_restrictions(&contents)?;
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
    // Moderation history fence: overwriting destination evidence could forget
    // a post-backup PUT/DELETE or a newer permanent ban. Refuse while the
    // lock is held so no concurrent writer slips evidence in after the check.
    // Tables absent from the target (fresh migrated targets, legacy schemas)
    // are skipped via to_regclass; never CASCADE through unrelated history.
    let has_history: bool = sqlx::query_scalar(
        "SELECT (to_regclass('moderation_member_bans') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_member_bans))
             OR (to_regclass('moderation_scheduled_unbans') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_scheduled_unbans))
             OR (to_regclass('moderation_audit') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_audit))
             OR (to_regclass('moderation_idempotency') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_idempotency))
             OR (to_regclass('moderation_warnings') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_warnings))
             OR (to_regclass('moderation_channel_executions') IS NOT NULL AND EXISTS (SELECT 1 FROM moderation_channel_executions))",
    )
    .fetch_one(&mut *tx)
    .await?;
    if has_history {
        return Err(DbDumpError::Refused(
            "destination moderation history exists; restore into a fresh migrated target, preserve the destination and reconcile both histories before enabling moderation".into(),
        ));
    }
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
                let sources = [(table, column.as_str())];
                restart_sequence(
                    &mut tx,
                    &sources,
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

    // Even a matching accepted snapshot intent cannot prove current remote
    // ownership: a permanent ban may have superseded it after the backup.
    // Quarantine ALL executable imports; preserve states in the original file,
    // ownership evidence, and the independent fence of imported running DELETEs.
    sqlx::query(
        "UPDATE moderation_scheduled_unbans SET dispatch_uncertain = TRUE
         WHERE state = 'running'
            OR (state = 'quarantined' AND (claim_token IS NOT NULL OR claimed_at IS NOT NULL))",
    )
    .execute(&mut *tx)
    .await?;
    let quarantined_unbans = sqlx::query(
        "UPDATE moderation_scheduled_unbans AS job SET state = 'quarantined'
         WHERE job.state IN ('staged', 'pending', 'running')",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Pre-0142 archives carry no delivery_uncertain column: their restored
    // pending rows would land at DEFAULT FALSE and replay past the fence.
    // Re-fence exactly like the 0142 backfill when the archive never
    // recorded the column. Archives that recorded it restore faithfully
    // (the drill asserts the exact roundtrip); targets without the column
    // predate the fence and take no update.
    let archive_fenced = manifest
        .tables
        .iter()
        .find(|table| table.name == "scheduled_messages")
        .is_some_and(|table| table.columns.iter().any(|c| c == "delivery_uncertain"));
    if !archive_fenced
        && columns_of(&mut tx, "scheduled_messages")
            .await?
            .iter()
            .any(|(name, _)| name == "delivery_uncertain")
    {
        sqlx::query(
            "UPDATE scheduled_messages SET delivery_uncertain = TRUE
              WHERE occurrence_nonce IS NOT NULL OR claim_token IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;
    }

    // Translate pre-0469 archive rows directly into the current authority.
    // Existing enforced entries win, exactly as in 0469. Post-0469 archives
    // discard stale mirrors, so lifted restrictions cannot be resurrected.
    let translated_voice_command_roles = legacy_voice.commands;
    let translated_voice_command_role_members = legacy_voice.members;
    let mut voice_access_controls_added = 0;
    let has_columns = |columns: &[(String, String)], names: &[&str]| {
        names
            .iter()
            .all(|name| columns.iter().any(|(column, _)| column == name))
    };
    if !legacy_voice.by_guild.is_empty() {
        if !has_columns(
            &columns_of(&mut tx, "voice_access_controls").await?,
            &["guild_id", "command_roles"],
        ) {
            return Err(DbDumpError::Refused(
                "target lacks voice command authority; refusing to lose historical restrictions"
                    .to_owned(),
            ));
        }
        let controls_before = count_of(&mut tx, "voice_access_controls").await?;
        for (guild, restrictions) in legacy_voice.by_guild {
            let encoded = serde_json::to_string(&restrictions).map_err(|_| {
                DbDumpError::Refused("cannot encode historical voice restrictions".to_owned())
            })?;
            sqlx::query(
                "INSERT INTO voice_access_controls (guild_id, command_roles)
                 VALUES ($1, $2::jsonb)
                 ON CONFLICT (guild_id) DO UPDATE
                   SET command_roles = EXCLUDED.command_roles || voice_access_controls.command_roles",
            )
            .bind(guild)
            .bind(encoded)
            .execute(&mut *tx)
            .await?;
        }
        voice_access_controls_added =
            (count_of(&mut tx, "voice_access_controls").await? - controls_before) as u64;
    }

    // Pre-0470 archives can hold the guild creation switch and guild-wide
    // command role only in voice_guild_settings (/import, 0229). Since 0470
    // the runtime gates read only voice_access_controls.room_creation_enabled
    // and required_role_id, so restoring a disagreeing pair as archived
    // would leave the imported switch and role silently unenforced. Replay
    // the 0470 translation, in migration order after the 0469 one above:
    // same tighten-only merge (creation stays off if either store says off;
    // a role already enforced wins over the imported one) and the same
    // mirror sync back to voice_guild_settings, leading-zero guard included,
    // so the previous binary's /export after a backout shows what is
    // enforced. Two differences from the migration file, neither of which
    // changes the enforced outcome: only settings rows that can tighten
    // (creation off, or a role present) merge, because a missing
    // voice_access_controls row already means the permissive defaults; and
    // there are no per-guild advisory locks, because restore already holds
    // ACCESS EXCLUSIVE locks on the replaced tables, so no /access save from
    // a still-running previous binary can race the merge. Targets predating
    // either store fail the column guards and take no translation.
    //
    // The evidence gate is 0469's, one migration later: only a source that
    // had not applied 0470 holds authoritative switch/role values in
    // voice_guild_settings. After 0470 those columns are a mirror /access
    // does not keep current, so a post-0470 archive can carry a switch or
    // role an admin has since lifted; replaying it would resurrect that
    // gate. Archives proving 0470 was applied therefore restore both voice
    // stores verbatim. Archives without recorded evidence predate the
    // evidence field and replay fail-closed; the report names how many
    // guilds the replay tightened.
    let mut translated_voice_creation_gates = 0;
    if !manifest.applied_cutover(470)
        && has_columns(
            &columns_of(&mut tx, "voice_guild_settings").await?,
            &["guild_id", "creation_enabled", "command_role_id"],
        )
        && has_columns(
            &columns_of(&mut tx, "voice_access_controls").await?,
            &["guild_id", "room_creation_enabled", "required_role_id"],
        )
        && count_of(&mut tx, "voice_guild_settings").await? > 0
    {
        // Guilds the merge actually tightens: no enforced row yet, a switch
        // about to turn off, or a role about to land where none is enforced.
        let (tightened,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM voice_guild_settings s \
              LEFT JOIN voice_access_controls a ON a.guild_id = s.guild_id \
             WHERE (NOT s.creation_enabled OR s.command_role_id IS NOT NULL) \
               AND (a.guild_id IS NULL \
                 OR (a.room_creation_enabled AND NOT s.creation_enabled) \
                 OR (a.required_role_id IS NULL AND s.command_role_id IS NOT NULL))",
        )
        .fetch_one(&mut *tx)
        .await?;
        translated_voice_creation_gates = tightened as u64;
        let controls_before = count_of(&mut tx, "voice_access_controls").await?;
        sqlx::query(
            "INSERT INTO voice_access_controls (guild_id, room_creation_enabled, required_role_id) \
             SELECT guild_id, creation_enabled, command_role_id \
               FROM voice_guild_settings \
              WHERE NOT creation_enabled OR command_role_id IS NOT NULL \
             ON CONFLICT (guild_id) DO UPDATE \
               SET room_creation_enabled = \
                     voice_access_controls.room_creation_enabled AND EXCLUDED.room_creation_enabled, \
                   required_role_id = \
                     COALESCE(voice_access_controls.required_role_id, EXCLUDED.required_role_id)",
        )
        .execute(&mut *tx)
        .await?;
        voice_access_controls_added +=
            (count_of(&mut tx, "voice_access_controls").await? - controls_before) as u64;
        sqlx::query(
            "UPDATE voice_guild_settings s \
                SET creation_enabled = a.room_creation_enabled, \
                    command_role_id = a.required_role_id \
               FROM voice_access_controls a \
              WHERE a.guild_id = s.guild_id \
                AND (a.required_role_id IS NULL OR a.required_role_id NOT LIKE '0%')",
        )
        .execute(&mut *tx)
        .await?;
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

    // Every other allocator (owned serial/identity, including columns not
    // named `id`, and the standalone table_sequences allocators) resumes past
    // the restored rows, the archive's mark and the target's own position.
    // 0113 draws retry queue tickets from the ban-ownership generation
    // sequence without owning a column: resume past restored tickets too, or
    // the next PUT or retry reuses a restored queue position.
    let has_retry_generation = columns_of(&mut tx, "moderation_scheduled_unbans")
        .await?
        .iter()
        .any(|(name, _)| name == "retry_generation");
    for &table in &target_tables {
        for (column, sequence) in table_sequences(&mut tx, table).await? {
            if is_destination_owned(table, &column) {
                continue;
            }
            let mut sources = vec![(table, column.as_str())];
            if table == "moderation_member_bans" && column == "generation" && has_retry_generation {
                sources.push(("moderation_scheduled_unbans", "retry_generation"));
            }
            restart_sequence(
                &mut tx,
                &sources,
                &sequence,
                manifest.sequence_mark(table, &column),
            )
            .await?;
        }
    }
    tx.commit().await?;

    let missing_member_ban_ownership = !manifest
        .tables
        .iter()
        .any(|table| table.name == "moderation_member_bans");
    let mut report = RestoreReport {
        manifest,
        restored: BTreeMap::new(),
        dropped_columns,
        initialized_tables,
        missing_member_ban_ownership,
        quarantined_unbans,
        translated_voice_command_roles,
        translated_voice_command_role_members,
        translated_voice_creation_gates,
        voice_access_controls_added,
        ok: true,
    };
    for table in &target_tables {
        let row: (i64,) = sqlx::query_as(audited(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(pool)
            .await?;
        let count = row.0 as u64;
        if count != report.expected_count(table) {
            report.ok = false;
        }
        report.restored.insert((*table).to_owned(), count);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(translated_roles: u64, translated_members: u64, added: u64) -> RestoreReport {
        let table = |name: &str, count: u64| DumpTableInfo {
            name: name.to_owned(),
            columns: Vec::new(),
            column_types: Vec::new(),
            count,
        };
        RestoreReport {
            manifest: DumpManifest {
                kind: "manifest".to_owned(),
                version: DUMP_VERSION,
                created_at: String::new(),
                tables: vec![
                    table("members", 3),
                    table("voice_access_controls", 2),
                    table("voice_command_roles", 4),
                    table("voice_command_role_members", 5),
                ],
                events_sequence: 0,
                schema_migrations: Vec::new(),
                cutover_migrations: Vec::new(),
                sequence_marks: Vec::new(),
            },
            restored: BTreeMap::new(),
            dropped_columns: BTreeMap::new(),
            initialized_tables: BTreeMap::from([("guild_settings_revision".to_owned(), 1)]),
            missing_member_ban_ownership: false,
            quarantined_unbans: 0,
            translated_voice_command_roles: translated_roles,
            translated_voice_command_role_members: translated_members,
            translated_voice_creation_gates: 0,
            voice_access_controls_added: added,
            ok: true,
        }
    }

    #[test]
    fn applied_cutover_needs_recorded_evidence_at_or_past_the_version() {
        let mut report = report(0, 0, 0);
        // Archives written before the evidence field prove nothing.
        assert!(!report.manifest.applied_cutover(469));
        report.manifest.cutover_migrations = vec![467, 468];
        assert!(!report.manifest.applied_cutover(469));
        report.manifest.cutover_migrations = vec![468, 469];
        assert!(report.manifest.applied_cutover(469));
        // Proving 0469 does not prove 0470: an archive taken between the two
        // migrations replays only the 0470 translation.
        assert!(!report.manifest.applied_cutover(470));
        // A later version implies the chain ran through 0469 in order.
        report.manifest.cutover_migrations = vec![470];
        assert!(report.manifest.applied_cutover(469));
        assert!(report.manifest.applied_cutover(470));
    }

    #[test]
    fn expected_count_matches_manifest_without_translation() {
        let report = report(0, 0, 0);
        assert_eq!(report.expected_count("members"), 3);
        assert_eq!(report.expected_count("voice_access_controls"), 2);
        assert_eq!(report.expected_count("voice_command_roles"), 0);
        assert_eq!(report.expected_count("voice_command_role_members"), 0);
        // Absent from the manifest: initialized baseline, else zero.
        assert_eq!(report.expected_count("guild_settings_revision"), 1);
        assert_eq!(report.expected_count("events"), 0);
    }

    fn historical_voice_archive(commands: Value, members: Value) -> DumpContents {
        let rows = |value: Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row.as_object().unwrap().clone())
                .collect()
        };
        DumpContents {
            manifest: report(0, 0, 0).manifest,
            buffers: BTreeMap::from([
                ("voice_command_roles".to_owned(), rows(commands)),
                ("voice_command_role_members".to_owned(), rows(members)),
            ]),
            rows: 0,
        }
    }

    #[test]
    fn historical_voice_rows_translate_without_legacy_relations() {
        let archive = historical_voice_archive(
            serde_json::json!([
                {"guild_id": "42", "command": "kick"},
                {"guild_id": "42", "command": "limit"},
                {"guild_id": "42", "command": "retired"}
            ]),
            serde_json::json!([
                {"guild_id": "42", "command": "kick", "role_id": "101"},
                {"guild_id": "42", "command": "kick", "role_id": "9"},
                {"guild_id": "42", "command": "retired", "role_id": "5"},
                {"guild_id": "43", "command": "kick", "role_id": "5"}
            ]),
        );
        let decoded = legacy_voice_restrictions(&archive).unwrap();
        assert_eq!(decoded.commands, 3);
        assert_eq!(decoded.members, 4);
        assert_eq!(
            decoded.by_guild,
            BTreeMap::from([(
                "42".to_owned(),
                BTreeMap::from([
                    ("kick".to_owned(), vec!["9".to_owned(), "101".to_owned()]),
                    ("limit".to_owned(), Vec::new()),
                ])
            )])
        );
        let mut post_authority = archive;
        post_authority.manifest.cutover_migrations = vec![469];
        let ignored = legacy_voice_restrictions(&post_authority).unwrap();
        assert!(
            ignored.by_guild.is_empty(),
            "stale mirrors cannot resurrect a lifted gate"
        );
        assert_eq!((ignored.commands, ignored.members), (0, 0));
    }

    #[test]
    fn historical_voice_decoder_preserves_row_validation() {
        let valid = serde_json::json!({"guild_id": "42", "command": "kick"});
        for bad in [
            serde_json::json!({"guild_id": null, "command": "kick"}),
            serde_json::json!({"guild_id": "042", "command": "kick"}),
            serde_json::json!({"guild_id": "0", "command": "kick"}),
            serde_json::json!({"guild_id": "123456789012345678901", "command": "kick"}),
            serde_json::json!({"guild_id": "42", "command": "  "}),
            serde_json::json!({"guild_id": "42"}),
            serde_json::json!({"guild_id": {}, "command": "kick"}),
        ] {
            assert!(legacy_voice_restrictions(&historical_voice_archive(
                serde_json::json!([bad]),
                serde_json::json!([]),
            ))
            .is_err());
        }
        assert!(
            legacy_voice_restrictions(&historical_voice_archive(
                serde_json::json!([valid, valid]),
                serde_json::json!([]),
            ))
            .is_err(),
            "duplicate parent key refused"
        );
        let member = serde_json::json!({"guild_id": "42", "command": "kick", "role_id": "9"});
        assert!(
            legacy_voice_restrictions(&historical_voice_archive(
                serde_json::json!([valid]),
                serde_json::json!([member, member]),
            ))
            .is_err(),
            "duplicate member key refused"
        );
        assert!(legacy_voice_restrictions(&historical_voice_archive(
            serde_json::json!([valid]),
            serde_json::json!([{"guild_id": "42", "command": "kick", "role_id": "01"}]),
        ))
        .is_err());
        let mut native = historical_voice_archive(
            serde_json::json!([{"guild_id": 42, "command": "kick"}]),
            serde_json::json!([]),
        );
        assert!(
            legacy_voice_restrictions(&native).is_ok(),
            "v3 native numeric cells supported"
        );
        native
            .manifest
            .tables
            .iter_mut()
            .find(|table| table.name == "voice_command_roles")
            .unwrap()
            .column_types = vec!["text".to_owned(), "text".to_owned()];
        assert!(
            legacy_voice_restrictions(&native).is_err(),
            "typed cells must be text encoded"
        );
        assert!(legacy_voice_restrictions(&historical_voice_archive(
            serde_json::json!([]),
            serde_json::json!([]),
        ))
        .unwrap()
        .by_guild
        .is_empty());
    }

    #[test]
    fn expected_count_reflects_the_0469_replay() {
        // Every legacy row consumed; one guild row inserted on top of the
        // two archived voice_access_controls rows.
        let report = report(4, 5, 1);
        assert_eq!(report.expected_count("voice_command_roles"), 0);
        assert_eq!(report.expected_count("voice_command_role_members"), 0);
        assert_eq!(report.expected_count("voice_access_controls"), 3);
        assert_eq!(report.expected_count("members"), 3);
    }
}
