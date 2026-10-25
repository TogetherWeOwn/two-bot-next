//! Protected acceptance for correcting an unsent one-off's deadline.
//! Real router/store/ticker; only Discord is loopback. No new production API
//! is imported, so the baseline can fail on behavior rather than compilation.
//! Command time is wall time; delivery boundaries use the injected tick clock.

#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest as GatedRest, ScriptedResponse};
use futures_util::FutureExt;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};
use two_bot_core::scheduled_store::{
    begin_scheduled_delivery, claim_due, complete_run, put_scheduled, retry_scheduled,
    ScheduledWrite,
};
use two_bot_testsupport::TestDatabase;

#[path = "schedule_retime_refusal_tests.rs"]
mod refusals;

async fn database() -> TestDatabase {
    crate::gateway::ensure_crypto_provider();
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("guarded disposable bootstrap required; zero selected tests is not evidence");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("migrated disposable database")
}

fn future() -> u64 {
    let now = u64::try_from(now_millis_for_test()).expect("positive fixture clock");
    (now / 86_400_000 + 2) * 86_400_000 + 123
}

fn iso(ms: u64, offset: i32) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("fixture instant")
        .to_offset(UtcOffset::from_whole_seconds(offset).expect("fixture offset"))
        .format(&Rfc3339)
        .expect("explicit-offset fixture time")
}

fn canonical(ms: u64) -> String {
    let format = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
    )
    .expect("independent millisecond formatter");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .expect("fixture instant")
        .format(&format)
        .expect("canonical UTC fixture")
}

async fn seed(pool: &PgPool, id: &str, due: u64) {
    assert!(put_scheduled(
        pool,
        &ScheduledWrite {
            id: id.to_owned(),
            guild_id: GUILD_S.to_owned(),
            channel_id: CHANNEL_S.to_owned(),
            body: "retained private announcement".to_owned(),
            next_run_at: canonical(due),
            interval_seconds: None,
            enabled: true,
            created_by: "77".to_owned(),
            created_at: "2020-01-01T00:00:00.000Z".to_owned(),
            updated_by: "77".to_owned(),
            updated_at: "2021-01-01T00:00:00.000Z".to_owned(),
        }
    )
    .await
    .expect("seed eligible definition"));
}

async fn schedules(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id), '[]'::jsonb) FROM scheduled_messages s",
    )
    .fetch_one(pool).await.expect("capture all schedule columns")
}

async fn state(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'schedules', (SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id), '[]'::jsonb) FROM scheduled_messages s),
          'audits', (SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM automation_audit_log a))",
    )
    .fetch_one(pool).await.expect("capture schedules and audits")
}

fn retime(id: &str, at: &str) -> Interaction {
    slash(
        "schedule-retime",
        Some(CHANNEL),
        vec![
            option("id", CommandOptionValue::String(id.to_owned())),
            option("at", CommandOptionValue::String(at.to_owned())),
        ],
    )
}

async fn invoke(pool: &PgPool, interaction: Interaction) -> Vec<RestRequest> {
    invoke_gated(pool, interaction, true).await
}

async fn invoke_gated(pool: &PgPool, interaction: Interaction, enabled: bool) -> Vec<RestRequest> {
    crate::gateway::ensure_crypto_provider();
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(enabled, false)),
        GUILD,
        enabled,
    );
    runtime.on_interaction(&interaction).await;
    let requests = mock.requests();
    drop(runtime);
    mock.shutdown().await;
    requests
}

