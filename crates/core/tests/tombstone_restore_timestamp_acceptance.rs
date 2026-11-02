//! Restore-plan contract: tombstones must carry real, fixed-width millisecond
//! UTC timestamps. These tests deliberately use the public reader, not a new
//! validator seam; accepting and normalizing an unsafe timestamp is not enough.

use serde_json::{json, Value};
use two_bot_core::backup::dump_file::{
    finish_gzip, inspect_bytes, new_encoder, write_line, DUMP_TABLES, DUMP_VERSION,
};

const TABLE: &str = "scheduled_events_tombstone";
const CANONICAL: &str = "2026-08-25T20:00:00.000Z";
const ECHO_SENTINEL: &str = "tombstone_cell_do_not_echo_81dc";

fn tombstone(deleted_at: Option<Value>) -> Value {
    let mut row = json!({"guild_id": ECHO_SENTINEL, "event_id": ECHO_SENTINEL});
    if let Some(value) = deleted_at {
        row["deleted_at"] = value;
    }
    row
}

fn archive(table: &str, rows: &[Value], columns: &[&str]) -> Vec<u8> {
    let tables: Vec<_> = DUMP_TABLES
        .iter()
        .map(|name| {
            let cols = if *name == table { columns } else { &[] };
            json!({
                "name": name,
                "columns": cols,
                "column_types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
                "count": if *name == table { rows.len() } else { 0 },
            })
        })
        .collect();
    let mut encoder = new_encoder();
    write_line(
        &mut encoder,
        &json!({
            "kind": "manifest", "version": DUMP_VERSION,
            "createdAt": CANONICAL, "tables": tables,
            "eventsSequence": 1, "schemaMigrations": [],
        }),
    )
    .unwrap();
    for row in rows {
        write_line(
            &mut encoder,
            &json!({"kind":"row", "table":table, "data":row}),
        )
        .unwrap();
    }
    write_line(&mut encoder, &json!({"kind":"end", "rows":rows.len()})).unwrap();
    finish_gzip(encoder).unwrap()
}

fn tombstone_archive(deleted_at: Option<Value>) -> Vec<u8> {
    archive(
        TABLE,
        &[tombstone(deleted_at)],
        &["guild_id", "event_id", "deleted_at"],
    )
}

fn assert_refusal(bytes: &[u8], label: &str, cell: Option<&Value>) {
    let result = inspect_bytes(bytes);
    assert!(
        result.is_err(),
        "inspect accepted {label} tombstone deleted_at; restore must refuse, not normalize"
    );
    let message = result.unwrap_err().to_string();
    assert!(message.len() <= 512, "refusal must be bounded");
    assert!(
        !message.contains(ECHO_SENTINEL),
        "refusal echoed a row cell"
    );
    if let Some(raw) = cell.and_then(Value::as_str).filter(|raw| raw.len() >= 8) {
        assert!(!message.contains(raw), "refusal echoed deleted_at");
    }
}

macro_rules! rejects {
    ($name:ident, $value:expr) => {
        #[test]
        fn $name() {
            // Prove the same archive shape is valid before changing one cell.
            assert!(inspect_bytes(&tombstone_archive(Some(json!(CANONICAL)))).is_ok());
            let value = $value;
            assert_refusal(
                &tombstone_archive(Some(value.clone())),
                stringify!($name),
                Some(&value),
            );
        }
    };
}

rejects!(rejects_seconds_only, json!("2026-08-25T20:00:00Z"));
rejects!(rejects_zero_offset, json!("2026-08-25T20:00:00.000+00:00"));
rejects!(
    rejects_nonzero_offset,
    json!("2026-08-25T21:00:00.000+01:00")
);
rejects!(rejects_overprecision, json!("2026-08-25T20:00:00.0001Z"));
rejects!(rejects_short_fraction, json!("2026-08-25T20:00:00.00Z"));
rejects!(rejects_non_leap_day, json!("2026-02-29T20:00:00.000Z"));
rejects!(rejects_non_leap_century, json!("1900-02-29T20:00:00.000Z"));
rejects!(rejects_impossible_day, json!("2026-04-31T20:00:00.000Z"));
rejects!(rejects_invalid_month, json!("2026-13-25T20:00:00.000Z"));
rejects!(rejects_invalid_hour, json!("2026-08-25T24:00:00.000Z"));
rejects!(rejects_invalid_minute, json!("2026-08-25T20:60:00.000Z"));
rejects!(rejects_leap_second, json!("2026-08-25T20:00:60.000Z"));
rejects!(rejects_lowercase_utc, json!("2026-08-25T20:00:00.000z"));
rejects!(rejects_space_separator, json!("2026-08-25 20:00:00.000Z"));
rejects!(
    rejects_surrounding_whitespace,
    json!(" 2026-08-25T20:00:00.000Z ")
);
rejects!(rejects_missing_zone, json!("2026-08-25T20:00:00.000"));
rejects!(rejects_malformed_text_without_echo, json!(ECHO_SENTINEL));
rejects!(rejects_empty_string, json!(""));
rejects!(rejects_null, Value::Null);
rejects!(rejects_number, json!(1_777_147_200));
rejects!(rejects_boolean, json!(true));
rejects!(rejects_array, json!([CANONICAL]));
rejects!(rejects_object, json!({"value":CANONICAL}));

