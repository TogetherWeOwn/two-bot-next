//! Rollback import contract: validate canonical tombstone timestamps both when
//! parsing a file and when applying publicly constructed ImportRow values.
//! No production interface or existing acceptance test is changed.

use serde_json::{json, Value};
use two_bot_cutover::rollback_import::{apply_import, parse_import, ImportRow};
use two_bot_testsupport::TestDatabase;

const TABLE: &str = "scheduled_events_tombstone";
const CANONICAL: &str = "2026-08-25T20:00:00.000Z";
const ECHO_SENTINEL: &str = "tombstone_cell_do_not_echo_81dc";
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn tombstone(deleted_at: Option<Value>) -> Value {
    let mut row = json!({"guild_id":ECHO_SENTINEL, "event_id":ECHO_SENTINEL});
    if let Some(value) = deleted_at {
        row["deleted_at"] = value;
    }
    row
}

fn line(table: &str, row: &Value) -> String {
    json!({"table":table,"row":row}).to_string() + "\n"
}

fn assert_redacted(message: &str, cell: Option<&Value>) {
    assert!(message.len() <= 512, "refusal must be bounded");
    assert!(
        !message.contains(ECHO_SENTINEL),
        "refusal echoed a row cell"
    );
    if let Some(raw) = cell.and_then(Value::as_str).filter(|raw| raw.len() >= 8) {
        assert!(!message.contains(raw), "refusal echoed deleted_at");
    }
}

fn assert_refusal(contents: &str, label: &str, cell: Option<&Value>) {
    let result = parse_import(contents);
    assert!(
        result.is_err(),
        "parse_import accepted {label} tombstone deleted_at; restore must refuse, not normalize"
    );
    assert_redacted(&result.unwrap_err().to_string(), cell);
}