// Immediate private refusal and deferred private completion are equally safe.
// Do not freeze an incidental acknowledgement strategy or literal wording.
fn private_text(requests: &[RestRequest]) -> String {
    let mut acknowledged = false;
    let mut text = None;
    for request in requests {
        if request.method == "GET" {
            continue;
        }
        let body: Value = serde_json::from_slice(&request.body).expect("interaction response JSON");
        let payload = if request.method == "POST" && request.path.ends_with("/callback") {
            assert!(!acknowledged, "one acknowledgement only");
            acknowledged = true;
            assert_eq!(
                body["data"]["flags"].as_u64().unwrap_or(0) & 64,
                64,
                "private ack"
            );
            assert!(body["type"] == 4 || body["type"] == 5, "message or defer");
            &body["data"]
        } else {
            assert!(acknowledged, "acknowledge before completion");
            assert!(
                request.path.starts_with("/api/v10/webhooks/1111/"),
                "no public message mutation"
            );
            if request.method == "POST" {
                assert_eq!(
                    body["flags"].as_u64().unwrap_or(0) & 64,
                    64,
                    "private follow-up"
                );
            } else {
                assert_eq!(request.method, "PATCH");
                assert!(
                    request.path.ends_with("/messages/@original"),
                    "edit the private original only"
                );
            }
            &body
        };
        if let Some(content) = payload["content"].as_str() {
            assert!(text.is_none(), "one final receipt");
            assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
            assert!(content.encode_utf16().count() <= 2000, "bounded receipt");
            assert!(!content.contains("@everyone") && !content.contains("@here"));
            assert!(
                !content.contains("retained private announcement"),
                "do not echo stored body"
            );
            text = Some(content.to_owned());
        }
    }
    assert!(
        acknowledged,
        "the registered retime operation must answer privately"
    );
    text.expect("a completed private saved/not-saved receipt")
}

fn saved(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["saved", "retimed", "rescheduled", "updated", "moved"]
        .iter()
        .any(|word| lower.contains(word))
        && ![
            "not saved",
            "not retimed",
            "not rescheduled",
            "not updated",
            "not moved",
            "failed",
            "cannot",
            "could not",
        ]
        .iter()
        .any(|word| lower.contains(word))
}

fn assert_localized(text: &str, ms: u64) {
    assert!(
        ["f", "F", "R"]
            .iter()
            .any(|style| text.contains(&format!("<t:{}:{style}>", ms / 1000))),
        "localized deadline {ms}: {text}"
    );
}

fn unchanged_except_update(mut row: Value) -> Value {
    for key in ["next_run_at", "updated_by", "updated_at"] {
        row.as_object_mut().expect("schedule object").remove(key);
    }
    row
}

#[tokio::test]
async fn retime_changes_only_deadline_and_attributed_update_metadata() {
    let db = database().await;
    let old = future() + 86_400_000;
    let new = old - 86_400_000;
    seed(db.pool(), "stable-one-off", old).await;
    let before = state(db.pool()).await;
    let requests = invoke(db.pool(), retime("stable-one-off", &iso(new, 19_800))).await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(
        after["schedules"][0]["next_run_at"],
        canonical(new),
        "retime must persist the new deadline on the existing row"
    );
    assert_eq!(after["schedules"].as_array().expect("rows").len(), 1);
    assert_eq!(unchanged_except_update(after["schedules"][0].clone()), unchanged_except_update(before["schedules"][0].clone()),
        "identity, body, destination, creator, enabled state and every delivery fence are immutable");
    assert_eq!(after["schedules"][0]["updated_by"], "88");
    assert_ne!(
        after["schedules"][0]["updated_at"],
        before["schedules"][0]["updated_at"]
    );
    let audits = after["audits"].as_array().expect("audit array");
    assert_eq!(audits.len(), 1, "one attributed successful mutation");
    assert_eq!(audits[0]["actor_id"], "88");
    assert_eq!(audits[0]["target_key"], "stable-one-off");
    assert_eq!(audits[0]["guild_id"], GUILD_S);
    assert_eq!(audits[0]["outcome"], "ok");
    assert!(audits[0]["action"]
        .as_str()
        .expect("action")
        .starts_with("scheduled."));
    assert!(!audits[0]
        .to_string()
        .contains("retained private announcement"));
    let text = private_text(&requests);
    assert!(saved(&text), "explicit saved receipt: {text}");
    assert!(text.contains("stable-one-off"));
    assert_localized(&text, old);
    assert_localized(&text, new);
    // The saved receipt and zero public mutations prove configuration, not
    // delivery. A truthful "not delivered" disclaimer is also permitted.
}

