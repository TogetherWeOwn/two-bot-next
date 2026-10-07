#![cfg(test)]
//! F2 boot-registry ownership + F5 mixed RSVP/custom contention, governed.
//!
//! Real shard + real runtimes; loopback REST and the parent's guarded,
//! isolated TestDb only. No live Discord, no production/staging stores.
//!
//! F2: a seeded custom slash row survives the boot replacement on both the
//! fresh READY and the persisted-session RESUMED acceptance path: the
//! actual PUT body and the final registry contain it exactly once, and the
//! boot emits exactly one PUT (READY/RESUMED republishes nothing).
//!
//! F5: an RSVP lookup holds the shared durable lane briefly while a custom
//! slash command is received. The custom defer retries provably pre-wire
//! Blocked contention inside the receipt-relative budget: exactly one
//! timely callback reaches the wire, one custom effect commits, and one
//! final edit lands, while the RSVP side still executes once.
use super::*;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use two_bot_core::{
    send_admission::{PgSendAdmission, SendAdmission},
    ClassifierConfig, InteractionRouter, RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};

use crate::discord_test_common::{MockRest, ScriptedResponse};
use crate::gateway_commands::GatewayCommandConfig;

const EVENT: &str = "1546451670500642999";

fn vars() -> HashMap<String, String> {
    HashMap::from([
        ("TWO_AUTOMATIONS".into(), "1".into()),
        ("TWO_TEXT_COMMANDS".into(), "1".into()),
    ])
}

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        announcements: true,
        scorecard: true,
        automations: true,
        moderation: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

/// Route-aware mock: identity/guild reads, full-registry PUT, interaction
/// callbacks (204) and deferred edits (ID-bearing 200 receipts). The first
/// scheduled-events GET holds the single-flight lane for `hold` to model
/// brief contention (`seen` fires when the hold starts); later reads pass
/// through immediately.
async fn composition_rest(seen: Arc<AtomicBool>, hold: Duration) -> MockRest {
    let held = Arc::new(AtomicBool::new(false));
    MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path.contains("scheduled-events") {
            seen.store(true, Ordering::Release);
            if !held.swap(true, Ordering::AcqRel) {
                return ScriptedResponse::json(
                    200,
                    json!({"id":EVENT,"guild_id":GUILD,"status":1}),
                )
                .delayed(hold);
            }
            return ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}));
        }
        if request.method == "GET" && request.path.ends_with("/applications/@me") {
            return ScriptedResponse::json(200, json!({"id":"1111"}));
        }
        if request.method == "GET" {
            // Custom-command bootstrap guild read.
            return ScriptedResponse::json(200, json!({"id":GUILD,"name":"Bootstrap guild"}));
        }
        if request.method == "PUT" {
            return ScriptedResponse::status(200);
        }
        if request.path.ends_with("/callback") {
            return ScriptedResponse::status(204);
        }
        // Deferred original edits require ID-bearing 200 receipts.
        ScriptedResponse::json(200, json!({"id":"99"}))
    })
    .await
}