macro_rules! rejects {
    ($name:ident, $value:expr) => {
        #[test]
        fn $name() {
            assert!(parse_import(&line(TABLE, &tombstone(Some(json!(CANONICAL))))).is_ok());
            let value = $value;
            assert_refusal(
                &line(TABLE, &tombstone(Some(value.clone()))),
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
fn rejects_missing_deleted_at() {
    assert_refusal(&line(TABLE, &tombstone(None)), "missing field", None);
}

#[test]
fn rejects_bad_later_line_after_canonical_prefix() {
    let bad = json!("2026-08-25T20:00:00Z");
    let contents = line(TABLE, &tombstone(Some(json!(CANONICAL))))
        + "\n"
        + &line(TABLE, &tombstone(Some(bad.clone())));
    assert_refusal(&contents, "seconds-only later line", Some(&bad));
}

#[test]
fn canonical_timestamps_pass_unchanged() {
    for timestamp in [
        CANONICAL,
        "2026-08-25T20:00:00.001Z",
        "2024-02-29T23:59:59.999Z",
        "2000-02-29T00:00:00.000Z",
    ] {
        let row = tombstone(Some(json!(timestamp)));
        let parsed = parse_import(&line(TABLE, &row)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].table, TABLE);
        assert_eq!(parsed[0].row, row);
    }
}

#[test]
fn unrelated_table_keeps_its_timestamp_behavior() {
    let row = json!({"id":"ticket-control", "created_at":"2026-08-25T20:00:00+00:00", "deleted_at":"unrelated text"});
    let parsed = parse_import(&line("tickets", &row)).unwrap();
    assert_eq!(parsed[0].row, row);
}

async fn database() -> Result<Option<TestDatabase>, Box<dyn std::error::Error + Send + Sync>> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("SKIP constructed ImportRow apply: TWO_TEST_DATABASE_URL is not set");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    // Use the shared guarded lifecycle, with only the actual table migration
    // needed here. No bespoke URL guard, admin connection, or production seam.
    let db =
        TestDatabase::create(&url, &sqlx::migrate::Migrator::with_migrations(Vec::new())).await?;
    let setup = sqlx::raw_sql(include_str!(
        "../migrations/0452_scheduled_events_tombstone.sql"
    ))
    .execute(db.pool())
    .await;
    if let Err(error) = setup {
        db.close().await?;
        return Err(error.into());
    }
    Ok(Some(db))
}

#[tokio::test]
async fn constructed_import_rows_reject_before_any_mutation() -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let result = async {
        // A sequence is intentionally nontransactional. This witness catches
        // "insert first, validate later, then roll back" as well as commits.
        // It lives only inside the owned disposable fixture.
        sqlx::raw_sql(
            "CREATE SEQUENCE tombstone_write_witness; \
             CREATE FUNCTION witness_tombstone_write() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('tombstone_write_witness'); RETURN NEW; END $$; \
             CREATE TRIGGER witness_tombstone_write BEFORE INSERT ON scheduled_events_tombstone \
             FOR EACH ROW EXECUTE FUNCTION witness_tombstone_write();",
        )
        .execute(db.pool())
        .await?;
        let cases = [
            ("seconds-only", Some(json!("2026-08-25T20:00:00Z"))),
            ("offset", Some(json!("2026-08-25T20:00:00.000+00:00"))),
            ("overprecision", Some(json!("2026-08-25T20:00:00.0001Z"))),
            ("invalid calendar", Some(json!("2026-02-29T20:00:00.000Z"))),
            ("malformed", Some(json!(ECHO_SENTINEL))),
            ("null", Some(Value::Null)),
            ("number", Some(json!(123))),
            ("boolean", Some(json!(true))),
            ("array", Some(json!([CANONICAL]))),
            ("object", Some(json!({"value":CANONICAL}))),
            ("missing", None),
        ];
        let mut observations = Vec::new();
        for (index, (label, value)) in cases.into_iter().enumerate() {
            let mut prefix = tombstone(Some(json!(CANONICAL)));
            prefix["event_id"] = json!(format!("canonical-prefix-{index}"));
            let mut invalid = tombstone(value.clone());
            invalid["event_id"] = json!(format!("invalid-{index}"));
            let rows = [
                ImportRow {
                    table: TABLE.to_owned(),
                    row: prefix,
                },
                ImportRow {
                    table: TABLE.to_owned(),
                    row: invalid,
                },
            ];
            let applied = apply_import(db.pool(), &rows).await;
            let error = applied.err().map(|error| error.to_string());
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_events_tombstone")
                .fetch_one(db.pool())
                .await?;
            let touched: bool = sqlx::query_scalar("SELECT is_called FROM tombstone_write_witness")
                .fetch_one(db.pool())
                .await?;
            observations.push((label, value, error, count, touched));
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(observations)
    }
    .await;
    let closed = db.close().await;
    closed?;
    let observations = result?;
    // All assertions follow explicit teardown, including the expected RED.
    for (label, value, error, count, touched) in observations {
        assert!(error.is_some(), "apply_import accepted constructed {label} tombstone; count={count}, write_witness={touched}");
        assert_redacted(error.as_deref().unwrap(), value.as_ref());
        assert_eq!(count, 0, "{label}: import wrote rows before refusal");
        assert!(
            !touched,
            "{label}: import attempted mutation before validation"
        );
    }
    Ok(())
}

#[tokio::test]
async fn constructed_canonical_rows_apply_unchanged_and_idempotently() -> TestResult {
    let Some(db) = database().await? else {
        return Ok(());
    };
    let result = async {
        let row = tombstone(Some(json!(CANONICAL)));
        let rows = [ImportRow {
            table: TABLE.to_owned(),
            row,
        }];
        let first = apply_import(db.pool(), &rows).await?;
        let second = apply_import(db.pool(), &rows).await?;
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT deleted_at FROM scheduled_events_tombstone")
                .fetch_all(db.pool())
                .await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, stored))
    }
    .await;
    let closed = db.close().await;
    closed?;
    let (first, second, stored) = result?;
    assert_eq!((first.inserted, first.skipped, first.divergent), (1, 0, 0));
    assert_eq!(
        (second.inserted, second.skipped, second.divergent),
        (0, 1, 0)
    );
    assert_eq!(stored, vec![CANONICAL.to_owned()]);
    Ok(())
}