#[tokio::test]
async fn old_deadline_never_posts_new_deadline_posts_once_even_after_restart() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    seed(db.pool(), "delivery-boundary", old).await;
    let requests = invoke(db.pool(), retime("delivery-boundary", &iso(new, 0))).await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let rest = executor_at(origin);
    let mut counts = Vec::new();
    for at in [old, new - 1, new] {
        crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || at)
            .await
            .expect("real scheduled tick");
        counts.push(
            mock.posts_to(&format!("/channels/{CHANNEL_S}/messages"))
                .await
                .len(),
        );
    }
    let peer = db.independent_pool().await.expect("restart pool");
    let (restart, restart_origin) = MockRest::start(Vec::new()).await;
    let restart_rest = executor_at(restart_origin);
    crate::scheduled_jobs::tick(&peer, &restart_rest, GUILD_S, || new + 120_000)
        .await
        .expect("restart tick");
    let after = schedules(&peer).await;
    let restart_posts = restart
        .posts_to(&format!("/channels/{CHANNEL_S}/messages"))
        .await
        .len();
    let public = mock
        .posts_to(&format!("/channels/{CHANNEL_S}/messages"))
        .await;
    peer.close().await;
    drop(rest);
    drop(restart_rest);
    mock.shutdown().await;
    restart.shutdown().await;
    db.close().await.expect("drop disposable database");

    assert_eq!(
        counts,
        vec![0, 0, 1],
        "no old/early occurrence, exactly one at the corrected deadline"
    );
    assert_eq!(restart_posts, 0);
    assert_eq!(after[0]["id"], "delivery-boundary");
    assert_eq!(after[0]["enabled"], false);
    assert_eq!(after[0]["last_run_at"], canonical(new));
    assert!(after[0]["last_message_id"].is_string());
    let payload: Value = serde_json::from_slice(&public[0].body).expect("public delivery JSON");
    assert!(payload["content"]
        .as_str()
        .expect("public body")
        .contains("retained private announcement"));
    assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    assert!(saved(&private_text(&requests)));
}

#[tokio::test]
async fn equivalent_offsets_and_repeated_requests_are_noops_including_audit_and_metadata() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    seed(db.pool(), "no-op-identity", old).await;
    let before = state(db.pool()).await;
    let equal = invoke(db.pool(), retime("no-op-identity", &iso(old, -25_200))).await;
    let after_equal = state(db.pool()).await;
    let first = invoke(db.pool(), retime("no-op-identity", &iso(new, 19_800))).await;
    let after_first = state(db.pool()).await;
    let mut repeats = Vec::new();
    for offset in [0, -25_200, 19_800] {
        repeats.push(invoke(db.pool(), retime("no-op-identity", &iso(new, offset))).await);
    }
    let after_repeats = state(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(after_equal, before, "same UTC instant writes nothing");
    assert_eq!(
        after_first["schedules"][0]["next_run_at"],
        canonical(new),
        "first changed deadline must actually save"
    );
    assert_eq!(
        after_repeats, after_first,
        "duplicates never rewrite metadata, allocate rows, audit or reset a fence"
    );
    assert!(saved(&private_text(&first)));
    for requests in std::iter::once(&equal).chain(repeats.iter()) {
        let text = private_text(requests);
        assert!(text.contains("no-op-identity"));
        assert!(
            ["unchanged", "already", "same", "no change"]
                .iter()
                .any(|word| text.to_lowercase().contains(word)),
            "explain idempotent no-op: {text}"
        );
    }
}

