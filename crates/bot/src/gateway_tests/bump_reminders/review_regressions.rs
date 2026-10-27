#![cfg(test)]
//! Review regressions at the real command-runtime boundary, not reply helpers.
//! Breaks caught: automation-gated bump ingress, late ACKs, missing consumed-row
//! reset, weakened feature/role guards, and success/idle replies after I/O errors.

use super::*;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;

const OPT_IN_OK: &str = "You're opted in — you'll get one ping when the next bump cooldown ends.";
const OPT_OUT_OK: &str = "You're opted out — no more bump pings.";
const IDLE_OK: &str = "No bump reminder pending. Bump the server to start one.";
const ADD_ERROR: &str = "Adding the bump role didn't work right now — please try later.";
const REMOVE_ERROR: &str = "Removing the bump role didn't work right now — please try later.";
const STATUS_ERROR: &str = "I can't read the reminder state right now — please try later.";
const DISABLED: &str = "Bump reminders are off on this server.";
const ROLE_UNSET: &str = "The bump role isn't set up yet — ask a moderator to finish setup.";
// Isolate boot env from unrelated parallel tests, then serialize and restore
// every env value touched inside the fixture as well. Carry coverage output
// through env_clear so the real boot reader remains measurable in CI.
fn boot_intents_case(enabled: bool) {
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"));
    child
        .args([
            "--exact",
            "gateway_tests::bump_reminders::review_regressions::boot_intents_env_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("BUMP_INTENTS_CASE", if enabled { "1" } else { "0" });
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        child.env("LLVM_PROFILE_FILE", profile);
    }
    let output = child.output().expect("isolated boot intents");
    assert!(
        output.status.success(),
        "boot case bump={enabled}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[test]
fn bump_only_boot_requests_message_content() {
    boot_intents_case(true);
}

#[test]
fn disabled_bump_only_boot_does_not_request_message_content() {
    boot_intents_case(false);
}

#[tokio::test]
async fn boot_intents_env_fixture() {
    let Ok(enabled) = std::env::var("BUMP_INTENTS_CASE") else {
        return;
    };
    let _serial = BUMP_ENV_LOCK.lock().await;
    let vars = [
        ("GUILD_ID", GUILD),
        ("DISCORD_TOKEN", "bump-intents-synthetic-token"),
        ("TWO_TEXT_COMMANDS", "0"),
        ("TWO_AUTOMOD", "0"),
        // Tickets have no switch: empty ticket bindings are the off state.
        ("DISCORD_TICKET_CATEGORY_ID", ""),
        ("DISCORD_TICKET_STAFF_ROLE_ID", ""),
        ("DISCORD_TICKET_PANEL_CHANNEL_ID", ""),
        ("TWO_AUTOMATIONS", "0"),
        ("TWO_BUMP_REMINDERS", enabled.as_str()),
        ("TWO_BUMP_BOT_IDS", DISBOARD_ID),
        ("TWO_BUMP_ROLE_ID", ROLE_ID),
    ];
    let _restore = BumpEnv {
        saved: vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect(),
    };
    for (key, value) in vars {
        std::env::set_var(key, value);
    }
    let config = two_bot_core::Config::from_env().expect("boot config");
    let activation = crate::activation::BootActivation::from_config(&config);
    let intents = crate::gateway::intents_from_env(&activation);
    assert_eq!(
        intents.contains(twilight_gateway::Intents::MESSAGE_CONTENT),
        enabled == "1",
        "bump-only enablement must determine MESSAGE_CONTENT without unrelated features"
    );
}

const SLOW_IO: Duration = Duration::from_millis(3500);
const EARLY_ACK_BUDGET: Duration = Duration::from_millis(2500);
const DISCORD_ACK_DEADLINE: Duration = Duration::from_secs(3);

fn callbacks(mock: &MockRest, token: &str) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.ends_with(&format!("/{token}/callback")))
        .collect()
}

fn original_edits(mock: &MockRest, token: &str) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|r| {
            r.method == "PATCH"
                && r.path
                    .replace("%40", "@")
                    .ends_with(&format!("/webhooks/{APP_ID}/{token}/messages/@original"))
        })
        .collect()
}

