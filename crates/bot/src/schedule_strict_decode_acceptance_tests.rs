//! Protected acceptance: strict schedule option decoding (Test Author
//! handoff; the Builder must never edit this file).
//!
//! Locked contract for `/schedule`, `/schedule-remove` and `/schedule-list`
//! option decoding, wired through the real `CommandRuntime::on_interaction`
//! dispatch (defer -> handler -> completion edit -> loopback REST). Every
//! case is behavioral: it dispatches a crafted interaction and asserts the
//! user-visible reply plus the persistent store, never a decoder symbol, so
//! RED means "today's decode is observably wrong", not "a helper is missing".
//!
//! - A present option with the wrong wire type, or an option name sent twice,
//!   is a *decode failure*. It refuses BEFORE any store read or mutation with
//!   an exact literal below: the text names the offending option and carries a
//!   copyable corrected invocation, echoes no user input (mention-safe), and
//!   stays far under Discord's 2,000-unit cap (bounded). No schedule row, no
//!   audit row, no command row changes; Discord sees exactly one private
//!   response (one ephemeral defer + one completion edit, nothing else).
//! - A missing *required* option keeps today's validation shape byte-identical
//!   (`/schedule` without `body` still audits `scheduled.create`/`rejected`
//!   like every other validation failure; `/schedule-remove` without `id`
//!   still takes the empty-guard text with no audit). A missing *optional*
//!   option still means "absent" (`page` omitted is page 1; timings omitted
//!   follow today's validation). Unknown option names stay ignored.
//! - Well-typed `page` handling keeps the pinned texts byte-identical: `page
//!   < 1` refuses, `page > total` refuses after the same bounded read every
//!   list uses, and extreme values never overflow page math (these overlap the
//!   earlier pagination handoff on purpose: this file is the complete decode
//!   contract the Builder runs).
//!
//! RED map (fails on current `main`, flips GREEN only with a correct
//! implementation): every `*_decode_failures_*` test. Today's wrong behavior
//! per matrix: wrong-typed `body` falls into the generic body-empty
//! validation (with an audit row); wrong-typed timings vanish into
//! missing-timing validation (with an audit row); duplicated create options
//! are last-wins (they can write a row); wrong-typed `id` falls into the
//! empty-id guard; duplicated `id` is first-wins (it can delete a row);
//! wrong-typed or duplicated `page` silently lists page 1. GREEN pins (pass
//! today and after): `schedule_create_missing_required_vs_optional_*`,
//! the missing/empty/underflow/out-of-range cases inside the other tests,
//! and every valid control.
//!
//! Runnable recipe (disposable test DB only, never staging/production):
//! ```sh
//! cargo fmt --all -- --check
//! python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot \
//!   --features two-bot-core/db schedule_strict_decode -- --nocapture
//! TWO_GATEWAY_TEST_DATABASE_URL='postgres://agent_test:@agent-testdb:5432/agent_test' \
//!   python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot \
//!   --features two-bot-core/db schedule_strict_decode -- --ignored \
//!   --test-threads=1 --nocapture
//! ```
//! The first `test` run executes the always-run lazy-pool cases (refusal
//! before any store read); the `--ignored` run executes the agent-testdb
//! cases (byte-identical persistent state). Both runs must be fully GREEN
//! after the Builder lands; on this RED head the `decode_failures` tests
//! fail and every other test in this file passes.
//!
//! Builder rules: consume this file's branch/SHA unchanged (see the issue's
//! protected-file manifest), never edit this file, keep the existing
//! `schedule_options` / `schedule_remove_option` unit tests compiling and
//! GREEN, and route any concrete conflict with a pinned control to a Test
//! Author follow-up instead of waiving it.

#![cfg(test)]

use super::*;

// ---------------------------------------------------------------------------
// Locked refusal literals: name the offending option + a copyable corrected
// invocation. Exact match (see module docs for the shape each keeps).
// ---------------------------------------------------------------------------

const INVALID_BODY: &str = "Invalid body option. Use /schedule body:<text> in-minutes:<minutes>.";
const DUPLICATE_BODY: &str =
    "Duplicate body option. Use /schedule body:<text> in-minutes:<minutes>.";
const INVALID_IN_MINUTES: &str =
    "Invalid in-minutes option. Use /schedule body:<text> in-minutes:<minutes>.";
const DUPLICATE_IN_MINUTES: &str =
    "Duplicate in-minutes option. Use /schedule body:<text> in-minutes:<minutes>.";