#[tokio::test]
async fn released_claim_snapshot_stays_stale_and_retime_preserves_occurrence_identity() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    seed(db.pool(), "stale-snapshot", old).await;
    let claimed = claim_due(
        db.pool(),
        GUILD_S,
        &canonical(old),
        "old-owner",
        &canonical(old + 60_000),
        "retained-nonce",
    )
    .await
    .expect("claim old due instant");
    assert_eq!(claimed.len(), 1, "baseline claim control");
    assert!(retry_scheduled(
        db.pool(),
        GUILD_S,
        "stale-snapshot",
        "old-owner",
        &canonical(old),
        true
    )
    .await
    .expect("release after proven no effect"));
    let before = schedules(db.pool()).await;
    let requests = invoke(db.pool(), retime("stale-snapshot", &iso(new, 0))).await;
    let after = schedules(db.pool()).await;
    let stale_can_send =
        begin_scheduled_delivery(db.pool(), GUILD_S, "stale-snapshot", "old-owner")
            .await
            .expect("old claim admission");
    let old_due = claim_due(
        db.pool(),
        GUILD_S,
        &canonical(old),
        "old-replay",
        &canonical(old + 60_000),
        "replacement-nonce",
    )
    .await
    .expect("old deadline admission");
    db.close().await.expect("drop disposable database");

    assert_eq!(after[0]["next_run_at"], canonical(new));
    assert_eq!(
        unchanged_except_update(after[0].clone()),
        unchanged_except_update(before[0].clone()),
        "retime is not definition replacement: retained nonce cannot be cleared"
    );
    assert_eq!(after[0]["occurrence_nonce"], "retained-nonce");
    assert!(
        !stale_can_send,
        "stale claim snapshot authorizes no old POST"
    );
    assert!(old_due.is_empty(), "old deadline cannot regain a claim");
    assert!(saved(&private_text(&requests)));
}

#[tokio::test]
async fn quota_full_retime_preserves_every_identity_and_uses_no_new_slot() {
    let db = database().await;
    let old = future();
    let new = old + 86_400_000;
    for index in 0..25 {
        seed(db.pool(), &format!("quota-{index:02}"), old).await;
    }
    let before = schedules(db.pool()).await;
    let requests = invoke(db.pool(), retime("quota-12", &iso(new, 0))).await;
    let after = schedules(db.pool()).await;
    let create = invoke(
        db.pool(),
        slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option(
                    "body",
                    CommandOptionValue::String("another body".to_owned()),
                ),
                option("at", CommandOptionValue::String(iso(new, 0))),
            ],
        ),
    )
    .await;
    let after_create = schedules(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(after.as_array().expect("rows").len(), 25);
    assert_eq!(
        after[12]["next_run_at"],
        canonical(new),
        "full capacity must not prevent an in-place retime"
    );
    for index in 0..25 {
        assert_eq!(
            unchanged_except_update(after[index].clone()),
            unchanged_except_update(before[index].clone())
        );
        if index != 12 {
            assert_eq!(
                after[index], before[index],
                "unrelated definition untouched"
            );
        }
    }
    assert_eq!(
        after_create, after,
        "retime did not release a capacity slot"
    );
    assert!(saved(&private_text(&requests)));
    let text = private_text(&create).to_lowercase();
    assert!(
        text.contains("25") || text.contains("limit") || text.contains("quota"),
        "unchanged capacity refusal: {text}"
    );
}

