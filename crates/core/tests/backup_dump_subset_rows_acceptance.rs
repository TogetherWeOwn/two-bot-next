//! Acceptance: dump inspect refuses a row that carries only a strict
//! subset of its table's manifest columns.
//!
//! A subset row can never restore: restore builds one parameter per kept
//! manifest column and refuses a row with a missing cell. Inspect (and
//! therefore a restore `--dry-run`) must refuse the same archive up front,
//! with fixed vocabulary that never echoes row content, instead of letting
//! an unrestorable file report as verified.
//!
//! Frozen v3 decision, from real v3 evidence: the checked-in archive
//! emitted by the actual frozen v3 writer (`fixtures/legacy-v3-native.ndjson`)
//! carries exactly its manifest columns on every row, and restore's
//! missing-cell refusal has no v3 exception, so a v3 subset row refuses
//! with the same fixed message. The unmodified fixture must keep passing.

use std::collections::BTreeMap;
use std::io::Write as _;

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;
use two_bot_core::backup::dump_file::{inspect_bytes, DUMP_TABLES, DUMP_VERSION};

/// A cell value that only exists inside the test archive. A refusal that
/// contains it echoed row content into operator output.
const ECHO_SENTINEL: &str = "sentinel_subset_row_echo_4e2a";

/// The fixed refusal vocabulary this contract pins.
const MISSING_COLUMN_REFUSAL: &str = "is missing a manifest column";

fn gzip_lines(objs: &[Value]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(9));
    for obj in objs {
        let mut line = serde_json::to_vec(obj).unwrap();
        line.push(b'\n');
        enc.write_all(&line).unwrap();
    }
    enc.finish().unwrap()
}

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

/// A current-version archive whose one `events` row carries exactly the
/// manifest columns `id` and `guild_id`.
fn complete_dump() -> Vec<Value> {
    let mut over = BTreeMap::new();
    over.insert("events", (vec!["id", "guild_id"], 1));
    vec![
        manifest(complete_tables(&over)),
        serde_json::json!({"kind":"row","table":"events","data":{"id":"1","guild_id":"g"}}),
        serde_json::json!({"kind":"end","rows":1}),
    ]
}

fn subset_refusal(objs: &[Value]) -> String {
    inspect_bytes(&gzip_lines(objs))
        .expect_err("a row missing a manifest column must refuse at inspect")
        .to_string()
}

#[test]
fn acceptance_inspect_refuses_row_missing_one_manifest_column() {
    // Strict subset: `guild_id` is declared but absent from the row. The
    // surviving cell carries the sentinel so any echo is visible.
    let mut objs = complete_dump();
    objs[1]["data"] = serde_json::json!({"id": ECHO_SENTINEL});
    let err = subset_refusal(&objs);
    assert!(err.contains(MISSING_COLUMN_REFUSAL), "{err}");
    assert!(!err.contains(ECHO_SENTINEL), "{err}");
}

#[test]
fn acceptance_inspect_refuses_row_with_empty_data_object() {
    // The extreme subset: a row carrying none of its declared columns is
    // the same refusal, not a vacuously valid row.
    let mut objs = complete_dump();
    objs[1]["data"] = serde_json::json!({});
    let err = subset_refusal(&objs);
    assert!(err.contains(MISSING_COLUMN_REFUSAL), "{err}");
    assert!(!err.contains(ECHO_SENTINEL), "{err}");
}

#[test]
fn acceptance_inspect_refuses_when_only_a_later_row_is_a_subset() {
    // Completeness is per row: a complete first row does not excuse a
    // subset second row for the same table.
    let mut over = BTreeMap::new();
    over.insert("events", (vec!["id", "guild_id"], 2));
    let objs = vec![
        manifest(complete_tables(&over)),
        serde_json::json!({"kind":"row","table":"events","data":{"id":"1","guild_id":"g"}}),
        serde_json::json!({"kind":"row","table":"events","data":{"id": ECHO_SENTINEL}}),
        serde_json::json!({"kind":"end","rows":2}),
    ];
    let err = subset_refusal(&objs);
    assert!(err.contains(MISSING_COLUMN_REFUSAL), "{err}");
    assert!(!err.contains(ECHO_SENTINEL), "{err}");
}

#[test]
fn acceptance_inspect_accepts_row_carrying_every_manifest_column() {
    // Regression guard: the completeness check refuses subsets only. A
    // complete row, and declared-but-empty tables, stay valid.
    let contents = inspect_bytes(&gzip_lines(&complete_dump())).unwrap();
    assert_eq!(contents.rows, 1);
    assert_eq!(contents.buffers["events"][0].len(), 2);
}

#[test]
fn acceptance_inspect_refuses_subset_row_in_real_frozen_v3_archive() {
    // Real v3 evidence: this archive came from the actual frozen writer
    // and every row carries exactly its manifest columns, so it passes
    // unchanged...
    let mut objs: Vec<Value> = include_str!("fixtures/legacy-v3-native.ndjson")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let contents = inspect_bytes(&gzip_lines(&objs)).unwrap();
    assert_eq!(contents.manifest.version, 3);
    assert_eq!(contents.rows, 2);

    // ...and removing one declared column from one of its rows refuses
    // with the same fixed message as a current-version subset row,
    // because restore would refuse that row's missing cell too.
    let row = objs
        .iter_mut()
        .find(|obj| obj.get("kind").and_then(Value::as_str) == Some("row"))
        .unwrap();
    let data = row["data"].as_object_mut().unwrap();
    let dropped = data.shift_remove("guild_id");
    assert!(dropped.is_some(), "fixture row must declare guild_id");
    data.insert("id".to_owned(), serde_json::json!(ECHO_SENTINEL));
    let err = subset_refusal(&objs);
    assert!(err.contains(MISSING_COLUMN_REFUSAL), "{err}");
    assert!(!err.contains(ECHO_SENTINEL), "{err}");
}