const INVALID_EVERY_MINUTES: &str =
    "Invalid every-minutes option. Use /schedule body:<text> every-minutes:<minutes>.";
const DUPLICATE_EVERY_MINUTES: &str =
    "Duplicate every-minutes option. Use /schedule body:<text> every-minutes:<minutes>.";
const INVALID_ID: &str = "Invalid id option. Use /schedule-remove id:<schedule-id>.";
const DUPLICATE_ID: &str = "Duplicate id option. Use /schedule-remove id:<schedule-id>.";
const INVALID_PAGE: &str =
    "Invalid page option. Use /schedule-list page:<number> with a whole number from 1.";
const DUPLICATE_PAGE: &str =
    "Duplicate page option. Use /schedule-list page:<number> with a whole number from 1.";

// Today's texts, pinned byte-identical (GREEN controls, not new behavior).
const BODY_EMPTY_TEXT: &str = "Message body must be between 1 and 2000 characters.";
const MISSING_ID_TEXT: &str =
    "No unique scheduled message matches ``. Use the full id from /schedule-list.";
const PAGE_UNDERFLOW_TEXT: &str = "Page must be at least 1.";
// Copy of `schedule_runtime`'s private constant: on the never-usable lazy
// pool any store touch replies with this, so seeing it proves the handler
// read the database instead of refusing first.
const STORE_FAILURE_TEXT: &str = "Schedule command failed; try again.";

/// Refusal replies stay far under Discord's 2,000 UTF-16-unit cap.
const REFUSAL_UNIT_CAP: usize = 500;

/// Full persistent state the schedule slice can touch: every column of every
/// scheduled row and every automation audit row for this guild as ordered
/// JSON (key order and row order deterministic), plus the command-row count.
/// Compared before/after with `assert_eq` for byte-identical refusal paths.
#[derive(Debug, PartialEq, Eq)]
struct ScheduleStoreSnapshot {
    schedules: String,
    audits: String,
    commands: i64,
}

async fn snapshot_schedule_store(pool: &PgPool) -> ScheduleStoreSnapshot {
    let schedules: String = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id), '[]'::jsonb)::text
           FROM scheduled_messages s WHERE guild_id = $1",
    )
    .bind(GUILD_S)
    .fetch_one(pool)
    .await
    .expect("snapshot scheduled rows");
    let audits: String = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY created_at, id), '[]'::jsonb)::text
           FROM automation_audit_log a WHERE guild_id = $1",
    )
    .bind(GUILD_S)
    .fetch_one(pool)
    .await
    .expect("snapshot audit rows");
    let commands: i64 =
        sqlx::query_scalar("SELECT count(*) FROM automation_commands WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_one(pool)
            .await
            .expect("count command rows");
    ScheduleStoreSnapshot {
        schedules,
        audits,
        commands,
    }
}

/// The completion-edit bodies of `invocations` sequential dispatches, in
/// order. Pins the transport shape shared by every refusal and control:
/// exactly one ephemeral defer (type 5, flags 64) plus one completion edit
/// per invocation — one private response — and no other Discord call.
async fn completion_edit_bodies(mock: &MockRest, invocations: usize) -> Vec<serde_json::Value> {
    let requests = mock.requests();
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        invocations,
        "one ephemeral defer per invocation"
    );
    for callback in &callbacks {
        let json: serde_json::Value =
            serde_json::from_slice(&callback.body).expect("callback json");
        assert_eq!(json["type"], 5, "deferred reply");
        assert_eq!(json["data"]["flags"], 64, "ephemeral defer");
    }
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(
        edits.len(),
        invocations,
        "one completion edit per invocation, no extra messages"
    );
    assert_eq!(
        requests.len(),
        invocations * 2,
        "no REST calls beyond defer + completion: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|r| r.method == "POST" || r.method == "PATCH"),
        "no DELETE/PUT/message post on refusal: {requests:?}"
    );
    assert!(mock.deletes().is_empty(), "no Discord delete on refusal");
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "no channel post on refusal"
    );
    edits
        .iter()
        .map(|edit| serde_json::from_slice(&edit.body).expect("completion json"))
        .collect()
}

/// A refusal is mention-safe (nothing parses as a mention, hostile input is
/// never echoed) and bounded.
fn assert_safe_refusal(reply: &serde_json::Value) {
    assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
    let content = reply["content"].as_str().expect("refusal is text");
    assert!(
        !content.contains("@everyone") && !content.contains("@here"),
        "refusal echoes no mass mention: {content:?}"
    );
    assert!(
        content.encode_utf16().count() <= REFUSAL_UNIT_CAP,
        "refusal stays bounded ({} units): {content:?}",
        content.encode_utf16().count()
    );
}

