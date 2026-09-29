//! The v3 backup envelope: manifest / rows / end marker, gzipped NDJSON.
//!
//! Port of the file half of legacy `src/store/dump.ts`. The writer side lives
//! in [`super::dump`]; everything here touches no database, so every refusal
//! below is pinned by a test that runs without Postgres.
//!
//! ## The format
//!
//! Gzipped NDJSON, one JSON object per line, in three kinds:
//!
//! ```text
//! {"kind":"manifest", version, createdAt, tables:[{name,columns,columnTypes,count}], ...}
//! {"kind":"row", table, data:{...}}
//! {"kind":"end", rows}
//! ```
//!
//! Values are stored in Postgres text-output form (`SELECT col::text`, one
//! `Option<String>` per cell) with the column type recorded per table in the
//! manifest. Restore re-applies the value with a `$n::type` cast, so the
//! round trip is faithful for every type the bot owns (text, integers,
//! timestamptz, booleans, json/jsonb, bytea hex, arrays, enums, intervals)
//! without per-type Rust decoding. `columnTypes` is additive to the legacy v3
//! envelope; a legacy-written dump restores with its types inferred as `text`.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Everything the bot owns. The website's own tables are not ours to back up.
///
/// The moderation tables are here because losing them is not cosmetic: a lost
/// scheduled unban is a tempban that became permanent, and a lost warn ledger
/// is a moderation history the staff cannot see (TOG-1659 High 5).
pub const DUMP_TABLES: &[&str] = &[
    "events",
    "members",
    "invite_snapshots",
    "operational_audit_log",
    "moderation_warnings",
    "moderation_scheduled_unbans",
    "moderation_audit",
    "moderation_lockdowns",
    "moderation_idempotency",
    "containment_events",
    "containment_incidents",
    "join_risk_flags",
    "automation_commands",
    "scheduled_messages",
    "sticky_messages",
    "automation_audit_log",
    "tickets",
    "ticket_transcripts",
    "automod_violations",
    "automod_processed_messages",
    "self_role_audit",
    "self_role_panel_claims",
];

/// The backup format version. Must stay 3: the envelope is frozen.
pub const DUMP_VERSION: u32 = 3;

/// A table name in the dump, validated against [`DUMP_TABLES`].
pub type DumpTable = String;

/// Per-table manifest entry: live column order, Postgres type per column
/// (`format_type()` spelling, used as the restore cast), and the row count
/// taken inside the same snapshot as the rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpTableInfo {
    pub name: String,
    pub columns: Vec<String>,
    /// Postgres type per column, parallel to `columns`. Absent on dumps
    /// written by legacy `two-bot`; restore then casts as `text`.
    #[serde(default)]
    pub column_types: Vec<String>,
    pub count: u64,
}

/// The leading manifest line of a dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpManifest {
    pub kind: String,
    pub version: u32,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub tables: Vec<DumpTableInfo>,
    /// So a restore can put the id sequence back where it belongs.
    #[serde(rename = "eventsSequence")]
    pub events_sequence: i64,
    /// Which migrations the source had applied, for diagnosing an old backup.
    #[serde(rename = "schemaMigrations")]
    pub schema_migrations: Vec<String>,
}

/// A file-level refusal: the dump is not ours, truncated, or internally
/// inconsistent. Never a database error; see [`super::dump`] for those.
#[derive(Debug, Error)]
pub enum DumpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gzip: {0}")]
    Gzip(String),
    #[error("json on line {line}: {message}")]
    Json { line: u64, message: String },
    #[error("{0}")]
    Refused(String),
}

/// True when `name` is a table this backup format owns.
///
/// The restore interpolates table names into SQL, and a backup file is not a
/// trusted input — bytes off a disk someone else may have written. Without
/// this gate a crafted dump naming `website_users` would be
/// truncated-and-inserted like one of ours.
#[must_use]
pub fn is_dump_table(name: &str) -> bool {
    DUMP_TABLES.contains(&name)
}

fn refuse(message: impl Into<String>) -> DumpError {
    DumpError::Refused(message.into())
}

/// Write one dump line (already-serialised JSON + newline) into the encoder.
pub fn write_line(enc: &mut GzEncoder<Vec<u8>>, value: &Value) -> Result<(), DumpError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|e| refuse(format!("cannot serialise dump line: {e}")))?;
    line.push(b'\n');
    enc.write_all(&line)?;
    Ok(())
}

/// Finish a gzip stream and return the compressed bytes.
pub fn finish_gzip(enc: GzEncoder<Vec<u8>>) -> Result<Vec<u8>, DumpError> {
    enc.finish().map_err(|e| DumpError::Gzip(e.to_string()))
}

/// A row line, with the table name already gated.
#[derive(Debug, Clone)]
pub struct DumpRow {
    pub table: String,
    pub data: Map<String, Value>,
}

