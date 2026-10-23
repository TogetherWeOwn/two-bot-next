//! Protected acceptance for an explicit RFC3339 due instant. Only test wiring
//! may change in the builder's workspace; consume this file read-only.
//!
//! Breaks caught: ignoring `at`, interpreting an offset as local/UTC wall time,
//! silently preferring a conflicting flag, accepting malformed/out-of-horizon
//! time, bypassing existing authority/quota/body safety, losing the due instant
//! on restart/backup, and repeating a completed or uncertain occurrence.
//!
//! Commands currently read the wall clock, so their relative controls bracket
//! that read. The ticker clock is injected: due-1, due and restart are exact.
//! Expected timestamps use the independent `time` formatter, not the scheduler's
//! parser/formatter. Discord is loopback only; persistence is a migrated,
//! disposable TestDatabase, never application, staging or production storage.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};
use two_bot_core::{
    backup::dump::{dump, restore},
    scheduled_store::{get_scheduled, list_scheduled, put_scheduled, ScheduledWrite},
};
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    crate::gateway::ensure_crypto_provider();
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("set the guarded disposable test bootstrap; these tests must execute");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated disposable database")
}

fn iso(ms: u64, offset: i32) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("fixture instant")
        .to_offset(UtcOffset::from_whole_seconds(offset).expect("fixture offset"))
        .format(&Rfc3339)
        .expect("RFC3339 fixture")
}

fn canonical(ms: u64) -> String {
    let format = time::format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
    )
    .expect("independent UTC millisecond format");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("fixture instant")
        .format(&format)
        .expect("canonical fixture")
}

fn future() -> u64 {
    // Midnight two days ahead, plus a fractional second. The negative offset
    // crosses the calendar date, catching wall-clock-only normalization.
    (now_millis_for_test() / 86_400_000 + 2) * 86_400_000 + 123
}

fn options(
    body: &str,
    at: Option<&str>,
    relative: Option<i64>,
    every: Option<i64>,
) -> Vec<CommandDataOption> {
    let mut values = vec![option("body", CommandOptionValue::String(body.to_owned()))];
    if let Some(at) = at {
        values.push(option("at", CommandOptionValue::String(at.to_owned())));
    }
    if let Some(minutes) = relative {
        values.push(option("in-minutes", CommandOptionValue::Integer(minutes)));
    }
    if let Some(minutes) = every {
        values.push(option(
            "every-minutes",
            CommandOptionValue::Integer(minutes),
        ));
    }
    values
}

async fn command(pool: &PgPool, name: &str, options: Vec<CommandDataOption>) -> Value {
    crate::gateway::ensure_crypto_provider();
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    runtime
        .on_interaction(&slash(name, Some(CHANNEL), options))
        .await;
    let reply = mock.deferred_reply();
    assert_eq!(
        mock.requests().len(),
        2,
        "defer + mention-safe completion only; no channel effect"
    );
    drop(runtime);
    mock.shutdown().await;
    reply
}

async fn rows(pool: &PgPool) -> Vec<two_bot_core::scheduled_store::ScheduledMessageRow> {
    list_scheduled(pool, GUILD_S)
        .await
        .expect("list actual scheduled rows")
}

async fn state(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'schedules', (SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id), '[]'::jsonb) FROM scheduled_messages s),
          'audits', (SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM automation_audit_log a))",
    )
    .fetch_one(pool).await.expect("capture all schedule/audit effects")
}

fn content(reply: &Value) -> &str {
    reply["content"]
        .as_str()
        .expect("human-readable completion")
}

fn due_reply(reply: &Value, id: &str, due: u64) {
    let text = content(reply);
    assert!(text.contains(id), "stable removable id: {text}");
    assert!(
        text.contains(&canonical(due)),
        "canonical persisted due instant: {text}"
    );
    assert!(
        [
            format!("<t:{}:F>", due / 1000),
            format!("<t:{}:f>", due / 1000)
        ]
        .iter()
        .any(|timestamp| text.contains(timestamp)),
        "Discord-localized absolute due time: {text}",
    );
    let lower = text.to_lowercase();
    assert!(
        ["ticker", "delivery", "tick"]
            .iter()
            .any(|word| lower.contains(word))
            && [
                "may",
                "approx",
                "not exact",
                "not guarantee",
                "best-effort",
                "best effort"
            ]
            .iter()
            .any(|word| lower.contains(word)),
        "explain delivery uncertainty, never promise exact-second delivery: {text}",
    );
}