fn valid_body() -> CommandDataOption {
    option("body", CommandOptionValue::String("hello".to_owned()))
}

fn valid_in_minutes() -> CommandDataOption {
    option("in-minutes", CommandOptionValue::Integer(30))
}

// ---------------------------------------------------------------------------
// `/schedule`: wrong types and duplicates refuse before any store read.
// ---------------------------------------------------------------------------

/// RED on current `main`: wrong-typed and duplicated create options refuse
/// before any store read or mutation. The pool is never usable, so any store
/// touch replies `STORE_FAILURE_TEXT` instead of the decode literal.
#[tokio::test]
async fn schedule_create_strict_decode_refuses_before_any_store_read() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);

    let hostile_body = format!("@everyone {}", "x".repeat(200));
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        // Wrong wire types (each option keeps a valid sibling so only the
        // mistyped option can explain the refusal).
        (
            vec![
                option("body", CommandOptionValue::Integer(7)),
                valid_in_minutes(),
            ],
            INVALID_BODY,
        ),
        (
            vec![
                option("body", CommandOptionValue::Boolean(true)),
                valid_in_minutes(),
            ],
            INVALID_BODY,
        ),
        (
            vec![
                valid_body(),
                option("in-minutes", CommandOptionValue::String("30".to_owned())),
            ],
            INVALID_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("in-minutes", CommandOptionValue::Number(2.5)),
            ],
            INVALID_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("every-minutes", CommandOptionValue::Boolean(false)),
            ],
            INVALID_EVERY_MINUTES,
        ),
        // Duplicated names refuse even when every value is well-typed; the
        // hostile second body proves the refusal echoes no user input.
        (
            vec![
                valid_body(),
                option("body", CommandOptionValue::String(hostile_body)),
                valid_in_minutes(),
            ],
            DUPLICATE_BODY,
        ),
        (
            vec![
                valid_body(),
                valid_in_minutes(),
                option("in-minutes", CommandOptionValue::Integer(31)),
            ],
            DUPLICATE_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("in-minutes", CommandOptionValue::String("a".to_owned())),
                option("in-minutes", CommandOptionValue::String("b".to_owned())),
            ],
            DUPLICATE_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("every-minutes", CommandOptionValue::Integer(60)),
                option("every-minutes", CommandOptionValue::Integer(90)),
            ],
            DUPLICATE_EVERY_MINUTES,
        ),
        // A duplicated name refuses even when one value is also mistyped:
        // name-count wins over type-check.
        (
            vec![
                valid_body(),
                option("body", CommandOptionValue::Integer(9)),
                valid_in_minutes(),
            ],
            DUPLICATE_BODY,
        ),
    ];
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule", Some(CHANNEL), options.clone()))
            .await;
    }

    let replies = completion_edit_bodies(&mock, cases.len()).await;
    for (reply, (_, want)) in replies.iter().zip(cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_ne!(
            &reply["content"], STORE_FAILURE_TEXT,
            "decode refused before any store access"
        );
        assert_safe_refusal(reply);
    }
    mock.shutdown().await;
}

/// GREEN pin: a missing required `body` keeps today's validation shape
/// (generic body-empty text, still before any store read), proving the new
/// decode path only triggers on present-but-unusable options.
#[tokio::test]
async fn schedule_create_missing_body_keeps_validation_refusal_before_store_read() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);

    runtime
        .on_interaction(&slash("schedule", Some(CHANNEL), vec![valid_in_minutes()]))
        .await;

    let replies = completion_edit_bodies(&mock, 1).await;
    assert_eq!(&replies[0]["content"], BODY_EMPTY_TEXT);
    assert_ne!(
        &replies[0]["content"], STORE_FAILURE_TEXT,
        "validation still precedes the store read"
    );
    assert_safe_refusal(&replies[0]);
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// `/schedule-remove`: wrong types and duplicates refuse before any store read.
// ---------------------------------------------------------------------------

