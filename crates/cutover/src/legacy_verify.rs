//! Independent, read-only database parity checks. No migration or copy path.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Postgres, Row, Transaction};
use thiserror::Error;

use crate::legacy_mapping::{
    quote_identifier, quote_table, MappingError, MappingSpec, TableMapping,
};

pub const DEFAULT_SAMPLE_LIMIT: usize = 10;
pub const MAX_SAMPLE_LIMIT: usize = 1000;

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error(transparent)]
    Mapping(#[from] MappingError),
    // Cast errors may quote private source values. Never render SQLx's message
    // or either connection URL in an operator report.
    #[error("database verification failed (database details omitted)")]
    Database(#[from] sqlx::Error),
    #[error("invalid JSON value during normalization")]
    Json(#[from] serde_json::Error),
    #[error("null or duplicate normalized key in table {0}")]
    InvalidKey(String),
    #[error("sample limit must not exceed 1000")]
    SampleLimit,
    #[error("JSON number exponent is outside the supported range")]
    Number,
}

#[derive(Debug, Serialize)]
pub struct VerificationReport {
    pub version: u32,
    pub matches: bool,
    pub tables: Vec<TableReport>,
}

#[derive(Debug, Serialize)]
pub struct TableReport {
    pub group: String,
    pub source: String,
    pub target: String,
    pub source_rows: u64,
    pub target_rows: u64,
    pub missing_in_target: KeyDifference,
    pub extra_in_target: KeyDifference,
    pub columns: Vec<ColumnReport>,
    pub matches: bool,
}

#[derive(Debug, Serialize)]
pub struct KeyDifference {
    pub count: usize,
    /// Tuples of normalized key components, not ambiguous joined strings.
    pub sample: Vec<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct ColumnReport {
    pub source: String,
    pub target: String,
    pub source_sha256: String,
    pub target_sha256: String,
    pub matches: bool,
}

/// Establish the read-only snapshot BEFORE any table read, on both endpoints.
/// Exposed so the integration test can attempt a forbidden write in this exact
/// transaction setup rather than in an unrelated read-only test transaction.
pub async fn read_only_transaction(
    connection: &mut PgConnection,
) -> Result<Transaction<'_, Postgres>, VerifyError> {
    let mut tx = connection.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL TIME ZONE 'UTC'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL DateStyle TO 'ISO, YMD'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL bytea_output TO 'hex'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout TO '30s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

pub async fn verify(
    source: &mut PgConnection,
    target: &mut PgConnection,
    spec: &MappingSpec,
    groups: &[String],
    sample_limit: usize,
) -> Result<VerificationReport, VerifyError> {
    let tables = spec.select(groups)?;
    if sample_limit > MAX_SAMPLE_LIMIT {
        return Err(VerifyError::SampleLimit);
    }
    let mut source_tx = read_only_transaction(source).await?;
    let mut target_tx = read_only_transaction(target).await?;
    let mut reports = Vec::with_capacity(tables.len());
    for table in tables {
        let left = inventory(&mut source_tx, table, true).await?;
        let right = inventory(&mut target_tx, table, false).await?;
        reports.push(compare(table, left, right, sample_limit));
    }
    // Explicit rollback even on success. An error drops/rolls back both txs.
    source_tx.rollback().await?;
    target_tx.rollback().await?;
    Ok(VerificationReport {
        version: 1,
        matches: reports.iter().all(|t| t.matches),
        tables: reports,
    })
}

struct Inventory {
    rows: u64,
    keys: BTreeSet<Vec<String>>,
    checksums: Vec<String>,
}

fn select_sql(table: &TableMapping, source: bool) -> Result<String, MappingError> {
    let projections: Result<Vec<_>, _> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = quote_identifier(if source { &c.source } else { &c.target })?;
            let value = if c.pg_type == "numeric" {
                format!("trim_scale({name}::numeric)::text")
            } else {
                format!("({name}::{})::text", c.pg_type)
            };
            Ok(format!("{value} AS c{i}"))
        })
        .collect();
    let ordering = table
        .keys
        .iter()
        .map(|key| {
            let index = table
                .columns
                .iter()
                .position(|c| &c.source == key)
                .expect("validated key");
            format!("c{index} COLLATE \"C\"")
        })
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "SELECT * FROM (SELECT {} FROM {}) AS normalized ORDER BY {ordering}",
        projections?.join(", "),
        quote_table(if source { &table.source } else { &table.target })?
    ))
}