async fn seed_faq(db: &TestDb) {
    two_bot_core::custom_command_service::put(
        &db.pool,
        true,
        GUILD,
        "3333",
        &two_bot_core::custom_commands::PutCommandInput {
            name: "faq".into(),
            description: "FAQ".into(),
            template: "Hi {user} {username} in {server} {channel}".into(),
            text_trigger: Some("!faq".into()),
        },
        "seed-faq",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("seed custom command");
}

/// Governed composition under test: the ordered RSVP runtime and the
/// detached command runtime (with initialized custom commands) send through
/// the same durable single-flight lane. One admission row per test token.
async fn spawn_governed_with_commands(
    db: &TestDb,
    url: &str,
    rest: &MockRest,
    token: &str,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) -> JoinHandle<Result<(), sqlx::Error>> {
    ensure_crypto_provider();
    let admission: Arc<dyn SendAdmission> =
        Arc::new(PgSendAdmission::new(db.pool.clone(), token).unwrap());
    let ordered = Arc::new(InteractionRuntime::with_router(
        InteractionRouter::new(gates()),
        db.pool.clone(),
        ActionExecutor::with_admission(token.into(), Some(rest.origin()), Arc::clone(&admission))
            .unwrap(),
        0,
        ClassifierConfig::default(),
    ));
    let commands = crate::command_runtime::CommandRuntime::new(
        db.pool.clone(),
        ActionExecutor::with_admission(token.into(), Some(rest.origin()), admission).unwrap(),
        crate::command_runtime::router_with_commands(gates()),
        GUILD.parse().unwrap(),
        true,
    );
    commands
        .initialize_custom_commands(GatewayCommandConfig::from_map(2222, &vars()).unwrap())
        .await
        .expect("custom-command bootstrap");
    let saved = load_boot_session(&db.store).await.unwrap();
    let shard =
        crate::gateway::build_shard(TOKEN.into(), Intents::empty(), saved.as_ref(), Some(url));
    tokio::spawn(run_shard(
        shard,
        Arc::new(build_pipeline(db.store.milestones().await.unwrap(), None)),
        Arc::new(RwLock::new(GatewayState::Armed)),
        db.store.clone(),
        Some(ordered),
        None,
        Some(commands),
        None,
        None,
        async move {
            match shutdown {
                Some(receiver) => crate::server::shutdown_requested(receiver).await,
                None => std::future::pending().await,
            }
        },
    ))
}

fn rsvp_packet(sequence: u64, status: &str) -> Value {
    json!({"op":0,"s":sequence,"t":"INTERACTION_CREATE","d":{
        "application_id":"1111", "authorizing_integration_owners":{"0":GUILD},
        "id":sequence.to_string(), "token":format!("mock-rsvp-{sequence}"),
        "type":2, "version":1, "guild_id":GUILD,
        "member":{"permissions":"0", "roles":[], "deaf":false, "mute":false,
            "flags":0, "user":{"id":"77", "username":"human", "discriminator":"0"}},
        "data":{"id":"4444", "name":"rsvp", "type":1, "options":[
            {"name":"event-id", "type":3, "value":EVENT},
            {"name":"status", "type":3, "value":status}
        ]}
    }})
}

fn faq_packet(sequence: u64, id: u64, token: &str) -> Value {
    json!({"op": 0, "s": sequence, "t": "INTERACTION_CREATE", "d": {
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": token, "version": 1,
        "guild_id": GUILD, "channel": {"id": "4444", "type": 0, "name": "commands"},
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": [],
        "member": {
            "permissions": "0", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": "faq", "type": 1, "options": []}
    }})
}

fn published_names(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Vec<Value>>(body)
        .expect("registry body")
        .iter()
        .map(|command| command["name"].as_str().expect("command name").to_owned())
        .collect()
}

/// The single serialized boot replacement carries the seeded custom row
/// exactly once alongside every constrained builtin, exactly once each.
fn assert_merged_registry(body: &[u8]) {
    let names = published_names(body);
    for name in [
        "faq",
        "rank",
        "leaderboard",
        "attendance",
        "rsvp",
        "rsvp-attendance",
    ] {
        assert_eq!(
            names
                .iter()
                .filter(|published| published.as_str() == name)
                .count(),
            1,
            "boot must publish the merged registry with {name} exactly once"
        );
    }
}

async fn wait_put(rest: &MockRest) -> Vec<crate::discord_test_common::RestRequest> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let puts: Vec<_> = rest
                .requests()
                .into_iter()
                .filter(|request| {
                    request.method == "PUT"
                        && request.path == "/api/v10/applications/1111/guilds/2222/commands"
                })
                .collect();
            if !puts.is_empty() {
                return puts;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("boot registry PUT deadline")
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn boot_registry_keeps_seeded_custom_row_on_fresh_ready() {
    let db = TestDb::new().await;
    seed_faq(&db).await;
    let rest = composition_rest(Arc::new(AtomicBool::new(false)), Duration::ZERO).await;
    let mut gateway = MockGateway::new(false, false).await;
    let runner =
        spawn_governed_with_commands(&db, &gateway.url, &rest, "mock-token-custom-fresh", None)
            .await;
    assert_eq!(gateway.authentication().await["op"], 2);
    // MockGateway answers IDENTIFY with READY + a leave; the boot PUT
    // precedes gateway connect, so it is already recorded once READY lands.
    wait_sequence(&db.store, 2).await;
    let puts = wait_put(&rest).await;
    assert_eq!(
        puts.len(),
        1,
        "one serialized boot owner; READY republishes nothing"
    );
    assert_merged_registry(&puts[0].body);
    // The persisted row is still the enabled source of that published entry.
    let row = two_bot_core::custom_command_store::get_command(&db.pool, GUILD, "faq")
        .await
        .expect("custom row read")
        .expect("seeded custom row");
    assert!(row.enabled);
    runner.abort();
    let _ = runner.await;
    gateway.task.abort();
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn boot_registry_keeps_seeded_custom_row_on_persisted_resumed() {
    let db = TestDb::new().await;
    let mut gateway = MockGateway::new(false, true).await;
    db.store
        .commit_dispatch(
            &checkpoint("persisted-session", 2, &gateway.url),
            FunnelBatch::default(),
        )
        .await
        .unwrap();
    seed_faq(&db).await;
    let rest = composition_rest(Arc::new(AtomicBool::new(false)), Duration::ZERO).await;
    let runner =
        spawn_governed_with_commands(&db, &gateway.url, &rest, "mock-token-custom-resumed", None)
            .await;
    assert_eq!(gateway.authentication().await["op"], 6);
    wait_sequence(&db.store, 3).await;
    let puts = wait_put(&rest).await;
    assert_eq!(
        puts.len(),
        1,
        "one serialized boot owner; RESUMED republishes nothing"
    );
    assert_merged_registry(&puts[0].body);
    runner.abort();
    let _ = runner.await;
    gateway.task.abort();
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn mixed_rsvp_custom_command_survives_brief_lane_contention() {
    let db = TestDb::new().await;
    seed_faq(&db).await;
    let seen = Arc::new(AtomicBool::new(false));
    // Inside the custom 2.5 s receipt-relative retry budget and Discord's
    // three-second acknowledgement window.
    let rest = composition_rest(Arc::clone(&seen), Duration::from_millis(1500)).await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let runner =
        spawn_governed_with_commands(&db, &url, &rest, "mock-token-mixed", Some(receiver)).await;
    let (socket, _) = listener.accept().await.unwrap();
    let (_, mut ws) = ServerBuilder::new().accept(socket).await.unwrap();
    ws.send(Message::text(
        json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
    ))
    .await
    .unwrap();
    loop {
        let message = ws.next().await.unwrap().unwrap();
        if !message.is_text() {
            continue;
        }
        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
        if packet["op"] == 2 {
            break;
        }
        if packet["op"] == 1 {
            ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                .await
                .unwrap();
        }
    }
    ws.send(Message::text(ready(&url, "mixed-session").to_string()))
        .await
        .unwrap();
    wait_sequence(&db.store, 1).await;
    // The RSVP live-event read holds the single-flight lane; send the custom
    // command into certain occupancy once the hold has started.
    ws.send(Message::text(rsvp_packet(2, "going").to_string()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !seen.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("lane hold never started");
    let delivered = tokio::time::Instant::now();
    ws.send(Message::text(faq_packet(3, 3, "mock-faq-3").to_string()))
        .await
        .unwrap();
    // One final custom edit has reached MockRest. It records the PATCH before
    // responding, so wait for the post-response audit before shutdown can
    // cancel the detached command task.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let done = rest
                .requests()
                .iter()
                .any(|request| request.method == "PATCH" && request.path.contains("mock-faq-3"));
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("custom completion deadline");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let runs: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM automation_audit_log WHERE target_key = 'faq' AND action = 'command.run'",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if runs >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("custom command audit deadline");
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(30), runner)
        .await
        .expect("mixed runner shutdown deadline")
        .unwrap()
        .unwrap();
    drop(ws);
    let requests = rest.requests();
    // One timely custom callback: earlier attempts met the held lane and
    // backed off inside the receipt budget instead of failing outright.
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| {
            request
                .path
                .ends_with("/interactions/3/mock-faq-3/callback")
        })
        .collect();
    assert_eq!(callbacks.len(), 1);
    let waited = callbacks[0].received_at.duration_since(delivered);
    assert!(waited > Duration::from_millis(1000), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // One custom effect and one final edit with the rendered template.
    let edits: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.contains("mock-faq-3"))
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&edits[0].body).unwrap()["content"],
        "Hi <@3333> tester in Bootstrap guild <#4444>"
    );
    // The seed above commits its own `command.create` audit row, so scope
    // the run assertion to the invocation record: exactly one `command.run`.
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM automation_audit_log WHERE target_key = 'faq' AND action = 'command.run'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        1
    );
    // The RSVP side still executes once, in gateway dispatch order.
    let rsvp_callbacks: Vec<_> = requests
        .iter()
        .filter(|request| {
            request
                .path
                .ends_with("/interactions/2/mock-rsvp-2/callback")
        })
        .collect();
    assert_eq!(rsvp_callbacks.len(), 1);
    let rsvp_edits: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.contains("mock-rsvp-2"))
        .collect();
    assert_eq!(rsvp_edits.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&rsvp_edits[0].body).unwrap()["content"],
        "RSVP saved: going."
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM event_rsvps")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        "going"
    );
    rest.shutdown().await;
    tokio::time::timeout(Duration::from_secs(30), db.close())
        .await
        .expect("test schema teardown deadline");
}