/// RED on current `main`: a wrong-typed or duplicated `id` refuses before
/// any store read. Missing and empty ids keep today's empty-guard text
/// (GREEN pins inside the same dispatch matrix).
#[tokio::test]
async fn schedule_remove_strict_decode_refuses_before_any_store_read() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);

    let hostile_id = format!("@everyone {}", "y".repeat(120));
    assert!(hostile_id.encode_utf16().count() > 128);
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (
            vec![option("id", CommandOptionValue::Integer(123))],
            INVALID_ID,
        ),
        (
            vec![option("id", CommandOptionValue::Boolean(true))],
            INVALID_ID,
        ),
        (
            vec![option("id", CommandOptionValue::Number(1.5))],
            INVALID_ID,
        ),
        (
            vec![
                option("id", CommandOptionValue::String("aaa".to_owned())),
                option("id", CommandOptionValue::String("bbb".to_owned())),
            ],
            DUPLICATE_ID,
        ),
        (
            vec![
                option("id", CommandOptionValue::String(hostile_id)),
                option("id", CommandOptionValue::String("ccc".to_owned())),
            ],
            DUPLICATE_ID,
        ),
        // GREEN pins: missing and empty ids keep the empty-guard text.
        (Vec::new(), MISSING_ID_TEXT),
        (
            vec![option("id", CommandOptionValue::String(String::new()))],
            MISSING_ID_TEXT,
        ),
    ];
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule-remove", Some(CHANNEL), options.clone()))
            .await;
    }

    let replies = completion_edit_bodies(&mock, cases.len()).await;
    for (reply, (_, want)) in replies.iter().zip(cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_ne!(
            &reply["content"], STORE_FAILURE_TEXT,
            "decode refused before any store access"
        );
        assert_safe_refusal(reply);
    }
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// `/schedule-list`: wrong types and duplicates refuse before any store read.
// ---------------------------------------------------------------------------

/// RED on current `main`: a wrong-typed or duplicated `page` refuses before
/// any store read instead of silently listing page 1. Page 0 keeps the
/// pinned underflow text (GREEN pin in the same matrix).
#[tokio::test]
async fn schedule_list_strict_decode_refuses_before_any_store_read() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);

    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (
            vec![option("page", CommandOptionValue::String("2".to_owned()))],
            INVALID_PAGE,
        ),
        (
            vec![option("page", CommandOptionValue::Boolean(true))],
            INVALID_PAGE,
        ),
        (
            vec![option("page", CommandOptionValue::Number(2.0))],
            INVALID_PAGE,
        ),
        (
            vec![
                option("page", CommandOptionValue::Integer(1)),
                option("page", CommandOptionValue::Integer(2)),
            ],
            DUPLICATE_PAGE,
        ),
        (
            vec![
                option("page", CommandOptionValue::Integer(1)),
                option("page", CommandOptionValue::String("2".to_owned())),
            ],
            DUPLICATE_PAGE,
        ),
        // GREEN pin: page 0 keeps the pinned underflow text.
        (
            vec![option("page", CommandOptionValue::Integer(0))],
            PAGE_UNDERFLOW_TEXT,
        ),
    ];
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule-list", Some(CHANNEL), options.clone()))
            .await;
    }

    let replies = completion_edit_bodies(&mock, cases.len()).await;
    for (reply, (_, want)) in replies.iter().zip(cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_ne!(
            &reply["content"], STORE_FAILURE_TEXT,
            "decode refused before any store access"
        );
        assert_safe_refusal(reply);
    }
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Persistent-state proofs (disposable agent-testdb only).
// ---------------------------------------------------------------------------