#[tokio::test]
async fn simultaneous_claim_and_retime_admission_have_exactly_one_winner() {
    let db = database().await;
    let old = future();
    let new = old + 86_400_000;
    let peer = db.independent_pool().await.expect("independent claimant");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    );
    let mut observations = Vec::new();
    for trial in 0..8 {
        let id = format!("contended-{trial}");
        let token = format!("claim-{trial}");
        seed(db.pool(), &id, old).await;
        let interaction = retime(&id, &iso(new, 0));
        let old_iso = canonical(old);
        let lease = canonical(old + 60_000);
        let start = mock.requests().len();
        let (_, claimed) = tokio::join!(
            runtime.on_interaction(&interaction),
            claim_due(&peer, GUILD_S, &old_iso, &token, &lease, "race-nonce")
        );
        let claimed = claimed.expect("contending due claim");
        let rows = schedules(db.pool()).await;
        let row = rows
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["id"] == id)
            .expect("same identity")
            .clone();
        let requests = mock.requests()[start..].to_vec();
        let stale_send = if claimed.is_empty() {
            begin_scheduled_delivery(&peer, GUILD_S, &id, &token)
                .await
                .expect("losing claim cannot send")
        } else {
            complete_run(&peer, GUILD_S, &id, &old_iso, None, &token)
                .await
                .expect("settle claim-winner fixture");
            false
        };
        observations.push((row, claimed.len(), requests, stale_send));
    }
    drop(runtime);
    mock.shutdown().await;
    peer.close().await;
    db.close().await.expect("drop disposable database");

    for (row, claimed, requests, stale_send) in observations {
        let retimed = row["next_run_at"] == canonical(new);
        assert!(claimed <= 1);
        assert_ne!(
            retimed,
            claimed == 1,
            "atomic retime/claim: exactly one wins, never neither or both"
        );
        assert!(!stale_send);
        let text = private_text(&requests);
        if retimed {
            assert!(saved(&text), "winning retime is acknowledged");
            assert!(row["claim_token"].is_null());
        } else {
            assert!(
                !saved(&text),
                "claim-winning refusal cannot claim a saved retime: {text}"
            );
            assert!(
                ["busy", "claim", "flight", "delivery"]
                    .iter()
                    .any(|word| text.to_lowercase().contains(word)),
                "actionable contention receipt: {text}"
            );
            assert_eq!(
                row["updated_by"], "77",
                "losing mutation cannot change attribution"
            );
            assert_eq!(row["occurrence_nonce"], "race-nonce");
        }
    }
}

#[tokio::test]
async fn in_flight_post_retains_its_fence_and_retime_cannot_create_a_second_delivery() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    seed(db.pool(), "in-flight", old).await;
    let (mock, gate) = GatedRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "9001"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = ActionExecutor::with_proxy_and_moderation_timeout(
        "test-token".to_owned(),
        Some(mock.origin()),
        Duration::from_secs(60),
    )
    .expect("bounded loopback executor");
    let pool = db.pool().clone();
    let mut task =
        tokio::spawn(
            async move { crate::scheduled_jobs::tick(&pool, &rest, GUILD_S, || old).await },
        );
    let observation =
        std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(30), async {
            gate.wait_for_request().await;
            let before = schedules(db.pool()).await;
            let requests = invoke(db.pool(), retime("in-flight", &iso(new, 0))).await;
            let after = schedules(db.pool()).await;
            (before, requests, after)
        }))
        .catch_unwind()
        .await;
    // Always release/join, including a fixture timeout or panic. Never detach
    // a ticker that still owns a claim or a blocked socket into teardown.
    gate.release();
    let tick_result = tokio::time::timeout(Duration::from_secs(10), &mut task).await;
    if tick_result.is_err() {
        task.abort();
        let _ = task.await;
    }
    let (restart, origin) = MockRest::start(Vec::new()).await;
    let rest = executor_at(origin);
    crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || new)
        .await
        .expect("no second occurrence");
    let completed = schedules(db.pool()).await;
    let posts = mock.requests();
    let replay = restart.requests();
    drop(rest);
    mock.shutdown().await;
    restart.shutdown().await;
    db.close().await.expect("drop disposable database");

    let (before, requests, after) = observation
        .expect("gated orchestration did not panic")
        .expect("gated orchestration completed within its bounded executor deadline");
    tick_result
        .expect("ticker joined after releasing the gate")
        .expect("scheduled task did not panic")
        .expect("confirmed original delivery settled");
    assert!(before[0]["claim_token"].is_string());
    assert!(before[0]["occurrence_nonce"].is_string());
    assert_eq!(
        before[0]["delivery_uncertain"], true,
        "fence commits before the POST"
    );
    assert_eq!(
        after, before,
        "claimed/in-flight retime must preserve every stored delivery fact"
    );
    assert_eq!(posts.len(), 1, "no orphan cleanup or replacement POST");
    assert!(replay.is_empty());
    assert_eq!(completed[0]["last_message_id"], "9001");
    assert_eq!(completed[0]["enabled"], false);
    let text = private_text(&requests).to_lowercase();
    assert!(!saved(&text));
    assert!(
        ["busy", "claim", "flight", "uncertain"]
            .iter()
            .any(|word| text.contains(word)),
        "actionable fence refusal: {text}"
    );
}