fn role_writes(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|r| {
            (r.method == "PUT" || r.method == "DELETE")
                && r.path.ends_with(&format!(
                    "/guilds/{GUILD}/members/{MEMBER_ID}/roles/{ROLE_ID}"
                ))
        })
        .collect()
}

fn assert_safe_final(request: &RestRequest, expected: &str) {
    let body: Value = serde_json::from_slice(&request.body).expect("reply JSON");
    let data = if request.method == "PATCH" {
        &body
    } else {
        assert_eq!(
            body["type"], 4,
            "immediate final must be a message callback"
        );
        assert_eq!(body["data"]["flags"], 64, "immediate final stays private");
        &body["data"]
    };
    assert_eq!(data["content"], expected, "honest, exact final result");
    assert_eq!(data["allowed_mentions"]["parse"], json!([]));
    for key in ["roles", "users"] {
        assert!(
            data["allowed_mentions"]
                .get(key)
                .is_none_or(|v| v == &json!([])),
            "no explicit {key} mentions: {data:?}"
        );
    }
    assert_no_internals(expected, "final result");
    assert!(!expected.contains("@everyone") && !expected.contains("@here"));
    assert!(!expected.contains("<@"), "command replies never ping");
}

fn assert_one_final(mock: &MockRest, token: &str, expected: &str) {
    let finals: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| {
            if !r.path.contains(token) {
                return false;
            }
            let body: Value = serde_json::from_slice(&r.body).expect("reply JSON");
            (r.method == "POST" && r.path.ends_with("/callback") && body["type"] == 4)
                || (r.method == "PATCH"
                    && r.path.replace("%40", "@").ends_with("/messages/@original"))
        })
        .collect();
    assert_eq!(
        finals.len(),
        1,
        "one final result, not silence or duplicate replies"
    );
    assert_safe_final(&finals[0], expected);
    if expected == ADD_ERROR || expected == REMOVE_ERROR || expected == STATUS_ERROR {
        let text = String::from_utf8_lossy(&finals[0].body);
        for dishonest in [OPT_IN_OK, OPT_OUT_OK, IDLE_OK, "Bump reminder armed"] {
            assert!(
                !text.contains(dishonest),
                "failure must not claim success/idle"
            );
        }
    }
}