/// RED on current `main`: every create decode failure leaves the persistent
/// schedule, command and audit state byte-identical and answers exactly one
/// private response. A valid one-shot control then proves the fixture was
/// live and the valid path is unchanged.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_decode_failures_leave_store_untouched() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "keep111").await;
    db.seed_schedule(GUILD_S, "keep222").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let before = snapshot_schedule_store(&db.pool).await;
    assert_eq!(db.schedule_rows().await.len(), 2, "two seeded rows");
    assert!(db.schedule_audits().await.is_empty(), "no audit rows yet");

    let hostile_body = format!("@everyone {}", "x".repeat(200));
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (
            vec![
                option("body", CommandOptionValue::Integer(7)),
                valid_in_minutes(),
            ],
            INVALID_BODY,
        ),
        (
            vec![
                option("body", CommandOptionValue::Boolean(true)),
                valid_in_minutes(),
            ],
            INVALID_BODY,
        ),
        (
            vec![
                valid_body(),
                option("in-minutes", CommandOptionValue::String("30".to_owned())),
            ],
            INVALID_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("in-minutes", CommandOptionValue::Number(2.5)),
            ],
            INVALID_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("every-minutes", CommandOptionValue::Boolean(false)),
            ],
            INVALID_EVERY_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("body", CommandOptionValue::String(hostile_body)),
                valid_in_minutes(),
            ],
            DUPLICATE_BODY,
        ),
        (
            vec![
                valid_body(),
                valid_in_minutes(),
                option("in-minutes", CommandOptionValue::Integer(31)),
            ],
            DUPLICATE_IN_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("every-minutes", CommandOptionValue::Integer(60)),
                option("every-minutes", CommandOptionValue::Integer(90)),
            ],
            DUPLICATE_EVERY_MINUTES,
        ),
        (
            vec![
                valid_body(),
                option("body", CommandOptionValue::Integer(9)),
                valid_in_minutes(),
            ],
            DUPLICATE_BODY,
        ),
    ];
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule", Some(CHANNEL), options.clone()))
            .await;
    }

    let replies = completion_edit_bodies(&mock, cases.len()).await;
    for (reply, (_, want)) in replies.iter().zip(cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_safe_refusal(reply);
    }
    assert_eq!(
        snapshot_schedule_store(&db.pool).await,
        before,
        "decode failures change no schedule, command or audit row"
    );

    // Valid control: the same router still creates a one-shot with today's
    // exact confirmation shape, row and audit.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![valid_body(), valid_in_minutes()],
        ))
        .await;
    let rows = db.schedule_rows().await;
    assert_eq!(rows.len(), 3, "control writes exactly one row");
    let (id, channel_id) = rows
        .iter()
        .find(|(id, _)| id != "keep111" && id != "keep222")
        .expect("the new row");
    assert_eq!(channel_id, CHANNEL_S, "schedule binds the invoking channel");
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), cases.len() + 1, "control completes its defer");
    let confirm: serde_json::Value = serde_json::from_slice(&edits[cases.len()].body).unwrap();
    assert!(
        confirm["content"]
            .as_str()
            .expect("confirm text")
            .starts_with(&format!("Scheduled message `{id}` at ")),
        "control keeps today's confirmation shape: {}",
        confirm["content"]
    );
    assert_eq!(
        db.schedule_audits().await,
        vec![("scheduled.create".to_owned(), id.clone(), "ok".to_owned())],
        "only the valid control audits"
    );
    mock.shutdown().await;
    db.close().await;
}

