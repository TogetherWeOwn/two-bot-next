//! Explicit, bounded legacy Postgres copy engine. No migrations or Discord calls.
//!
//! A stopped copy resumes by replaying committed batches. Unchanged conflicts
//! perform no UPDATE, so replay is a no-op and needs no external checkpoint file.

pub mod mapping;
pub mod options;

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Column {
    pub source: &'static str,
    pub target: &'static str,
    pub pg_type: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CopyMode {
    Upsert,
    AppendOnly,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Sequence {
    pub name: &'static str,
    pub column: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Table {
    pub group: &'static str,
    pub source: &'static str,
    pub target: &'static str,
    /// Source primary key in native Postgres order, not text/JSON order.
    pub keys: &'static [&'static str],
    pub conflict: &'static [&'static str],
    pub columns: &'static [Column],
    pub mode: CopyMode,
    pub sequence: Option<Sequence>,
    /// Compiled safety predicate: refuse unresolved delivery work, never skip it.
    pub source_refusal: Option<&'static str>,
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("batch size must be between 1 and 10000")]
    BatchSize,
    #[error("{table}: divergent append-only row; refusing overwrite")]
    AppendOnlyConflict { table: &'static str },
    #[error("{table}: unresolved delivery state; quiesce/reconcile legacy delivery before copy")]
    UnresolvedDelivery { table: &'static str },
    #[error("{table}: database operation failed (details withheld)")]
    Database {
        table: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Receipt {
    pub table: &'static str,
    pub source_rows: i64,
    pub scanned: u64,
    pub changed: u64,
    pub batches: u64,
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn relation(name: &str) -> String {
    format!("public.{}", ident(name))
}

impl Table {
    /// All names/types come from the compiled mapping, never CLI input.
    fn page_sql(&self) -> String {
        let key = self
            .keys
            .iter()
            .map(|k| format!("s.{}", ident(k)))
            .collect::<Vec<_>>()
            .join(", ");
        let previous = self
            .keys
            .iter()
            .map(|k| format!("p.{}", ident(k)))
            .collect::<Vec<_>>()
            .join(", ");
        let cursor = self
            .keys
            .iter()
            .map(|k| format!("'{k}', s.{}", ident(k)))
            .collect::<Vec<_>>()
            .join(", ");
        let fields = self
            .columns
            .iter()
            .map(|c| {
                format!(
                    "s.{}::{} AS {}",
                    ident(c.source),
                    c.pg_type,
                    ident(c.target)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "SELECT jsonb_build_object({cursor}) AS cursor, \
             (SELECT to_jsonb(m) FROM (SELECT {fields}) m) AS payload \
             FROM {} s \
             LEFT JOIN jsonb_populate_record(NULL::{}, $1::jsonb) p ON true \
             WHERE $1::jsonb IS NULL OR ROW({key}) > ROW({previous}) \
             ORDER BY {key} LIMIT $2",
            relation(self.source),
            relation(self.source),
        )
    }

    fn upsert_sql(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|c| ident(c.target))
            .collect::<Vec<_>>();
        let conflict = self
            .conflict
            .iter()
            .map(|c| ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let mutable = self
            .columns
            .iter()
            .filter(|c| !self.conflict.contains(&c.target))
            .map(|c| ident(c.target))
            .collect::<Vec<_>>();
        let action = if mutable.is_empty() || self.mode == CopyMode::AppendOnly {
            "DO NOTHING".to_owned()
        } else {
            let assignments = mutable
                .iter()
                .map(|c| format!("{c} = EXCLUDED.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            let old = mutable
                .iter()
                .map(|c| format!("t.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            let new = mutable
                .iter()
                .map(|c| format!("EXCLUDED.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("DO UPDATE SET {assignments} WHERE ROW({old}) IS DISTINCT FROM ROW({new})")
        };
        format!(
            "INSERT INTO {} AS t ({}) \
             SELECT {} FROM jsonb_populate_recordset(NULL::{}, $1::jsonb) \
             ON CONFLICT ({conflict}) {action}",
            relation(self.target),
            columns.join(", "),
            columns.join(", "),
            relation(self.target),
        )
    }

    /// Compare typed target records, not timestamp strings or JSON encodings.
    /// If a batch is unchanged, issue no INSERT at all: statement-level settings
    /// revision triggers would otherwise advance even for a no-op conflict.
    fn changed_sql(&self) -> String {
        let join = self
            .conflict
            .iter()
            .map(|k| format!("t.{} = p.{}", ident(k), ident(k)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let old = self
            .columns
            .iter()
            .map(|c| format!("t.{}", ident(c.target)))
            .collect::<Vec<_>>()
            .join(", ");
        let new = self
            .columns
            .iter()
            .map(|c| format!("p.{}", ident(c.target)))
            .collect::<Vec<_>>()
            .join(", ");
        let fields = self
            .columns
            .iter()
            .map(|c| format!("p.{}", ident(c.target)))
            .collect::<Vec<_>>()
            .join(", ");
        let first_key = ident(self.conflict[0]);
        format!(
            "SELECT t.{first_key} IS NOT NULL AS existing, \
            (SELECT to_jsonb(m) FROM (SELECT {fields}) m) AS payload \
            FROM jsonb_populate_recordset(NULL::{}, $1::jsonb) p \
            LEFT JOIN {} t ON {join} \
            WHERE t.{first_key} IS NULL OR ROW({old}) IS DISTINCT FROM ROW({new})",
            relation(self.target),
            relation(self.target)
        )
    }
}

/// Count and type-check EVERY selected mapping before the first target write.
/// Counts and copy use one read-only repeatable-read source snapshot. Operators
/// must stop the legacy writer for final cutover; this tool does not lock it.
pub async fn copy(
    source: &PgPool,
    target: &PgPool,
    tables: &[Table],
    batch_size: u32,
    apply: bool,
) -> Result<Vec<Receipt>, CopyError> {
    if !(1..=10_000).contains(&batch_size) {
        return Err(CopyError::BatchSize);
    }
    let mut snapshot = source.begin().await.map_err(|source| CopyError::Database {
        table: "source snapshot",
        source,
    })?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *snapshot)
        .await
        .map_err(|source| CopyError::Database {
            table: "source snapshot",
            source,
        })?;
    sqlx::query("SET LOCAL TIME ZONE 'UTC'")
        .execute(&mut *snapshot)
        .await
        .map_err(|source| CopyError::Database {
            table: "source snapshot",
            source,
        })?;
    let mut receipts = Vec::new();
    for table in tables {
        let error = |source| CopyError::Database {
            table: table.source,
            source,
        };
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {}",
            relation(table.source)
        )))
        .fetch_one(&mut *snapshot)
        .await
        .map_err(error)?;
        if let Some(predicate) = table.source_refusal {
            let unresolved: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS(SELECT 1 FROM {} WHERE {predicate})",
                relation(table.source)
            )))
            .fetch_one(&mut *snapshot)
            .await
            .map_err(error)?;
            if unresolved {
                return Err(CopyError::UnresolvedDelivery {
                    table: table.source,
                });
            }
        }
        if let Some(sequence) = table.sequence {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT last_value, is_called FROM {}",
                relation(sequence.name)
            )))
            .fetch_one(target)
            .await
            .map_err(error)?;
        }
        // LIMIT 0 resolves all source columns and conversion types without
        // reading a batch. Target EXPLAIN resolves columns/conflict arbiter;
        // it does not execute an INSERT, even in dry-run.
        sqlx::query(sqlx::AssertSqlSafe(table.page_sql()))
            .bind(Option::<Value>::None)
            .bind(0_i64)
            .fetch_all(&mut *snapshot)
            .await
            .map_err(error)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN {}",
            table.upsert_sql()
        )))
        .bind(serde_json::json!([]))
        .fetch_all(target)
        .await
        .map_err(error)?;
        receipts.push(Receipt {
            table: table.source,
            source_rows: count,
            ..Receipt::default()
        });
    }
    if apply {
        for (table, receipt) in tables.iter().zip(&mut receipts) {
            let error = |source| CopyError::Database {
                table: table.source,
                source,
            };
            let mut cursor: Option<Value> = None;
            loop {
                let rows = sqlx::query(sqlx::AssertSqlSafe(table.page_sql()))
                    .bind(&cursor)
                    .bind(i64::from(batch_size))
                    .fetch_all(&mut *snapshot)
                    .await
                    .map_err(error)?;
                if rows.is_empty() {
                    break;
                }
                let payload: Vec<Value> = rows
                    .iter()
                    .map(|row| row.try_get("payload"))
                    .collect::<Result<_, _>>()
                    .map_err(error)?;
                let mut batch = target.begin().await.map_err(error)?;
                // Serialize diff/insert/sequence reconciliation against writers
                // of this table. Both bot writers must still be stopped for the
                // whole cutover: this is not online replication.
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
                    relation(table.target)
                )))
                .execute(&mut *batch)
                .await
                .map_err(error)?;
                let differences = sqlx::query(sqlx::AssertSqlSafe(table.changed_sql()))
                    .bind(Value::Array(payload))
                    .fetch_all(&mut *batch)
                    .await
                    .map_err(error)?;
                let mut changed_payload = Vec::new();
                for row in differences {
                    if table.mode == CopyMode::AppendOnly
                        && row.try_get::<bool, _>("existing").map_err(error)?
                    {
                        return Err(CopyError::AppendOnlyConflict {
                            table: table.target,
                        });
                    }
                    changed_payload.push(row.try_get::<Value, _>("payload").map_err(error)?);
                }
                // A lost commit acknowledgement is safe: unchanged batches
                // replay without DML, including append-only settings audit.
                let changed = if changed_payload.is_empty() {
                    0
                } else {
                    sqlx::query(sqlx::AssertSqlSafe(table.upsert_sql()))
                        .bind(Value::Array(changed_payload))
                        .execute(&mut *batch)
                        .await
                        .map_err(error)?
                        .rows_affected()
                };
                if let Some(sequence) = table.sequence {
                    // Never move an allocated sequence backwards. setval is not
                    // transactional; rollback may leave a harmless forward gap.
                    // Source: postgresql.org/docs/18/functions-sequence.html
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "SELECT setval($1::regclass, m.maximum, true) \
                         FROM (SELECT max({}) AS maximum FROM {}) m, {} s \
                         WHERE m.maximum > s.last_value \
                         OR (m.maximum = s.last_value AND NOT s.is_called)",
                        ident(sequence.column),
                        relation(table.target),
                        relation(sequence.name)
                    )))
                    .bind(relation(sequence.name))
                    .execute(&mut *batch)
                    .await
                    .map_err(error)?;
                }
                batch.commit().await.map_err(error)?;
                cursor = Some(rows.last().unwrap().try_get("cursor").map_err(error)?);
                receipt.scanned += rows.len() as u64;
                receipt.changed += changed;
                receipt.batches += 1;
            }
        }
    }
    snapshot
        .rollback()
        .await
        .map_err(|source| CopyError::Database {
            table: "source snapshot",
            source,
        })?;
    Ok(receipts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: Table = Table {
        group: "fixture",
        source: "legacy",
        target: "next",
        keys: &["guild_id", "id"],
        conflict: &["guild_id", "id"],
        mode: CopyMode::Upsert,
        sequence: None,
        source_refusal: None,
        columns: &[
            Column {
                source: "guild_id",
                target: "guild_id",
                pg_type: "text",
            },
            Column {
                source: "id",
                target: "id",
                pg_type: "bigint",
            },
            Column {
                source: "occurred_at",
                target: "occurred_at",
                pg_type: "timestamptz",
            },
        ],
    };

    #[test]
    fn keyset_uses_native_source_record_and_no_offset() {
        let sql = TABLE.page_sql();
        assert!(sql.contains("ROW(s.\"guild_id\", s.\"id\") > ROW(p.\"guild_id\", p.\"id\")"));
        assert!(sql.contains("NULL::public.\"legacy\""));
        assert!(sql.contains("s.\"occurred_at\"::timestamptz"));
        assert!(!sql.contains("OFFSET"));
    }

    #[test]
    fn upsert_does_not_touch_unchanged_rows_or_keys() {
        let sql = TABLE.upsert_sql();
        assert!(sql.contains("ON CONFLICT (\"guild_id\", \"id\")"));
        assert!(sql.contains(
            "WHERE ROW(t.\"occurred_at\") IS DISTINCT FROM ROW(EXCLUDED.\"occurred_at\")"
        ));
        assert!(!sql.contains("SET \"id\""));
    }
}