async fn inventory(
    tx: &mut Transaction<'_, Postgres>,
    table: &TableMapping,
    source: bool,
) -> Result<Inventory, VerifyError> {
    let sql = select_sql(table, source)?;
    // Server cursor bounds decoded row storage. Only key tuples, not all values,
    // are retained for exact set differences; checksums are accumulated online.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DECLARE legacy_verify_rows NO SCROLL CURSOR FOR {sql}"
    )))
    .execute(&mut **tx)
    .await?;
    let key_indexes = table
        .keys
        .iter()
        .map(|key| {
            table
                .columns
                .iter()
                .position(|c| &c.source == key)
                .expect("validated key")
        })
        .collect::<Vec<_>>();
    let mut keys = BTreeSet::new();
    let mut rows = 0;
    let mut hashes: Vec<Sha256> = table.columns.iter().map(|_| Sha256::new()).collect();
    loop {
        let page = sqlx::query("FETCH FORWARD 1024 FROM legacy_verify_rows")
            .fetch_all(&mut **tx)
            .await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let mut cells = Vec::with_capacity(table.columns.len());
            for (i, column) in table.columns.iter().enumerate() {
                let value: Option<String> = row.try_get(i)?;
                cells.push(match value {
                    Some(text) if matches!(column.pg_type.as_str(), "json" | "jsonb") => {
                        Some(canonical_json(&serde_json::from_str(&text)?)?)
                    }
                    other => other,
                });
            }
            let key: Vec<String> = key_indexes
                .iter()
                .map(|i| {
                    cells[*i].clone().ok_or_else(|| {
                        VerifyError::InvalidKey(if source {
                            table.source.clone()
                        } else {
                            table.target.clone()
                        })
                    })
                })
                .collect::<Result<_, _>>()?;
            if !keys.insert(key.clone()) {
                return Err(VerifyError::InvalidKey(if source {
                    table.source.clone()
                } else {
                    table.target.clone()
                }));
            }
            let key_bytes = serde_json::to_vec(&key)?;
            for (hash, cell) in hashes.iter_mut().zip(cells) {
                frame(hash, &key_bytes);
                // JSON framing distinguishes SQL NULL, empty string and "null".
                frame(hash, &serde_json::to_vec(&cell)?);
            }
            rows += 1;
        }
    }
    sqlx::query("CLOSE legacy_verify_rows")
        .execute(&mut **tx)
        .await?;
    Ok(Inventory {
        rows,
        keys,
        checksums: hashes
            .into_iter()
            .map(|h| hex::encode(h.finalize()))
            .collect(),
    })
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn compare(
    table: &TableMapping,
    source: Inventory,
    target: Inventory,
    limit: usize,
) -> TableReport {
    let difference = |left: &BTreeSet<Vec<String>>, right: &BTreeSet<Vec<String>>| KeyDifference {
        count: left.difference(right).count(),
        sample: left.difference(right).take(limit).cloned().collect(),
    };
    let missing = difference(&source.keys, &target.keys);
    let extra = difference(&target.keys, &source.keys);
    let columns: Vec<_> = table
        .columns
        .iter()
        .zip(source.checksums)
        .zip(target.checksums)
        .map(|((column, left), right)| ColumnReport {
            source: column.source.clone(),
            target: column.target.clone(),
            matches: left == right,
            source_sha256: left,
            target_sha256: right,
        })
        .collect();
    TableReport {
        group: table.group.clone(),
        source: table.source.clone(),
        target: table.target.clone(),
        source_rows: source.rows,
        target_rows: target.rows,
        matches: source.rows == target.rows
            && missing.count == 0
            && extra.count == 0
            && columns.iter().all(|c| c.matches),
        missing_in_target: missing,
        extra_in_target: extra,
        columns,
    }
}

/// Canonical object keys and exact decimal numbers, without f64 rounding.
/// Array order and string contents are significant. JSON null != SQL NULL.
fn canonical_json(value: &Value) -> Result<String, VerifyError> {
    Ok(match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value)?,
        Value::Number(value) => canonical_number(&value.to_string())?,
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        ),
        Value::Object(values) => {
            let mut pairs = values.iter().collect::<Vec<_>>();
            pairs.sort_by(|(left, _), (right, _)| left.cmp(right));
            let fields = pairs
                .into_iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key)?,
                        canonical_json(value)?
                    ))
                })
                .collect::<Result<Vec<_>, VerifyError>>()?;
            format!("{{{}}}", fields.join(","))
        }
    })
}

fn canonical_number(text: &str) -> Result<String, VerifyError> {
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().map_err(|_| VerifyError::Number)?),
        None => (text, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let decimals = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits = mantissa.replace('.', "");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok("0".into());
    }
    let significant = digits.trim_end_matches('0');
    let shift = i64::try_from(digits.len() - significant.len()).map_err(|_| VerifyError::Number)?;
    let exponent = exponent
        .checked_sub(i64::try_from(decimals).map_err(|_| VerifyError::Number)?)
        .and_then(|e| e.checked_add(shift))
        .ok_or(VerifyError::Number)?;
    Ok(format!(
        "{}{significant}e{exponent}",
        if negative { "-" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_canonical_without_losing_large_numbers() {
        let normalize = |s| canonical_json(&serde_json::from_str(s).unwrap()).unwrap();
        assert_eq!(
            normalize(r#"{"z":[1.00,{"b":true,"a":null}],"a":-0.0}"#),
            normalize(r#"{"a":0,"z":[1,{"a":null,"b":true}]}"#)
        );
        assert_ne!(
            normalize("184467440737095516160"),
            normalize("184467440737095516161")
        );
        assert_ne!(normalize("[1,2]"), normalize("[2,1]"));
        assert_ne!(normalize(r#""null""#), normalize("null"));
    }

    #[test]
    fn framing_cannot_join_ambiguous_values() {
        let checksum = |values: &[&[u8]]| {
            let mut hash = Sha256::new();
            for value in values {
                frame(&mut hash, value);
            }
            hex::encode(hash.finalize())
        };
        assert_ne!(checksum(&[b"ab", b"c"]), checksum(&[b"a", b"bc"]));
    }

    #[test]
    fn sql_orders_both_sides_by_the_same_normalized_key() {
        let spec = MappingSpec::parse(include_str!("../mappings/example.json")).unwrap();
        let source = select_sql(&spec.tables[0], true).unwrap();
        let target = select_sql(&spec.tables[0], false).unwrap();
        assert!(source.contains("(\"id\"::bigint)::text AS c0"));
        assert!(target.contains("(\"member_id\"::bigint)::text AS c0"));
        assert!(source.ends_with("ORDER BY c0 COLLATE \"C\""));
        assert!(target.ends_with("ORDER BY c0 COLLATE \"C\""));
    }
}