/// GREEN pins: missing-required refuses with today's validation shape (text
/// + one `scheduled.create`/`rejected` audit, no row) while missing-optional
/// timings proceed, and unknown option names stay ignored. Passes today and
/// after the Builder lands.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_missing_required_vs_optional_keeps_todays_shape() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Missing required `body`: today's body-empty validation refusal.
    runtime
        .on_interaction(&slash("schedule", Some(CHANNEL), vec![valid_in_minutes()]))
        .await;
    // Missing optional `every-minutes`: a valid one-shot.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![valid_body(), valid_in_minutes()],
        ))
        .await;
    // Missing optional `in-minutes`: a valid recurring schedule.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hourly".to_owned())),
                option("every-minutes", CommandOptionValue::Integer(90)),
            ],
        ))
        .await;
    // Unknown option names stay ignored: still a valid one-shot.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                valid_body(),
                valid_in_minutes(),
                option("bogus", CommandOptionValue::String("x".to_owned())),
            ],
        ))
        .await;

    let replies = completion_edit_bodies(&mock, 4).await;
    assert_eq!(&replies[0]["content"], BODY_EMPTY_TEXT);

    let rows = db.schedule_rows().await;
    assert_eq!(rows.len(), 3, "three valid controls write three rows");
    for (_, channel_id) in &rows {
        assert_eq!(channel_id, CHANNEL_S, "schedules bind the invoking channel");
    }
    let recurring = rows
        .iter()
        .find(|(id, _)| {
            replies[2]["content"]
                .as_str()
                .expect("confirm text")
                .contains(id)
        })
        .expect("recurring confirm names its row");
    let interval: Option<i64> =
        sqlx::query_scalar("SELECT interval_seconds FROM scheduled_messages WHERE id = $1")
            .bind(&recurring.0)
            .fetch_one(&db.pool)
            .await
            .expect("interval");
    assert_eq!(interval, Some(90 * 60), "every-minutes becomes seconds");
    assert!(
        replies[2]["content"]
            .as_str()
            .expect("confirm text")
            .ends_with("every 90m."),
        "recurring confirm keeps today's shape: {}",
        replies[2]["content"]
    );
    for reply in &replies[1..] {
        assert!(
            reply["content"]
                .as_str()
                .expect("confirm text")
                .starts_with("Scheduled message `"),
            "valid controls confirm: {}",
            reply["content"]
        );
    }

    let audits = db.schedule_audits().await;
    assert_eq!(audits.len(), 4, "one audit row per invocation");
    assert_eq!(
        (audits[0].0.as_str(), audits[0].2.as_str()),
        ("scheduled.create", "rejected"),
        "missing body still audits a validation rejection first: {audits:?}"
    );
    assert!(
        audits[1..]
            .iter()
            .all(|(action, _, outcome)| action == "scheduled.create" && outcome == "ok"),
        "valid controls audit ok: {audits:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

/// RED on current `main`: every remove decode failure leaves the persistent
/// schedule, command and audit state byte-identical and answers exactly one
/// private response. Missing/empty ids keep today's guard text (GREEN), and
/// a valid prefix control then deletes exactly one row.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_remove_decode_failures_leave_store_untouched() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "keep111").await;
    db.seed_schedule(GUILD_S, "keep222").await;
    db.seed_schedule(GUILD_S, "xyz999").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let before = snapshot_schedule_store(&db.pool).await;
    assert_eq!(db.schedule_rows().await.len(), 3, "three seeded rows");

    let hostile_id = format!("@everyone {}", "y".repeat(120));
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (
            vec![option("id", CommandOptionValue::Integer(123))],
            INVALID_ID,
        ),
        (
            vec![option("id", CommandOptionValue::Boolean(true))],
            INVALID_ID,
        ),
        (
            vec![
                option("id", CommandOptionValue::String("xyz".to_owned())),
                option("id", CommandOptionValue::String("keep".to_owned())),
            ],
            DUPLICATE_ID,
        ),
        (
            vec![
                option("id", CommandOptionValue::String(hostile_id)),
                option("id", CommandOptionValue::String("xyz".to_owned())),
            ],
            DUPLICATE_ID,
        ),
        // GREEN pins: missing and empty ids keep the empty-guard text.
        (Vec::new(), MISSING_ID_TEXT),
        (
            vec![option("id", CommandOptionValue::String(String::new()))],
            MISSING_ID_TEXT,
        ),
    ];
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule-remove", Some(CHANNEL), options.clone()))
            .await;
    }

    let replies = completion_edit_bodies(&mock, cases.len()).await;
    for (reply, (_, want)) in replies.iter().zip(cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_safe_refusal(reply);
    }
    assert_eq!(
        snapshot_schedule_store(&db.pool).await,
        before,
        "decode failures delete no row and audit nothing"
    );

    // Valid control: the uniquely-resolving prefix still deletes exactly one
    // row through the same router.
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("xyz".to_owned()))],
        ))
        .await;
    let remaining: Vec<String> = db
        .schedule_rows()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        remaining,
        vec!["keep111".to_owned(), "keep222".to_owned()],
        "valid prefix still deletes its row after the decode matrix"
    );
    assert_eq!(
        db.schedule_audits().await,
        vec![(
            "scheduled.delete".to_owned(),
            "xyz999".to_owned(),
            "ok".to_owned()
        )],
        "only the valid control audits"
    );
    mock.shutdown().await;
    db.close().await;
}