#[test]
fn rejects_missing_deleted_at_cell() {
    assert_refusal(&tombstone_archive(None), "missing cell", None);
}

#[test]
fn rejects_deleted_at_omitted_from_manifest_and_row() {
    // Completeness relative to a hostile manifest is not semantic validation.
    let bytes = archive(TABLE, &[tombstone(None)], &["guild_id", "event_id"]);
    assert_refusal(&bytes, "missing field and manifest column", None);
}

#[test]
fn rejects_bad_later_row_after_canonical_prefix() {
    let bad = json!("2026-08-25T20:00:00Z");
    let bytes = archive(
        TABLE,
        &[
            tombstone(Some(json!(CANONICAL))),
            tombstone(Some(bad.clone())),
        ],
        &["guild_id", "event_id", "deleted_at"],
    );
    assert_refusal(&bytes, "seconds-only second row", Some(&bad));
}

#[test]
fn canonical_timestamps_pass_unchanged() {
    for timestamp in [
        CANONICAL,
        "2026-08-25T20:00:00.001Z",
        "2024-02-29T23:59:59.999Z",
        "2000-02-29T00:00:00.000Z",
    ] {
        let contents = inspect_bytes(&tombstone_archive(Some(json!(timestamp)))).unwrap();
        assert_eq!(contents.rows, 1);
        assert_eq!(contents.buffers[TABLE][0]["deleted_at"], json!(timestamp));
    }
}

#[test]
fn unrelated_table_keeps_its_text_timestamp_behavior() {
    let row = json!({"id":"1", "created_at":"2026-08-25T20:00:00Z"});
    let contents = inspect_bytes(&archive(
        "events",
        std::slice::from_ref(&row),
        &["id", "created_at"],
    ))
    .unwrap();
    assert_eq!(contents.buffers["events"][0], *row.as_object().unwrap());
}

#[test]
fn seconds_only_witness_reverses_chronological_text_order() {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    let earlier = "2026-08-25T20:00:00Z";
    let later = "2026-08-25T20:00:00.001Z";
    assert!(
        OffsetDateTime::parse(earlier, &Rfc3339).unwrap()
            < OffsetDateTime::parse(later, &Rfc3339).unwrap()
    );
    assert!(
        earlier > later,
        "TEXT comparison incorrectly treats the older seconds-only tombstone as newer"
    );
    assert!(
        CANONICAL < later,
        "canonical millisecond stamps preserve order"
    );
}

#[cfg(feature = "db")]
#[tokio::test]
async fn generated_canonical_tombstone_dump_round_trips(
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use two_bot_core::backup::dump::{dump, restore};
    use two_bot_testsupport::TestDatabase;

    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("SKIP generated canonical dump: TWO_TEST_DATABASE_URL is not set");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations")).await?;
    let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .or_else(|| std::env::var_os("RUNNER_TEMP"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!("{}-canonical-tombstone.gz", db.name()));
    let result = async {
        sqlx::query("INSERT INTO scheduled_events_tombstone (guild_id,event_id,deleted_at) VALUES ($1,$2,$3)")
            .bind(ECHO_SENTINEL).bind("roundtrip").bind(CANONICAL).execute(db.pool()).await?;
        dump(db.pool(), &path).await?;
        let contents = inspect_bytes(&std::fs::read(&path)?)?;
        let archived = contents.buffers[TABLE][0]["deleted_at"].clone();
        let report = restore(db.pool(), &path).await?;
        let restored: String = sqlx::query_scalar("SELECT deleted_at FROM scheduled_events_tombstone WHERE guild_id=$1 AND event_id=$2")
            .bind(ECHO_SENTINEL).bind("roundtrip").fetch_one(db.pool()).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((archived, report.ok, restored))
    }.await;
    // Explicitly release the fixture even when a fallible operation fails.
    let closed = db.close().await;
    let _ = std::fs::remove_file(&path);
    closed?;
    let (archived, restored_ok, restored) = result?;
    assert_eq!(archived, json!(CANONICAL));
    assert!(restored_ok);
    assert_eq!(restored, CANONICAL);
    Ok(())
}