/// A fully validated dump: manifest plus rows keyed by table, with the total
/// already checked against the file's own `end` marker.
#[derive(Debug)]
pub struct DumpContents {
    pub manifest: DumpManifest,
    /// Rows read off the file, keyed by table, in file order.
    pub buffers: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Total rows read, already checked against the file's own `end` marker.
    pub rows: u64,
}

/// Read a dump and check that it is internally consistent, touching no database.
///
/// Everything knowable from the file alone is decided here: the format
/// version, that the table names are ours, that the end marker is present,
/// and that the row counts match what the writer declared. `restore()` calls
/// this first so a bad file is refused before a transaction opens — and
/// `restore --dry-run` calls it *instead*, which is what makes the dry run a
/// real check of the backup rather than a check that a URL parses.
pub fn inspect_bytes(bytes: &[u8]) -> Result<DumpContents, DumpError> {
    let decoder = GzDecoder::new(bytes);
    let reader = std::io::BufReader::new(decoder);
    let mut manifest: Option<DumpManifest> = None;
    let mut saw_end = false;
    let mut declared_rows: u64 = 0;
    let mut buffers: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();

    for (index, line) in reader.lines().enumerate() {
        let line_no = (index + 1) as u64;
        let line = line.map_err(|e| DumpError::Gzip(format!("line {line_no}: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let obj: Value = serde_json::from_str(&line).map_err(|e| DumpError::Json {
            line: line_no,
            message: e.to_string(),
        })?;
        if saw_end {
            return Err(refuse("dump contains data after its end marker"));
        }
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "manifest" => {
                if manifest.is_some() {
                    return Err(refuse("dump contains more than one manifest"));
                }
                let version = obj.get("version").and_then(Value::as_u64).unwrap_or(0);
                if version != u64::from(DUMP_VERSION) {
                    return Err(refuse(format!(
                        "dump version {version}, this build reads {DUMP_VERSION}"
                    )));
                }
                manifest = Some(validate_manifest(&obj)?);
            }
            "row" => {
                let Some(m) = manifest.as_ref() else {
                    return Err(refuse("dump row appears before the manifest"));
                };
                let table = obj.get("table").and_then(Value::as_str).unwrap_or("");
                if !is_dump_table(table) {
                    return Err(refuse(format!(
                        "row: {table:?} is not a table this backup format owns \
                         (expected one of {})",
                        DUMP_TABLES.join(", ")
                    )));
                }
                if !m.tables.iter().any(|t| t.name == table) {
                    return Err(refuse(format!(
                        "row table {table} is not declared in the manifest"
                    )));
                }
                let data = obj
                    .get("data")
                    .and_then(Value::as_object)
                    .ok_or_else(|| refuse("dump row has no data object"))?
                    .clone();
                buffers.entry(table.to_owned()).or_default().push(data);
            }
            "end" => {
                saw_end = true;
                declared_rows = obj.get("rows").and_then(Value::as_u64).unwrap_or(0);
            }
            _ => {
                return Err(refuse(format!(
                    "dump line {line_no} has unknown kind {kind:?}"
                )));
            }
        }
    }

    let Some(m) = manifest else {
        return Err(refuse(
            "no manifest: not a two-bot dump, or the file is truncated",
        ));
    };
    // A dump that stops mid-file is the disk-full case. Refuse it rather than
    // restoring a prefix of the data and calling it a success.
    if !saw_end {
        return Err(refuse(
            "dump has no end marker - it is truncated, treat it as lost",
        ));
    }

    let mut read_rows: u64 = 0;
    for table in &m.tables {
        let actual = buffers.get(&table.name).map(Vec::len).unwrap_or(0) as u64;
        if actual != table.count {
            return Err(refuse(format!(
                "{}: manifest declares {} rows, file contains {actual}",
                table.name, table.count
            )));
        }
        read_rows += actual;
    }
    if read_rows != declared_rows {
        return Err(refuse(format!(
            "dump declares {declared_rows} rows, file contains {read_rows}"
        )));
    }

    Ok(DumpContents {
        manifest: m,
        buffers,
        rows: read_rows,
    })
}

/// Read and validate a dump file from disk.
pub fn inspect(path: &Path) -> Result<DumpContents, DumpError> {
    let bytes = std::fs::read(path)?;
    inspect_bytes(&bytes)
}