/// RED on current `main` for the decode half, GREEN pins for the page half:
/// wrong-typed and duplicated `page` options refuse without touching the
/// store, while omitted/page-1, page 0/negative, out-of-range and extreme
/// pages keep their pinned texts byte-identical.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_decode_failures_and_page_matrix() {
    let db = TestDb::new().await;
    let fixture = schedule_list_fixture_rows();
    for row in fixture.iter().take(3) {
        db.seed_schedule_fixture_row(GUILD_S, row).await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let before = snapshot_schedule_store(&db.pool).await;
    assert_eq!(db.schedule_rows().await.len(), 3, "three seeded rows");

    // RED half: present-but-unusable `page` refuses.
    let decode_cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (
            vec![option("page", CommandOptionValue::String("2".to_owned()))],
            INVALID_PAGE,
        ),
        (
            vec![option("page", CommandOptionValue::Boolean(true))],
            INVALID_PAGE,
        ),
        (
            vec![option("page", CommandOptionValue::Number(1.5))],
            INVALID_PAGE,
        ),
        (
            vec![
                option("page", CommandOptionValue::Integer(1)),
                option("page", CommandOptionValue::Integer(2)),
            ],
            DUPLICATE_PAGE,
        ),
        (
            vec![
                option("page", CommandOptionValue::Integer(1)),
                option("page", CommandOptionValue::String("2".to_owned())),
            ],
            DUPLICATE_PAGE,
        ),
    ];
    for (options, _) in &decode_cases {
        runtime
            .on_interaction(&slash("schedule-list", Some(CHANNEL), options.clone()))
            .await;
    }
    // GREEN half: the pinned page matrix on a 3-row (single-page) guild.
    invoke_schedule_list(&runtime, None).await;
    invoke_schedule_list(&runtime, Some(1)).await;
    invoke_schedule_list(&runtime, Some(0)).await;
    invoke_schedule_list(&runtime, Some(-1)).await;
    invoke_schedule_list(&runtime, Some(2)).await;
    invoke_schedule_list(&runtime, Some(i64::MAX)).await;

    let total = decode_cases.len() + 6;
    let replies = completion_edit_bodies(&mock, total).await;
    for (reply, (_, want)) in replies.iter().zip(decode_cases.iter()) {
        assert_eq!(&reply["content"], want);
        assert_safe_refusal(reply);
    }
    let pages: Vec<&str> = replies[decode_cases.len()..]
        .iter()
        .map(|reply| reply["content"].as_str().expect("list text"))
        .collect();
    assert_eq!(pages[0], pages[1], "omitting page means page 1 exactly");
    let listed: Vec<&str> = schedule_list_row_lines(pages[0])
        .iter()
        .map(|line| schedule_list_row_id(line))
        .collect();
    assert_eq!(
        listed,
        fixture
            .iter()
            .take(3)
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        "page 1 lists the whole small guild in order"
    );
    assert_eq!(pages[2], PAGE_UNDERFLOW_TEXT);
    assert_eq!(pages[3], PAGE_UNDERFLOW_TEXT);
    assert_eq!(
        pages[4],
        "Page 2 is out of range: this server has 1 page(s) of schedules."
    );
    assert_eq!(
        pages[5],
        format!(
            "Page {} is out of range: this server has 1 page(s) of schedules.",
            i64::MAX
        ),
        "extreme page values refuse cleanly instead of overflowing page math"
    );
    assert_eq!(
        snapshot_schedule_store(&db.pool).await,
        before,
        "decode failures and refused pages change no row"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "listing writes no audit rows"
    );
    mock.shutdown().await;
    db.close().await;
}

// Inspect/edit were added after the original create/remove/list handoff. These
// regressions are GREEN at their implementation baseline; lenient dispatch and
// first-wins mutants must turn them RED through the real gateway runtime.
const MANAGEMENT_ID: &str = "aabbccdd001122334455667788990001";
const MANAGEMENT_BODY: &str = "private-decode-replacement-sentinel";
type ManagementDecodeCase = (Vec<CommandDataOption>, String);

fn management_decode_cases(command: &str, revision: &str) -> Vec<ManagementDecodeCase> {
    let (fields, invocation) = match command {
        "schedule-inspect" => (
            vec![("id", MANAGEMENT_ID), ("view", "metadata")],
            "/schedule-inspect id:<schedule-id> view:<metadata|body>",
        ),
        "schedule-edit" => (
            vec![
                ("id", MANAGEMENT_ID),
                ("revision", revision),
                ("body", MANAGEMENT_BODY),
            ],
            "/schedule-edit id:<schedule-id> revision:<revision> body:<text>",
        ),
        _ => panic!("only the two management commands belong in this matrix"),
    };
    let valid = || {
        fields
            .iter()
            .map(|(name, value)| option(name, CommandOptionValue::String((*value).to_owned())))
            .collect::<Vec<_>>()
    };
    let mut cases = Vec::new();
    for (index, (name, _)) in fields.iter().enumerate() {
        for wrong in [
            CommandOptionValue::Integer(7),
            CommandOptionValue::Boolean(true),
            CommandOptionValue::Number(2.5),
        ] {
            let mut options = valid();
            options[index].value = wrong;
            cases.push((options, format!("Invalid {name} option. Use {invocation}.")));
        }
        // All-string duplicates must refuse, even when first-wins would be a
        // valid operation. Mixed-type duplicates pin name-count before type.
        for (first, second) in [
            (
                CommandOptionValue::String(fields[index].1.to_owned()),
                CommandOptionValue::String("@everyone @here duplicate-input-sentinel".to_owned()),
            ),
            (
                CommandOptionValue::String(fields[index].1.to_owned()),
                CommandOptionValue::Integer(9),
            ),
            (
                CommandOptionValue::Boolean(false),
                CommandOptionValue::String(fields[index].1.to_owned()),
            ),
        ] {
            let mut options = valid();
            options[index].value = first;
            options.push(option(name, second));
            cases.push((
                options,
                format!("Duplicate {name} option. Use {invocation}."),
            ));
        }
    }
    if command == "schedule-inspect" {
        for view in [
            "",
            "BODY",
            "summary",
            "@everyone @here unknown-view-sentinel",
        ] {
            let mut options = valid();
            options[1].value = CommandOptionValue::String(view.to_owned());
            cases.push((
                options,
                "Unknown view. Use /schedule-inspect id:<schedule-id> view:<metadata|body>."
                    .to_owned(),
            ));
        }
    }
    cases
}