async fn wait_for_callback(mock: &MockRest, token: &str, budget: Duration) {
    let start = tokio::time::Instant::now();
    while callbacks(mock, token).is_empty() && start.elapsed() < budget {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn assert_deferred_lifecycle(
    mock: &MockRest,
    token: &str,
    started: tokio::time::Instant,
    early_callbacks: &[RestRequest],
    expected: &str,
) {
    assert_eq!(
        early_callbacks.len(),
        1,
        "one ACK must arrive while dependency I/O is still blocked, before three seconds"
    );
    let all = callbacks(mock, token);
    assert_eq!(all.len(), 1, "defer is the sole initial callback");
    let ack: Value = serde_json::from_slice(&all[0].body).expect("ACK JSON");
    assert_eq!(
        ack["type"], 5,
        "I/O-bearing command must defer, not answer late"
    );
    assert_eq!(ack["data"]["flags"], 64, "defer must be ephemeral");
    assert!(ack["data"].get("content").is_none());
    assert!(
        all[0].received_at.duration_since(started) < DISCORD_ACK_DEADLINE,
        "ACK must beat Discord's receipt-relative three-second deadline"
    );
    let edits = original_edits(mock, token);
    assert_eq!(
        edits.len(),
        1,
        "finish with exactly one original-response PATCH"
    );
    assert!(edits[0].received_at >= all[0].received_at);
    assert_one_final(mock, token, expected);
    assert_no_error_markers(expected, "slow successful command");
}

async fn automation_runtime(pool: PgPool, mock: &MockRest, enabled: bool) -> Arc<CommandRuntime> {
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).expect("executor");
    let mut router_gates = gates();
    router_gates.automations = enabled;
    let runtime = CommandRuntime::new(
        pool,
        executor,
        router_with_commands(router_gates),
        GUILD.parse().expect("guild"),
        enabled,
    );
    let vars = HashMap::from([
        (
            "TWO_AUTOMATIONS".to_owned(),
            if enabled { "1" } else { "0" }.to_owned(),
        ),
        ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
        ("TWO_AUTOMOD".to_owned(), "0".to_owned()),
    ]);
    runtime
        .initialize_custom_commands(
            GatewayCommandConfig::from_map_for_activation(2222, &vars, true).expect("config"),
        )
        .await
        .expect("real custom-command initialization");
    runtime
}

async fn seed_automation_controls(db: &TestDb) -> Value {
    use two_bot_core::custom_commands::PutCommandInput;
    use two_bot_core::sticky::{store, PutSticky};
    two_bot_core::custom_command_service::put(
        &db.pool,
        true,
        GUILD,
        MEMBER_ID,
        &PutCommandInput {
            name: "bumpfaq".to_owned(),
            description: "synthetic FAQ".to_owned(),
            template: "automation-only-faq".to_owned(),
            text_trigger: Some("!bumpfaq".to_owned()),
        },
        "seed-bump-faq",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("seed live custom trigger");
    store::put_sticky(
        &db.pool,
        &PutSticky {
            guild_id: GUILD,
            channel_id: CHANNEL_ID,
            body: "automation-only-sticky",
            debounce_seconds: 5,
            enabled: true,
            actor_id: MEMBER_ID,
            now_ms: two_bot_core::funnel::now_millis_for_test(),
        },
    )
    .await
    .expect("seed eligible sticky");
    sqlx::query("UPDATE sticky_messages SET last_message_id = '8999', last_posted_at = clock_timestamp() - INTERVAL '2 minutes'")
        .execute(&db.pool).await.expect("make sticky due");
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sticky_messages s")
        .fetch_one(&db.pool)
        .await
        .expect("sticky snapshot")
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn bump_dispatch_arms_and_sweeps_with_automations_off_without_other_effects() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let sticky_before = seed_automation_controls(&db).await;
    let audit_before: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE action LIKE 'command.text%' OR action LIKE 'sticky.%'")
        .fetch_one(&db.pool).await.expect("seed audit baseline");
    let mock = MockRest::with_responder(responder()).await;
    let runtime = automation_runtime(db.pool.clone(), &mock, false).await;
    let outcome = AssertUnwindSafe(async {
        // Exercise both an allowlisted bot success and a real human prefix
        // trigger. Enabling bump must not punch through the automation fence.
        dispatch_message(&runtime, &success_bump(9700), "automation-independent bump").await;
        tokio::time::sleep(ARM_SETTLE).await;
        armed_status(&runtime, &mock, 9701, "auto-off-armed", "automations off").await;
        let prefix = message_event(9702, human_author(), json!([]), "!bumpfaq", Some(GUILD));
        dispatch_message(&runtime, &prefix, "automation-fenced prefix and expiry sweep").await;
        let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
        assert_eq!(pings.len(), 1, "bump-only ingress also sweeps expiry");
        assert_ping_shape(&pings[0]);
        tokio::time::sleep(ARM_SETTLE).await;
        let sticky_after: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM sticky_messages s")
            .fetch_one(&db.pool).await.expect("sticky after dispatch");
        assert_eq!(sticky_before, sticky_after, "no sticky delivery or state mutation");
        let posts: Vec<_> = mock.requests().into_iter().filter(|r| r.method == "POST" && r.path.ends_with(&format!("/channels/{CHANNEL_ID}/messages"))).collect();
        assert_eq!(posts.len(), 1, "only bump ping, never sticky/custom reply");
        assert!(mock.requests().iter().all(|r| !(r.method == "DELETE" && r.path.ends_with("/messages/8999"))), "no sticky cleanup");
        let unrelated_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE action LIKE 'command.text%' OR action LIKE 'sticky.%'")
            .fetch_one(&db.pool).await.expect("automation audit fence");
        assert_eq!(unrelated_audits, audit_before, "dispatch adds no automation audits beyond fixture seeding");
    }).catch_unwind().await;
    drop(runtime);
    mock.shutdown().await;
    db.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn automation_fixture_really_posts_custom_and_sticky_when_enabled() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let before = seed_automation_controls(&db).await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = automation_runtime(db.pool.clone(), &mock, true).await;
    let outcome = AssertUnwindSafe(async {
        let prefix = message_event(9703, human_author(), json!([]), "!bumpfaq", Some(GUILD));
        dispatch_message(&runtime, &prefix, "eligible automation control").await;
        let start = tokio::time::Instant::now();
        loop {
            let contents: Vec<String> = mock
                .requests()
                .iter()
                .filter(|r| {
                    r.method == "POST"
                        && r.path
                            .ends_with(&format!("/channels/{CHANNEL_ID}/messages"))
                })
                .map(|r| {
                    serde_json::from_slice::<Value>(&r.body).expect("message JSON")["content"]
                        .as_str()
                        .expect("content")
                        .to_owned()
                })
                .collect();
            if contents.contains(&"automation-only-faq".to_owned())
                && contents.contains(&"automation-only-sticky".to_owned())
            {
                break;
            }
            assert!(
                start.elapsed() < FIRE_BUDGET,
                "both seeded automations must really run: {contents:?}"
            );
            tokio::time::sleep(POLL_TICK).await;
        }
        let after: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM sticky_messages s")
            .fetch_one(&db.pool)
            .await
            .expect("delivered sticky");
        assert_ne!(
            before, after,
            "positive control changes sticky delivery state"
        );
        assert!(
            bump_pings(&mock).is_empty(),
            "automation control does not fabricate bumps"
        );
    })
    .catch_unwind()
    .await;
    drop(runtime);
    mock.shutdown().await;
    db.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// A real pool/SQL read must remain blocked until after the early ACK check.
/// Holding the sole pool lease avoids sleeps inside SQL or a fake store.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn slow_status_read_defers_ephemerally_before_pool_io() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::with_pool_max(1).await;
    sqlx::query("INSERT INTO bump_reminders (guild_id, channel_id, fires_at, fired) VALUES ($1, $2, to_timestamp(1789000000), FALSE)")
        .bind(GUILD).bind(CHANNEL_ID).execute(&db.pool).await.expect("seed independently known status");
    let expected = "Bump reminder armed — pinging the bump role <t:1789000000:R>.";
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    let lease = db.pool.acquire().await.expect("hold sole test connection");
    let started = tokio::time::Instant::now();
    dispatch_command(
        &runtime,
        &bump_command(9300, "slow-status", "status"),
        "slow status",
    )
    .await;
    wait_for_callback(&mock, "slow-status", EARLY_ACK_BUDGET).await;
    let early = callbacks(&mock, "slow-status");
    let edits_while_blocked = original_edits(&mock, "slow-status");
    tokio::time::sleep_until(started + SLOW_IO).await;
    let released = tokio::time::Instant::now();
    drop(lease);
    let reply = await_reply_text(&mock, "slow-status", REPLY_BUDGET).await;
    drop(runtime);
    db.close().await;
    assert_eq!(reply.as_deref(), Some(expected));
    assert!(
        edits_while_blocked.is_empty(),
        "no final result before the DB read"
    );
    assert_deferred_lifecycle(&mock, "slow-status", started, &early, expected);
    assert!(
        original_edits(&mock, "slow-status")[0].received_at >= released,
        "status result must follow the real store read, not guess while the pool is held"
    );
    mock.shutdown().await;
}

async fn slow_role_case(subcommand: &str, method: &str, expected: &str, id: u64) {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let base = responder();
    let method_owned = method.to_owned();
    let mock = MockRest::with_responder(move |r| {
        if r.method == method_owned && r.path.ends_with(&format!("/roles/{ROLE_ID}")) {
            ScriptedResponse::status(204).delayed(SLOW_IO)
        } else {
            base(r)
        }
    })
    .await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    let token = format!("slow-{subcommand}");
    let started = tokio::time::Instant::now();
    dispatch_command(&runtime, &bump_command(id, &token, subcommand), subcommand).await;
    wait_for_callback(&mock, &token, EARLY_ACK_BUDGET).await;
    let early = callbacks(&mock, &token);
    let edits_while_blocked = original_edits(&mock, &token);
    let role_completed_early = mock
        .responses()
        .iter()
        .any(|r| r.path.ends_with(&format!("/roles/{ROLE_ID}")));
    let reply = await_reply_text(&mock, &token, REPLY_BUDGET).await;
    drop(runtime);
    db.close().await;
    assert_eq!(reply.as_deref(), Some(expected));
    assert!(
        !role_completed_early,
        "role response is still withheld at ACK observation"
    );
    assert!(
        edits_while_blocked.is_empty(),
        "no success before role completion"
    );
    let writes = role_writes(&mock);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, method);
    let completed = mock
        .responses()
        .into_iter()
        .find(|r| r.path.ends_with(&format!("/roles/{ROLE_ID}")))
        .expect("successful role response");
    assert_eq!(completed.status, 204);
    assert_deferred_lifecycle(&mock, &token, started, &early, expected);
    let edit = original_edits(&mock, &token).remove(0);
    assert!(
        edit.received_at.duration_since(writes[0].received_at) >= SLOW_IO,
        "final result only after slow REST finishes"
    );
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn slow_opt_in_defers_before_role_put() {
    slow_role_case("opt-in", "PUT", OPT_IN_OK, 9301).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn slow_opt_out_defers_before_role_delete() {
    slow_role_case("opt-out", "DELETE", OPT_OUT_OK, 9302).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn refused_defer_never_changes_role_or_edits_response() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let base = responder();
    let mock = MockRest::with_responder(move |r| {
        if r.method == "POST" && r.path.ends_with("/callback") {
            ScriptedResponse::json(403, json!({"message": "synthetic ACK refusal"}))
        } else {
            base(r)
        }
    })
    .await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    for (id, subcommand) in [(9303, "opt-in"), (9304, "opt-out")] {
        let token = format!("refused-{subcommand}");
        dispatch_command(&runtime, &bump_command(id, &token, subcommand), subcommand).await;
        wait_for_callback(&mock, &token, REPLY_BUDGET).await;
    }
    tokio::time::sleep(ARM_SETTLE).await;
    drop(runtime);
    db.close().await;
    assert!(
        role_writes(&mock).is_empty(),
        "a refused ACK cannot authorize role mutation"
    );
    for subcommand in ["opt-in", "opt-out"] {
        let token = format!("refused-{subcommand}");
        let acks = callbacks(&mock, &token);
        assert_eq!(acks.len(), 1, "no retried initial callback");
        let body: Value = serde_json::from_slice(&acks[0].body).expect("ACK JSON");
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
        assert!(original_edits(&mock, &token).is_empty());
    }
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn consumed_guild_can_arm_and_fire_a_second_cycle_without_duplicates() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    let outcome = AssertUnwindSafe(async {
        for cycle in 1..=2u64 {
            dispatch_message(&runtime, &success_bump(9400 + cycle), "fresh cycle success").await;
            tokio::time::sleep(ARM_SETTLE).await;
            let token = format!("cycle-{cycle}");
            armed_status(&runtime, &mock, 9410 + cycle, &token, "re-armed guild").await;
            tokio::time::sleep(POST_EXPIRY_SLEEP).await;
            dispatch_message(&runtime, &trigger_message(9420 + cycle), "cycle expiry").await;
            let pings = await_pings(&mock, cycle as usize, FIRE_BUDGET).await;
            assert_eq!(
                pings.len(),
                cycle as usize,
                "each fresh cycle earns one ping"
            );
            assert_ping_shape(&pings[cycle as usize - 1]);
            let fired: bool =
                sqlx::query_scalar("SELECT fired FROM bump_reminders WHERE guild_id = $1")
                    .bind(GUILD)
                    .fetch_one(&db.pool)
                    .await
                    .expect("consumed row");
            assert!(fired, "same guild row is consumed before the next upsert");
            dispatch_message(&runtime, &trigger_message(9430 + cycle), "duplicate sweep").await;
            tokio::time::sleep(ARM_SETTLE).await;
            assert_eq!(
                bump_pings(&mock).len(),
                cycle as usize,
                "repeat trigger never duplicates cycle {cycle}"
            );
        }
    })
    .catch_unwind()
    .await;
    drop(runtime);
    mock.shutdown().await;
    db.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Audit *attempted successful table writes*, including no-op UPDATEs, rather
/// than only checking the final row count (which could miss write-then-delete).
async fn install_write_probe(db: &TestDb) {
    sqlx::raw_sql(
        "CREATE TABLE bump_test_writes (operation TEXT NOT NULL);
        CREATE FUNCTION bump_test_record_write() RETURNS TRIGGER LANGUAGE plpgsql AS $$
        BEGIN INSERT INTO bump_test_writes VALUES (TG_OP); RETURN NULL; END $$;
        CREATE TRIGGER bump_test_write AFTER INSERT OR UPDATE OR DELETE ON bump_reminders
        FOR EACH STATEMENT EXECUTE FUNCTION bump_test_record_write();",
    )
    .execute(&db.pool)
    .await
    .expect("install schema-local write witness");
}

async fn refusal_case(enabled: bool) {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    if enabled {
        std::env::remove_var("TWO_BUMP_ROLE_ID");
    } else {
        std::env::set_var("TWO_BUMP_REMINDERS", "0");
    }
    let db = TestDb::new().await;
    // Existing due work must also stay untouched while the feature is off or
    // lacks a role. Seed before installing the write witness.
    sqlx::query("INSERT INTO bump_reminders (guild_id, channel_id, fires_at, fired) VALUES ($1, $2, now() - INTERVAL '1 minute', FALSE)")
        .bind(GUILD).bind(CHANNEL_ID).execute(&db.pool).await.expect("seed due test reminder");
    install_write_probe(&db).await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    // Rejected dispatch is legal here; lack of admission must not skip the
    // side-effect observations or the independent slash refusals below.
    runtime.dispatch_with_verdict(&success_bump(9500), Some(FunnelDisposition::Accept));
    tokio::time::sleep(ARM_SETTLE + POST_EXPIRY_SLEEP).await;
    runtime.dispatch_with_verdict(&trigger_message(9501), Some(FunnelDisposition::Accept));
    let commands = if enabled {
        vec!["opt-in", "opt-out"]
    } else {
        vec!["opt-in", "opt-out", "status"]
    };
    let mut replies = Vec::new();
    for (index, subcommand) in commands.iter().enumerate() {
        let token = format!("refusal-{subcommand}");
        dispatch_command(
            &runtime,
            &bump_command(9510 + index as u64, &token, subcommand),
            subcommand,
        )
        .await;
        replies.push((
            token.clone(),
            await_reply_text(&mock, &token, REPLY_BUDGET).await,
        ));
    }
    let pings = await_pings(&mock, 1, ABSENCE_BUDGET).await;
    let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM bump_test_writes")
        .fetch_one(&db.pool)
        .await
        .expect("write count");
    let still_pending: bool =
        sqlx::query_scalar("SELECT NOT fired FROM bump_reminders WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(&db.pool)
            .await
            .expect("retained row");
    drop(runtime);
    db.close().await;
    assert_eq!(writes, 0, "refused ingress/sweep has zero reminder writes");
    assert!(still_pending, "refusal does not consume existing work");
    assert!(pings.is_empty());
    assert!(role_writes(&mock).is_empty());
    for (token, reply) in replies {
        let expected = if enabled { ROLE_UNSET } else { DISABLED };
        assert_eq!(reply.as_deref(), Some(expected));
        assert_one_final(&mock, &token, expected);
        let acks = callbacks(&mock, &token);
        assert_eq!(acks.len(), 1, "refusals remain immediate");
        assert_safe_final(&acks[0], expected);
        assert!(original_edits(&mock, &token).is_empty());
    }
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn disabled_switch_with_retained_config_has_zero_effects_and_slash_refusal() {
    refusal_case(false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn enabled_without_role_has_zero_effects_and_actual_role_refusal() {
    refusal_case(true).await;
}

async fn role_error_case(subcommand: &str, method: &str, timeout: bool, expected: &str, id: u64) {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let base = responder();
    let method_owned = method.to_owned();
    let mock = MockRest::with_responder(move |r| {
        if r.method == method_owned && r.path.ends_with(&format!("/roles/{ROLE_ID}")) {
            if timeout {
                ScriptedResponse::status(204).delayed(Duration::from_secs(7))
            } else {
                ScriptedResponse::json(
                    403,
                    json!({"message": "synthetic forbidden @everyone sqlx"}),
                )
            }
        } else {
            base(r)
        }
    })
    .await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    let token = format!("failure-{subcommand}-{timeout}");
    dispatch_command(&runtime, &bump_command(id, &token, subcommand), subcommand).await;
    let reply = await_reply_text(&mock, &token, ROLE_BUDGET).await;
    let writes = role_writes(&mock);
    let completed = mock.responses();
    drop(runtime);
    db.close().await;
    assert_eq!(writes.len(), 1, "exactly one failed role request, no retry");
    assert_eq!(writes[0].method, method);
    if timeout {
        assert!(
            !completed.iter().any(|r| r.path == writes[0].path),
            "client reports timeout while role response is withheld"
        );
    } else {
        assert!(completed
            .iter()
            .any(|r| r.path == writes[0].path && r.status == 403));
    }
    assert_eq!(reply.as_deref(), Some(expected));
    assert_one_final(&mock, &token, expected);
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn forbidden_opt_in_reports_failure_not_success() {
    role_error_case("opt-in", "PUT", false, ADD_ERROR, 9600).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn forbidden_opt_out_reports_failure_not_success() {
    role_error_case("opt-out", "DELETE", false, REMOVE_ERROR, 9601).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn timed_out_opt_in_reports_failure_not_success() {
    role_error_case("opt-in", "PUT", true, ADD_ERROR, 9602).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn timed_out_opt_out_reports_failure_not_success() {
    role_error_case("opt-out", "DELETE", true, REMOVE_ERROR, 9603).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn failed_status_store_read_reports_failure_not_idle() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    // A closed real pool deterministically exercises the production store's
    // Err branch; no successful read is substituted by a fake.
    db.pool.close().await;
    dispatch_command(
        &runtime,
        &bump_command(9604, "failed-status", "status"),
        "failed status",
    )
    .await;
    let reply = await_reply_text(&mock, "failed-status", REPLY_BUDGET).await;
    drop(runtime);
    db.close().await;
    assert_eq!(reply.as_deref(), Some(STATUS_ERROR));
    assert_one_final(&mock, "failed-status", STATUS_ERROR);
    assert!(role_writes(&mock).is_empty());
    assert!(bump_pings(&mock).is_empty());
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn help_stays_immediate_and_ephemeral_without_dependency_io() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    db.pool.close().await;
    dispatch_command(
        &runtime,
        &bump_command(9605, "immediate-help", "help"),
        "help",
    )
    .await;
    let reply = await_reply_text(&mock, "immediate-help", REPLY_BUDGET)
        .await
        .expect("help text");
    drop(runtime);
    db.close().await;
    let acks = callbacks(&mock, "immediate-help");
    assert_eq!(acks.len(), 1);
    assert_safe_final(&acks[0], &reply);
    assert!(original_edits(&mock, "immediate-help").is_empty());
    assert!(
        reply.contains("/bump opt-in")
            && reply.contains("/bump opt-out")
            && reply.contains("/bump status")
    );
    assert!(reply.len() <= 1000);
    assert_no_error_markers(&reply, "immediate help");
    assert!(role_writes(&mock).is_empty());
    mock.shutdown().await;
}
