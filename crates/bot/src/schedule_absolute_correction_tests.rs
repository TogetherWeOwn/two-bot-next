//! Additive correction tests; the original absolute-time acceptance stays
//! immutable. Breaks caught: ignoring duplicate/oversized `at`, last-wins
//! decoding, touching the store on a refusal, and emitting a channel effect.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use two_bot_testsupport::TestDatabase;

pub(super) async fn state(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'schedules', (SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id), '[]'::jsonb) FROM scheduled_messages s),
          'audits', (SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM automation_audit_log a),
          'commands', (SELECT COALESCE(jsonb_agg(to_jsonb(c) ORDER BY c.guild_id, c.name), '[]'::jsonb) FROM automation_commands c))",
    )
    .fetch_one(pool)
    .await
    .expect("capture complete schedule, audit and command state")
}

fn cases() -> Vec<(Vec<CommandDataOption>, bool)> {
    let now = u64::try_from(now_millis_for_test()).expect("unsigned fixture clock");
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now + 3_600_000) * 1_000_000)
        .expect("future fixture instant")
        .format(&Rfc3339)
        .expect("explicit-offset fixture");
    let mut cases = Vec::new();
    // Identical duplicates still refuse; mixed types/values must not silently
    // choose the first or last value. Both orders carry a usable future value.
    for (first, second) in [
        (
            CommandOptionValue::String(at.clone()),
            CommandOptionValue::String(at.clone()),
        ),
        (
            CommandOptionValue::String(at.clone()),
            CommandOptionValue::Integer(7),
        ),
        (
            CommandOptionValue::Integer(7),
            CommandOptionValue::String(at.clone()),
        ),
    ] {
        cases.push((
            vec![
                option(
                    "body",
                    CommandOptionValue::String("private body".to_owned()),
                ),
                option("at", first),
                option("at", second),
            ],
            true,
        ));
    }
    // Keep the parseable oversized value in the future: rejecting it merely
    // because its date has passed would not exercise the runtime length cap.
    let second_format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]",
    )
    .expect("fixture second format");
    let future_second =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(now + 3_600_000) * 1_000_000)
            .expect("future fixture second")
            .format(&second_format)
            .expect("future UTC fields");
    let fractional = format!("{future_second}.123{}Z", "0".repeat(41));
    assert_eq!(fractional.encode_utf16().count(), 65);
    for oversized in [
        fractional,
        "x".repeat(65),
        "😀".repeat(33),
        "@everyone".repeat(9),
    ] {
        assert!(oversized.encode_utf16().count() > 64);
        cases.push((
            vec![
                option(
                    "body",
                    CommandOptionValue::String("private body".to_owned()),
                ),
                option("at", CommandOptionValue::String(oversized)),
            ],
            false,
        ));
    }
    cases
}

fn assert_refusals(mock: &MockRest, cases: &[(Vec<CommandDataOption>, bool)]) {
    let requests = mock.requests();
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(callbacks.len(), cases.len(), "one defer per refusal");
    assert_eq!(edits.len(), cases.len(), "one completion per refusal");
    assert_eq!(
        requests.len(),
        cases.len() * 2,
        "no channel or other REST effect"
    );
    for callback in callbacks {
        let reply: Value = serde_json::from_slice(&callback.body).expect("callback JSON");
        assert_eq!(reply["type"], 5);
        assert_eq!(reply["data"]["flags"], 64, "private defer");
    }
    for (edit, (_, duplicate)) in edits.iter().zip(cases) {
        let reply: Value = serde_json::from_slice(&edit.body).expect("completion JSON");
        assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
        let text = reply["content"].as_str().expect("refusal text");
        let lower = text.to_lowercase();
        assert!(lower.contains("at"), "identify the refused option: {text}");
        if *duplicate {
            assert!(lower.contains("duplicate"), "explain duplicate at: {text}");
        } else {
            assert!(
                ["64", "long", "rfc3339", "timestamp"]
                    .iter()
                    .any(|word| lower.contains(word)),
                "explain invalid or oversized at: {text}"
            );
        }
        assert!(
            !lower.contains("try again"),
            "not a transient store failure: {text}"
        );
        assert!(!text.contains("private body") && !text.contains("@everyone"));
        assert!(text.encode_utf16().count() <= 500, "bounded refusal");
    }
}

#[tokio::test]
async fn duplicate_and_oversized_at_refuse_before_any_store_access() {
    crate::gateway::ensure_crypto_provider();
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, false), true, origin);
    let cases = cases();
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule", Some(CHANNEL), options.clone()))
            .await;
    }
    assert_refusals(&mock, &cases);
    drop(runtime);
    mock.shutdown().await;
}

#[tokio::test]
async fn duplicate_and_oversized_at_leave_all_durable_state_unchanged() {
    crate::gateway::ensure_crypto_provider();
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("guarded disposable test bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated disposable database");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    let before = state(db.pool()).await;
    let cases = cases();
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule", Some(CHANNEL), options.clone()))
            .await;
    }
    let after = state(db.pool()).await;
    drop(runtime);
    db.close().await.expect("drop disposable database");
    assert_eq!(
        after, before,
        "refusals cannot create schedules, audits or commands"
    );
    assert_refusals(&mock, &cases);
    mock.shutdown().await;
}