async fn management_decode_refusals(
    pool: &PgPool,
    command: &str,
    cases: Vec<ManagementDecodeCase>,
) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    let guard = runtime.dispatch_guard();
    let before = if pool.is_closed() {
        None
    } else {
        Some(snapshot_schedule_store(pool).await)
    };
    for (index, (options, expected)) in cases.iter().enumerate() {
        assert!(runtime.dispatch(&interaction_event(slash(
            command,
            Some(CHANNEL),
            options.clone()
        ))));
        wait_for(
            || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
            "management decode settles",
        )
        .await;
        let replies = completion_edit_bodies(&mock, index + 1).await;
        let reply = &replies[index];
        assert_eq!(
            reply["content"], *expected,
            "{command} case {index}: {options:?}"
        );
        assert_ne!(
            reply["content"], STORE_FAILURE_TEXT,
            "decode must precede the CLOSED pool"
        );
        assert_safe_refusal(reply);
        assert_eq!(
            reply["allowed_mentions"],
            serde_json::json!({"parse":[],"roles":[],"users":[],"replied_user":false})
        );
        let requests = mock.requests();
        assert!(
            requests[index * 2].path.ends_with("/callback"),
            "defer before completion"
        );
        assert!(requests[index * 2 + 1]
            .path
            .starts_with("/api/v10/webhooks/1111/"));
        assert!(requests[index * 2 + 1]
            .path
            .ends_with("/messages/@original"));
        if let Some(before) = &before {
            assert_eq!(
                &snapshot_schedule_store(pool).await,
                before,
                "{command} case {index} changes no schedule, audit or command row"
            );
        }
    }
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
}

async fn closed_management_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-connected parser fixture, no DB credentials");
    pool.close().await;
    assert!(pool.is_closed());
    pool
}

#[tokio::test]
async fn schedule_inspect_strict_decode_refuses_before_closed_pool_access() {
    let pool = closed_management_pool().await;
    management_decode_refusals(
        &pool,
        "schedule-inspect",
        management_decode_cases("schedule-inspect", "unused-revision"),
    )
    .await;
}

#[tokio::test]
async fn schedule_edit_strict_decode_refuses_before_closed_pool_access() {
    let pool = closed_management_pool().await;
    management_decode_refusals(
        &pool,
        "schedule-edit",
        management_decode_cases("schedule-edit", "unused-revision"),
    )
    .await;
}

#[tokio::test]
async fn schedule_management_strict_decode_preserves_real_store_per_invocation() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded migrated disposable database");
    sqlx::query("INSERT INTO scheduled_messages (id,guild_id,channel_id,body,next_run_at,enabled,created_by,created_at,updated_by,updated_at) VALUES ($1,$2,'3333','existing-private-body-sentinel','2030-01-01T00:00:00.000Z',TRUE,'77','2026-10-01T00:00:00.000Z','77','2026-10-01T00:00:00.000Z')")
        .bind(MANAGEMENT_ID).bind(GUILD_S).execute(db.pool()).await.expect("seed valid target so first-wins would really edit it");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    runtime
        .on_interaction(&slash(
            "schedule-inspect",
            Some(CHANNEL),
            vec![option(
                "id",
                CommandOptionValue::String(MANAGEMENT_ID.to_owned()),
            )],
        ))
        .await;
    let metadata = completion_edit_bodies(&mock, 1).await;
    let token = metadata[0]["content"]
        .as_str()
        .expect("metadata text")
        .lines()
        .find_map(|line| line.strip_prefix("Revision: "))
        .expect("valid current revision")
        .to_owned();
    assert!(!token.is_empty());
    drop(runtime);
    mock.shutdown().await;
    let before = snapshot_schedule_store(db.pool()).await;
    management_decode_refusals(
        db.pool(),
        "schedule-inspect",
        management_decode_cases("schedule-inspect", &token),
    )
    .await;
    management_decode_refusals(
        db.pool(),
        "schedule-edit",
        management_decode_cases("schedule-edit", &token),
    )
    .await;
    assert_eq!(snapshot_schedule_store(db.pool()).await, before);
    db.close().await.expect("drop own fixture");
}