fn validate_manifest(obj: &Value) -> Result<DumpManifest, DumpError> {
    let tables = obj
        .get("tables")
        .and_then(Value::as_array)
        .ok_or_else(|| refuse("manifest has no table list"))?;
    let mut names = std::collections::BTreeSet::new();
    for table in tables {
        let row = table
            .as_object()
            .ok_or_else(|| refuse("manifest table is not an object"))?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| refuse("manifest table has no name"))?;
        if !is_dump_table(name) {
            return Err(refuse(format!(
                "manifest table: {name:?} is not a table this backup format owns \
                 (expected one of {})",
                DUMP_TABLES.join(", ")
            )));
        }
        if !names.insert(name.to_owned()) {
            return Err(refuse(format!("manifest table {name} is duplicated")));
        }
        let columns = row
            .get("columns")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid column list")))?;
        if columns.iter().any(|c| !c.is_string()) {
            return Err(refuse(format!(
                "manifest table {name} has an invalid column list"
            )));
        }
        let count = row
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid row count")))?;
        let _ = count;
    }
    let missing: Vec<&str> = DUMP_TABLES
        .iter()
        .filter(|name| !names.contains(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(refuse(format!(
            "manifest is missing tables: {}",
            missing.join(", ")
        )));
    }
    serde_json::from_value(obj.clone()).map_err(|e| refuse(format!("manifest is malformed: {e}")))
}

/// Fresh gzip encoder at the legacy compression level (9).
#[must_use]
pub fn new_encoder() -> GzEncoder<Vec<u8>> {
    GzEncoder::new(Vec::new(), Compression::new(9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder as WEnc;

    fn complete_tables(overrides: &BTreeMap<&str, (Vec<&str>, u64)>) -> Vec<Value> {
        DUMP_TABLES
            .iter()
            .map(|name| {
                let (cols, count) = overrides.get(name).cloned().unwrap_or_default();
                serde_json::json!({
                    "name": name,
                    "columns": cols,
                    "column_types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
                    "count": count,
                })
            })
            .collect()
    }

    fn manifest(tables: Vec<Value>) -> Value {
        serde_json::json!({
            "kind": "manifest",
            "version": DUMP_VERSION,
            "createdAt": "2026-08-25T00:00:00.000Z",
            "tables": tables,
            "eventsSequence": 1,
            "schemaMigrations": ["001"],
        })
    }

    fn gzip_lines(objs: &[Value]) -> Vec<u8> {
        let mut enc = WEnc::new(Vec::new(), Compression::new(9));
        for obj in objs {
            let mut line = serde_json::to_vec(obj).unwrap();
            line.push(b'\n');
            enc.write_all(&line).unwrap();
        }
        enc.finish().unwrap()
    }

    fn good_dump() -> Vec<Value> {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id", "guild_id"], 1));
        vec![
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1","guild_id":"g"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]
    }

    #[test]
    fn accepts_a_well_formed_dump() {
        let contents = inspect_bytes(&gzip_lines(&good_dump())).expect("good dump");
        assert_eq!(contents.rows, 1);
        assert_eq!(contents.buffers["events"].len(), 1);
        assert_eq!(contents.manifest.version, DUMP_VERSION);
    }

    #[test]
    fn refuses_a_foreign_table_on_the_read_path() {
        // A backup file is not a trusted input; restore() interpolates table
        // names into SQL, so the gate belongs on the read path.
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 0));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"website_users","data":{}}),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("foreign table must be refused");
        assert!(err.to_string().contains("website_users"), "{err}");
    }

    #[test]
    fn refuses_a_truncated_dump_without_end_marker() {
        let objs = good_dump();
        let err = inspect_bytes(&gzip_lines(&objs[..2])).expect_err("truncated");
        assert!(err.to_string().contains("no end marker"), "{err}");
    }

    #[test]
    fn refuses_a_version_mismatch() {
        let mut m = manifest(complete_tables(&BTreeMap::new()));
        m["version"] = serde_json::json!(DUMP_VERSION + 1);
        let bytes = gzip_lines(&[m, serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("version must be refused");
        assert!(err.to_string().contains("dump version"), "{err}");
    }

    #[test]
    fn refuses_data_after_the_end_marker() {
        let mut objs = good_dump();
        objs.push(serde_json::json!({"kind":"row","table":"events","data":{}}));
        let err = inspect_bytes(&gzip_lines(&objs)).expect_err("trailing data");
        assert!(err.to_string().contains("after its end marker"), "{err}");
    }

    #[test]
    fn refuses_a_row_count_mismatch() {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 2));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("count mismatch");
        assert!(
            err.to_string().contains("manifest declares 2 rows"),
            "{err}"
        );
    }

    #[test]
    fn refuses_a_manifest_missing_tables() {
        let tables =
            vec![serde_json::json!({"name":"events","columns":[],"column_types":[],"count":0})];
        let bytes = gzip_lines(&[manifest(tables), serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("missing tables");
        assert!(err.to_string().contains("missing tables"), "{err}");
    }

    #[test]
    fn refuses_a_row_before_the_manifest() {
        let bytes = gzip_lines(&[
            serde_json::json!({"kind":"row","table":"events","data":{}}),
            manifest(complete_tables(&BTreeMap::new())),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("row before manifest");
        assert!(err.to_string().contains("before the manifest"), "{err}");
    }
}