#[tokio::test]
async fn failed_success_audit_rolls_back_deadline_and_metadata_then_a_retry_can_save() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    seed(db.pool(), "audit-control", old).await;
    seed(db.pool(), "audit-rollback", old).await;
    let control = invoke(db.pool(), retime("audit-control", &iso(new, 0))).await;
    sqlx::query("CREATE FUNCTION refuse_retime_audit() RETURNS trigger AS $$ BEGIN IF NEW.target_key = 'audit-rollback' AND NEW.outcome = 'ok' THEN RAISE EXCEPTION 'synthetic audit refusal'; END IF; RETURN NEW; END; $$ LANGUAGE plpgsql")
        .execute(db.pool()).await.expect("install disposable audit failure");
    sqlx::query("CREATE TRIGGER refuse_retime_audit BEFORE INSERT ON automation_audit_log FOR EACH ROW EXECUTE FUNCTION refuse_retime_audit()")
        .execute(db.pool()).await.expect("attach disposable audit failure");
    let before = schedules(db.pool()).await;
    let refused = invoke(db.pool(), retime("audit-rollback", &iso(new, 0))).await;
    let after = state(db.pool()).await;
    sqlx::query("DROP TRIGGER refuse_retime_audit ON automation_audit_log")
        .execute(db.pool())
        .await
        .expect("remove disposable failure");
    let retried = invoke(db.pool(), retime("audit-rollback", &iso(new, 0))).await;
    let final_rows = schedules(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(
        before[0]["next_run_at"],
        canonical(new),
        "unfaulted retime is the positive control"
    );
    assert_eq!(
        after["schedules"], before,
        "audit failure must roll back the deadline and attributed metadata atomically"
    );
    assert!(!after["audits"]
        .as_array()
        .expect("audits")
        .iter()
        .any(|row| row["target_key"] == "audit-rollback" && row["outcome"] == "ok"));
    assert_eq!(
        final_rows[1]["next_run_at"],
        canonical(new),
        "removing the audit fault permits saving the same target"
    );
    assert!(saved(&private_text(&control)) && saved(&private_text(&retried)));
    let text = private_text(&refused).to_lowercase();
    assert!(!saved(&text), "failed audit is not a saved deadline");
    assert!(["not", "failed", "could not", "cannot", "unable"]
        .iter()
        .any(|word| text.contains(word)));
    assert!(!text.contains("synthetic") && !text.contains("sqlx") && !text.contains("postgres"));
}

#[test]
fn published_retime_surface_is_guild_management_only_with_bounded_required_time_and_id() {
    let definitions = two_bot_core::feature_commands::automation_commands();
    let definition = definitions
        .iter()
        .find(|command| command.name == "schedule-retime")
        .expect("publish the retime operation alongside the existing schedule commands");
    assert!(!definition.dm_permission);
    assert_eq!(definition.default_member_permissions.as_deref(), Some("32"));
    for (name, bound) in [("id", 128), ("at", 64)] {
        let option = definition
            .options
            .iter()
            .find(|option| option.name == name)
            .expect("required option");
        assert_eq!(option.kind, 3);
        assert_eq!(option.required, Some(true));
        assert_eq!(option.max_length, Some(bound));
    }
    assert!(
        !definition.examples.is_empty(),
        "help must explain explicit-offset usage"
    );
}