#[tokio::test]
async fn equivalent_explicit_offsets_persist_the_same_utc_millisecond_instant() {
    let db = database().await;
    let due = future();
    let mut replies = Vec::new();
    for offset in [0, 19_800, -25_200] {
        replies.push(
            command(
                db.pool(),
                "schedule",
                options("offset meeting", Some(&iso(due, offset)), None, None),
            )
            .await,
        );
    }
    let stored = rows(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(stored.len(), 3, "each at-only command creates a one-shot");
    for row in &stored {
        assert_eq!(row.next_run_at, canonical(due));
        assert_eq!(row.interval_seconds, None);
        assert!(row.enabled);
        assert_eq!(row.channel_id, CHANNEL_S);
        assert_eq!(row.body, "offset meeting");
        assert!(!row.delivery_uncertain);
        assert!(replies.iter().any(|reply| content(reply).contains(&row.id)));
    }
    let ids: std::collections::BTreeSet<_> = stored.iter().map(|row| &row.id).collect();
    assert_eq!(
        ids.len(),
        3,
        "normalization must not conflate distinct schedule identities"
    );
}

#[tokio::test]
async fn list_and_remove_expose_the_canonical_due_time_and_stable_id() {
    let db = database().await;
    let due = future();
    let set = command(
        db.pool(),
        "schedule",
        options("meeting", Some(&iso(due, 19_800)), None, None),
    )
    .await;
    let stored = rows(db.pool()).await;
    if stored.len() != 1 {
        db.close().await.expect("drop disposable database");
        assert_eq!(
            stored.len(),
            1,
            "at-only set must persist before it can be listed"
        );
        return;
    }
    let id = stored[0].id.clone();
    let listed = command(db.pool(), "schedule-list", Vec::new()).await;
    let removed = command(
        db.pool(),
        "schedule-remove",
        vec![option("id", CommandOptionValue::String(id.clone()))],
    )
    .await;
    let remaining = rows(db.pool()).await;
    db.close().await.expect("drop disposable database");
    due_reply(&set, &id, due);
    due_reply(&listed, &id, due);
    assert!(content(&removed).contains(&id));
    assert!(
        remaining.is_empty(),
        "the listed stable id remains removable"
    );
}

async fn refuse_time(
    at: CommandOptionValue,
    relative: Option<i64>,
    every: Option<i64>,
    guidance: &[&str],
) {
    let db = database().await;
    let before = state(db.pool()).await;
    let mut values = options("private schedule text", None, relative, every);
    values.push(option("at", at));
    let reply = command(db.pool(), "schedule", values).await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(
        after, before,
        "time refusal precedes ALL store effects, including rejection audits"
    );
    let text = content(&reply).to_lowercase();
    assert!(text.contains("at"), "identify the invalid option: {text}");
    assert!(
        guidance.iter().any(|word| text.contains(word)),
        "actionable time refusal: {text}"
    );
    assert!(
        !text.contains("private schedule text"),
        "do not echo the body in an error"
    );
    assert!(
        !text.contains("try again"),
        "validation is not a transient store error"
    );
}

#[tokio::test]
async fn absolute_and_relative_conflict_refuses_without_any_store_or_channel_effect() {
    refuse_time(
        CommandOptionValue::String(iso(future(), 0)),
        Some(5),
        None,
        &["one", "either", "conflict", "combine"],
    )
    .await;
}

#[tokio::test]
async fn absolute_and_recurrence_conflict_refuses_without_any_store_or_channel_effect() {
    refuse_time(
        CommandOptionValue::String(iso(future(), 0)),
        None,
        Some(60),
        &["one-off", "one-shot", "recurr", "every-minutes"],
    )
    .await;
}

#[tokio::test]
async fn malformed_offset_free_past_and_overflow_times_refuse_before_storage() {
    for (at, guidance) in [
        (
            "not-a-time @everyone",
            vec!["rfc3339", "offset", "timestamp"],
        ),
        ("2026-10-24T12:30:00", vec!["offset", "utc", "rfc3339"]),
        (
            "2026-02-30T12:30:00Z",
            vec!["rfc3339", "invalid", "timestamp"],
        ),
        (
            "2026-10-24T12:30:00+25:00",
            vec!["rfc3339", "invalid", "offset"],
        ),
        ("1970-01-01T00:00:00Z", vec!["future", "past"]),
        (
            "9999-12-31T23:59:59Z",
            vec!["horizon", "525600", "year", "range", "365"],
        ),
        (
            "+999999999999-01-01T00:00:00Z",
            vec!["rfc3339", "invalid", "range", "timestamp"],
        ),
        ("", vec!["rfc3339", "offset", "timestamp"]),
    ] {
        refuse_time(
            CommandOptionValue::String(at.to_owned()),
            None,
            None,
            &guidance,
        )
        .await;
    }
}

#[tokio::test]
async fn wrong_type_at_is_not_ignored_when_valid_relative_timing_is_present() {
    refuse_time(
        CommandOptionValue::Integer(123),
        Some(5),
        None,
        &["string", "rfc3339", "timestamp", "conflict", "either"],
    )
    .await;
}

#[tokio::test]
async fn out_of_horizon_at_cannot_fall_back_to_a_valid_relative_time() {
    refuse_time(
        CommandOptionValue::String(iso(now_millis_for_test() + 31_536_000_000 + 86_400_000, 0)),
        Some(5),
        None,
        &["one", "either", "conflict", "horizon", "year", "range"],
    )
    .await;
}

#[tokio::test]
async fn absolute_horizon_accepts_the_existing_year_limit_but_refuses_a_day_beyond() {
    let db = database().await;
    let due = now_millis_for_test() + 31_536_000_000;
    command(
        db.pool(),
        "schedule",
        options("horizon boundary", Some(&iso(due, 0)), None, None),
    )
    .await;
    let stored = rows(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(
        stored.len(),
        1,
        "the full existing relative horizon is available to at"
    );
    assert_eq!(stored[0].next_run_at, canonical(due));
    refuse_time(
        CommandOptionValue::String(iso(now_millis_for_test() + 31_536_000_000 + 86_400_000, 0)),
        None,
        None,
        &["horizon", "525600", "year", "range", "365"],
    )
    .await;
}

#[tokio::test]
async fn relative_and_interval_controls_retain_their_existing_first_run_semantics() {
    let db = database().await;
    for (relative, every, delay, interval) in [
        (Some(1), None, 60_000, None),
        (Some(525_600), None, 31_536_000_000, None),
        (None, Some(90), 5_400_000, Some(5_400)),
        (Some(5), Some(90), 300_000, Some(5_400)),
    ] {
        let before = now_millis_for_test();
        let reply = command(
            db.pool(),
            "schedule",
            options("relative control", None, relative, every),
        )
        .await;
        let after = now_millis_for_test();
        let stored = rows(db.pool()).await;
        let row = stored
            .iter()
            .find(|row| content(&reply).contains(&row.id))
            .expect("created control id in confirmation");
        let due = OffsetDateTime::parse(&row.next_run_at, &Rfc3339)
            .expect("stored due time")
            .unix_timestamp_nanos()
            / 1_000_000;
        assert!(
            due >= i128::from(before + delay) && due <= i128::from(after + delay),
            "relative due time stays now + delay"
        );
        assert_eq!(row.interval_seconds, interval);
    }
    db.close().await.expect("drop disposable database");
}

#[tokio::test]
async fn permission_guild_and_disabled_gate_controls_still_precede_at_storage() {
    let db = database().await;
    for (enabled, authority, guild, want) in [
        (false, true, GUILD, Some(AUTOMATIONS_DISABLED_REPLY)),
        (true, false, GUILD, Some(MANAGE_SERVER_REQUIRED)),
        (true, true, GUILD + 1, None),
    ] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = CommandRuntime::new(
            db.pool().clone(),
            executor_at(origin),
            router_with_commands(gates(enabled, false)),
            GUILD,
            enabled,
        );
        let mut interaction = slash(
            "schedule",
            Some(CHANNEL),
            options("guarded", Some(&iso(future(), 0)), None, None),
        );
        interaction.guild_id = Some(Id::new(guild));
        if !authority {
            interaction.member.as_mut().expect("member").permissions = Some(Permissions::empty());
        }
        runtime.on_interaction(&interaction).await;
        let requests = mock.requests();
        if let Some(want) = want {
            assert_eq!(
                requests.len(),
                1,
                "one refusal, no defer/store/channel effects"
            );
            let reply: Value = serde_json::from_slice(&requests[0].body).expect("refusal callback");
            assert_eq!(reply["type"], 4);
            assert_eq!(reply["data"]["flags"], 64);
            assert_eq!(reply["data"]["content"], want);
        } else {
            assert!(
                requests.is_empty(),
                "other-guild interactions remain silent"
            );
        }
        drop(runtime);
        mock.shutdown().await;
    }
    let snapshot = state(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(snapshot, json!({"schedules": [], "audits": []}));
}

#[tokio::test]
async fn absolute_time_retains_body_bounds_and_safe_outbound_mentions() {
    let db = database().await;
    let at = iso(future(), 0);
    for body in [" ".to_owned(), "x".repeat(2_001), "😀".repeat(1_001)] {
        let reply = command(db.pool(), "schedule", options(&body, Some(&at), None, None)).await;
        assert!(
            content(&reply).contains("body"),
            "body bound still explains refusal"
        );
        assert!(rows(db.pool()).await.is_empty());
    }
    let reply = command(
        db.pool(),
        "schedule",
        options(&"x".repeat(2_000), Some(&at), None, None),
    )
    .await;
    let stored = rows(db.pool()).await;
    assert_eq!(
        stored.len(),
        1,
        "exact body ceiling is still accepted with at"
    );
    assert!(content(&reply).contains(&stored[0].id));
    command(
        db.pool(),
        "schedule",
        options(
            "meet @everyone @here discord.gg/fixture",
            Some(&at),
            None,
            None,
        ),
    )
    .await;
    let stored = rows(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(stored.len(), 2);
    let safe = stored
        .iter()
        .find(|row| row.body.starts_with("meet"))
        .expect("mention-safe definition");
    assert_eq!(
        safe.body,
        "meet @\u{200b}everyone @\u{200b}here discord.\u{200b}gg/fixture"
    );
}

#[tokio::test]
async fn absolute_time_does_not_bypass_the_existing_twenty_five_schedule_quota() {
    let db = database().await;
    let due = future();
    for index in 0..25 {
        let stamp = canonical(due);
        assert!(put_scheduled(
            db.pool(),
            &ScheduledWrite {
                id: format!("quota-{index}"),
                guild_id: GUILD_S.to_owned(),
                channel_id: CHANNEL_S.to_owned(),
                body: "quota control".to_owned(),
                next_run_at: stamp.clone(),
                interval_seconds: None,
                enabled: true,
                created_by: "88".to_owned(),
                created_at: stamp.clone(),
                updated_by: "88".to_owned(),
                updated_at: stamp,
            }
        )
        .await
        .expect("seed quota through actual store"));
    }
    let before = rows(db.pool()).await;
    let reply = command(
        db.pool(),
        "schedule",
        options("overflow", Some(&iso(due, 0)), None, None),
    )
    .await;
    let after = rows(db.pool()).await;
    db.close().await.expect("drop disposable database");
    assert_eq!(before.len(), 25);
    assert_eq!(after.len(), 25);
    assert!(after.iter().all(|row| row.body == "quota control"));
    assert!(
        content(&reply).contains("25") && content(&reply).contains("/schedule-remove"),
        "actionable existing quota refusal: {}",
        content(&reply)
    );
}

fn channel_posts(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .iter()
        .filter(|request| {
            request.method == "POST" && request.path == "/api/v10/channels/3333/messages"
        })
        .map(|request| serde_json::from_slice(&request.body).expect("outbound message JSON"))
        .collect()
}

#[tokio::test]
async fn fresh_runtime_reads_the_persisted_due_instant_and_delivers_once_at_the_boundary() {
    let db = database().await;
    let due = future();
    command(
        db.pool(),
        "schedule",
        options("meet @everyone", Some(&iso(due, -25_200)), None, None),
    )
    .await;
    let stored = rows(db.pool()).await;
    assert_eq!(
        stored.len(),
        1,
        "absolute schedule must exist before restart"
    );
    let id = &stored[0].id;
    let restarted = db.independent_pool().await.expect("new runtime pool");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = executor_at(origin.clone());
    crate::scheduled_jobs::tick(&restarted, &executor, GUILD_S, || due - 1)
        .await
        .expect("before due");
    assert!(
        channel_posts(&mock).is_empty(),
        "never deliver a millisecond early"
    );
    let before = get_scheduled(&restarted, GUILD_S, id)
        .await
        .expect("read")
        .expect("persisted definition");
    assert_eq!(
        before.next_run_at,
        canonical(due),
        "restart does not recompute now + delay"
    );
    crate::scheduled_jobs::tick(&restarted, &executor, GUILD_S, || due)
        .await
        .expect("at due");
    let done = get_scheduled(&restarted, GUILD_S, id)
        .await
        .expect("read completion")
        .expect("retained disabled one-shot");
    restarted.close().await;
    drop(executor);
    let again = db.independent_pool().await.expect("second runtime pool");
    let executor = executor_at(origin);
    crate::scheduled_jobs::tick(&again, &executor, GUILD_S, || due + 3_600_000)
        .await
        .expect("redelivered tick after restart");
    let sent = channel_posts(&mock);
    again.close().await;
    mock.shutdown().await;
    db.close().await.expect("drop disposable database");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["content"], "meet @\u{200b}everyone");
    assert_eq!(
        sent[0]["allowed_mentions"],
        json!({"parse": [], "roles": [], "users": [], "replied_user": false})
    );
    assert_eq!(sent[0]["enforce_nonce"], true);
    assert_eq!(sent[0]["nonce"].as_str().map(str::len), Some(24));
    assert!(!done.enabled);
    assert_eq!(done.last_run_at.as_deref(), Some(canonical(due).as_str()));
    assert!(done.last_message_id.is_some());
}

#[tokio::test]
async fn backup_and_restart_preserve_an_uncertain_absolute_occurrence_without_reposting() {
    let db = database().await;
    let due = future();
    command(
        db.pool(),
        "schedule",
        options("backup meeting", Some(&iso(due, 19_800)), None, None),
    )
    .await;
    let stored = rows(db.pool()).await;
    assert_eq!(
        stored.len(),
        1,
        "absolute schedule must exist before delivery"
    );
    let id = &stored[0].id;
    let (mock, origin) = MockRest::start_script(vec![RestResponse {
        status: 200,
        body: Some("{}".to_owned()),
        delay: Duration::ZERO,
    }])
    .await;
    let executor = executor_at(origin.clone());
    crate::scheduled_jobs::tick(db.pool(), &executor, GUILD_S, || due)
        .await
        .expect("uncertain receipt is held");
    let pending = get_scheduled(db.pool(), GUILD_S, id)
        .await
        .expect("read pending")
        .expect("retained schedule");
    let sent = channel_posts(&mock);
    assert_eq!(
        sent.len(),
        1,
        "remote accepted one POST but omitted a usable receipt"
    );
    assert!(pending.delivery_uncertain && pending.enabled);
    assert_eq!(
        pending.occurrence_nonce.as_deref(),
        sent[0]["nonce"].as_str()
    );
    assert!(pending.last_run_at.is_none());
    let base = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!(
        "absolute-schedule-{}-{}.jsonl.gz",
        std::process::id(),
        new_id()
    ));
    dump(db.pool(), &path)
        .await
        .expect("real complete-schema backup");
    let restored = database().await;
    let report = restore(restored.pool(), &path)
        .await
        .expect("restore into fresh disposable database");
    assert!(report.ok);
    let recovered = get_scheduled(restored.pool(), GUILD_S, id)
        .await
        .expect("read restored occurrence")
        .expect("backup preserves schedule");
    assert_eq!(recovered.next_run_at, pending.next_run_at);
    assert_eq!(recovered.occurrence_nonce, pending.occurrence_nonce);
    assert!(recovered.delivery_uncertain);
    let restart = restored
        .independent_pool()
        .await
        .expect("fresh runtime after backup");
    let executor = executor_at(origin);
    for now in [due + 3_600_000, due + 7_200_000] {
        crate::scheduled_jobs::tick(&restart, &executor, GUILD_S, || now)
            .await
            .expect("restart holds uncertainty beyond nonce dedup window");
    }
    let after = get_scheduled(&restart, GUILD_S, id)
        .await
        .expect("read held occurrence")
        .expect("retained occurrence");
    let sends = channel_posts(&mock);
    restart.close().await;
    mock.shutdown().await;
    restored.close().await.expect("drop restored database");
    db.close().await.expect("drop source database");
    std::fs::remove_file(&path).expect("remove owned synthetic backup");
    assert_eq!(
        sends.len(),
        1,
        "neither backup nor expired leases authorize a second POST"
    );
    assert!(after.delivery_uncertain && after.enabled);
    assert_eq!(after.occurrence_nonce, pending.occurrence_nonce);
    assert!(after.last_run_at.is_none());
}