#[tokio::test]
async fn valid_id_time_length_and_future_horizon_boundaries_still_save() {
    let db = database().await;
    let old = future();
    let new = old + 3_600_000;
    let id = "x".repeat(128);
    seed(db.pool(), &id, old).await;
    let second = iso(new, 0)
        .split('.')
        .next()
        .expect("UTC seconds")
        .to_owned();
    let at = format!("{second}.123{}Z", "0".repeat(40));
    assert_eq!(
        at.len(),
        64,
        "valid input at the existing time-length bound"
    );
    let length_requests = invoke(db.pool(), retime(&id, &at)).await;
    let after_length = schedules(db.pool()).await;
    let now = u64::try_from(now_millis_for_test()).expect("positive clock");
    // Commands have no injectable wall clock: stay two minutes inside the
    // existing 365-day ceiling, rather than claiming an exact-now boundary.
    let horizon = now + 365 * 86_400_000 - 120_000;
    let horizon_requests = invoke(db.pool(), retime(&id, &iso(horizon, -25_200))).await;
    let after_horizon = schedules(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(after_length[0]["id"], id);
    assert_eq!(after_length[0]["next_run_at"], canonical(new));
    assert_eq!(after_horizon[0]["next_run_at"], canonical(horizon));
    assert!(saved(&private_text(&length_requests)));
    assert!(saved(&private_text(&horizon_requests)));
}

#[test]
fn control_existing_explicit_time_contract_has_exact_fixed_clock_bounds() {
    use two_bot_core::scheduled::validate_schedule_at;
    let now = 1_800_000_000_123;
    let horizon = now + 365 * 86_400_000;
    assert!(validate_schedule_at(&canonical(now), now).is_err());
    assert_eq!(
        validate_schedule_at(&canonical(now + 1), now).ok(),
        Some(now + 1)
    );
    assert_eq!(
        validate_schedule_at(&iso(horizon, 19_800), now).ok(),
        Some(horizon)
    );
    assert!(validate_schedule_at(&canonical(horizon + 1), now).is_err());
    let same_millisecond = canonical(now).replace(".123Z", ".123999Z");
    assert!(
        validate_schedule_at(&same_millisecond, now).is_err(),
        "fractional sub-millisecond future is not a future persisted deadline"
    );
}

#[tokio::test]
async fn control_absolute_creation_and_cancellation_still_use_the_existing_surface() {
    let db = database().await;
    let due = future();
    let created = invoke(
        db.pool(),
        slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option(
                    "body",
                    CommandOptionValue::String("unchanged creation body".to_owned()),
                ),
                option("at", CommandOptionValue::String(iso(due, 19_800))),
            ],
        ),
    )
    .await;
    let before_remove = schedules(db.pool()).await;
    let id = before_remove[0]["id"]
        .as_str()
        .expect("existing create control returns a stored id")
        .to_owned();
    let removed = invoke(
        db.pool(),
        slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String(id.clone()))],
        ),
    )
    .await;
    let after_remove = schedules(db.pool()).await;
    db.close().await.expect("drop disposable database");

    assert_eq!(before_remove.as_array().expect("rows").len(), 1);
    assert_eq!(before_remove[0]["next_run_at"], canonical(due));
    assert_eq!(before_remove[0]["body"], "unchanged creation body");
    assert_eq!(before_remove[0]["channel_id"], CHANNEL_S);
    assert!(private_text(&created).contains(&id));
    assert!(private_text(&removed).contains(&id));
    assert_eq!(after_remove, json!([]));
}
