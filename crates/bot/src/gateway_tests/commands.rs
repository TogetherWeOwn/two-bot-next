//! Real shard + runtime composition, with only loopback Discord traffic and
//! the parent's guarded, isolated TestDb. All database tests are opt-in.
use super::*;

use std::collections::HashMap;

use crate::gateway_commands::{GatewayCommandConfig, GatewayCommands};
use two_bot_discord::ActionExecutor;

use crate::discord_test_common::{MockRest, ScriptedResponse};

const RENDERED: &str = "Hi <@3333> tester in Bootstrap guild <#4444>";
const BOUND: Duration = Duration::from_secs(5);

fn vars(automod: Option<&str>) -> HashMap<String, String> {
    let mut vars = HashMap::from([
        ("TWO_AUTOMATIONS".into(), "1".into()),
        ("TWO_TEXT_COMMANDS".into(), "1".into()),
    ]);
    if let Some(value) = automod {
        vars.insert("TWO_AUTOMOD".into(), value.into());
    }
    vars
}

fn bootstrap_responses() -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::json(200, json!({"id": "1111"})),
        ScriptedResponse::json(200, json!({"id": GUILD, "name": "Bootstrap guild"})),
    ]
}

async fn bootstrap(
    db: &TestDb,
    rest: &MockRest,
    vars: &HashMap<String, String>,
) -> Arc<crate::command_runtime::CommandRuntime> {
    let config = GatewayCommandConfig::from_map(2222, vars).expect("command config");
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap();
    let commands = tokio::time::timeout(
        BOUND,
        GatewayCommands::bootstrap(db.pool.clone(), executor, config),
    )
    .await
    .expect("bootstrap deadline")
    .expect("bootstrap context");
    let requests = rest.requests();
    assert_eq!(
        requests.len(),
        2,
        "context fetched before constructing shard"
    );
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].path.ends_with("/applications/@me"));
    assert_eq!(requests[1].method, "GET");
    assert!(requests[1].path.ends_with("/guilds/2222"));
    commands
}

async fn seed(db: &TestDb) {
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
        "seed",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("seed custom command");
}

// The JSON bodies mirror discord/tests/custom_command_runtime.rs, including
// Twilight's required integration owners, member, and message fields.
fn slash(sequence: u64, id: u64) -> Value {
    json!({"op": 0, "s": sequence, "t": "INTERACTION_CREATE", "d": {
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "token": "custom-command-fixture", "version": 1,
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

fn prefix(sequence: u64, id: u64) -> Value {
    json!({"op": 0, "s": sequence, "t": "MESSAGE_CREATE", "d": {
        "id": id.to_string(), "guild_id": GUILD, "channel_id": "4444", "type": 0,
        "author": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null},
        "content": "!FaQ ignored arguments", "timestamp": "2026-09-30T12:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false
    }})
}

fn resumed(sequence: u64) -> Value {
    json!({"op": 0, "s": sequence, "t": "RESUMED", "d": {}})
}

/// Channel-driven dispatch like deadline.rs: no race with READY checkpointing,
/// no sleeps to inject events, and heartbeat ACKs remain live while waiting.
struct CommandGateway {
    url: String,
    dispatch: mpsc::Sender<Value>,
    auth: mpsc::Receiver<Value>,
    task: JoinHandle<()>,
}

impl CommandGateway {
    async fn start(heartbeat_ms: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (dispatch, mut incoming) = mpsc::channel::<Value>(4);
        let (authenticated, auth) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(
                json!({"op": 10, "d": {"heartbeat_interval": heartbeat_ms}}).to_string(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    packet = ws.next() => {
                        let Some(Ok(message)) = packet else { break };
                        if !message.is_text() { continue; }
                        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                        match packet["op"].as_u64() {
                            Some(1) => ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned())).await.unwrap(),
                            Some(2 | 6) => authenticated.send(packet).await.unwrap(),
                            _ => {}
                        }
                    }
                    Some(event) = incoming.recv() => {
                        ws.send(Message::text(event.to_string())).await.unwrap();
                    }
                }
            }
        });
        Self {
            url,
            dispatch,
            auth,
            task,
        }
    }

    async fn authentication(&mut self) -> Value {
        tokio::time::timeout(BOUND, self.auth.recv())
            .await
            .expect("authentication deadline")
            .expect("authentication")
    }

    async fn send(&self, event: Value) {
        tokio::time::timeout(BOUND, self.dispatch.send(event))
            .await
            .expect("dispatch deadline")
            .expect("dispatch receiver");
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn wait_connected(state: &RwLock<GatewayState>) {
    tokio::time::timeout(BOUND, async {
        while *state.read().await != GatewayState::Connected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("readiness deadline");
}

async fn wait_requests(rest: &MockRest, count: usize) {
    tokio::time::timeout(BOUND, async {
        while rest.requests().len() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("REST request deadline");
}

/// Detached command work commits its audit rows after the checkpoint the test
/// already waited on, so poll for them instead of asserting immediately.
async fn wait_audit_ok(pool: &PgPool, expected: i64) {
    tokio::time::timeout(BOUND, async {
        loop {
            let ok: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM automation_audit_log WHERE action = 'command.run' AND outcome = 'ok'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if ok >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("command audit deadline");
}

async fn wait_audit_result(pool: &PgPool, id: &str) -> (String, String) {
    tokio::time::timeout(BOUND, async {
        loop {
            if let Ok(fact) =
                sqlx::query_as("SELECT outcome, reason FROM automation_audit_log WHERE id = $1")
                    .bind(id)
                    .fetch_one(pool)
                    .await
            {
                return fact;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("command audit deadline")
}

fn assert_registry(rest: &MockRest) {
    let requests = rest.requests();
    let sets = requests
        .iter()
        .filter(|request| request.method == "PUT")
        .collect::<Vec<_>>();
    assert_eq!(sets.len(), 1);
    assert!(sets[0]
        .path
        .ends_with("/applications/1111/guilds/2222/commands"));
    let commands: Vec<Value> = serde_json::from_slice(&sets[0].body).unwrap();
    for name in ["rank", "command", "command-remove", "command-list", "faq"] {
        assert!(
            commands.iter().any(|command| command["name"] == name),
            "complete registry contains {name}"
        );
    }
}

fn assert_rendered_replies(rest: &MockRest, message_id: u64) {
    let requests = rest.requests();
    let edits = requests
        .iter()
        .filter(|request| request.method == "PATCH")
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), 1);
    assert!(edits[0]
        .path
        .ends_with("/webhooks/1111/custom-command-fixture/messages/@original"));
    let edit: Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert_eq!(edit["content"], RENDERED);
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    let callbacks = requests
        .iter()
        .filter(|request| request.path.ends_with("/callback"))
        .collect::<Vec<_>>();
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].method, "POST");
    let defer: Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    let messages = requests
        .iter()
        .filter(|request| request.path.ends_with("/channels/4444/messages"))
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].method, "POST");
    let body: Value = serde_json::from_slice(&messages[0].body).unwrap();
    assert_eq!(body["content"], RENDERED);
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(body["enforce_nonce"], true);
    assert_eq!(body["nonce"], message_id);
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn ready_routes_custom_slash_and_accepted_prefix_before_checkpoint() {
    let db = TestDb::new().await;
    seed(&db).await;
    let mut script = bootstrap_responses();
    script.extend([
        ScriptedResponse::json(200, json!({})), // READY registry
        ScriptedResponse::status(204),          // slash defer
        // The completion receipt must carry a message id: the executor
        // rejects id-less mutation receipts as uncertain delivery.
        ScriptedResponse::json(200, json!({"id": "9001"})), // slash completion
        ScriptedResponse::json(200, json!({"id": "9000"})), // prefix
        ScriptedResponse::status(403),                      // terminal runtime error
    ]);
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let commands = bootstrap(&db, &rest, &vars(Some("0"))).await;
    let mut gateway = CommandGateway::start(45000).await;
    let (runner, state) = spawn_runner_with_commands(&db, &gateway.url, Some(commands)).await;
    assert_eq!(gateway.authentication().await["op"], 2);
    gateway.send(ready(&gateway.url, "command-session")).await;
    wait_sequence(&db.store, 1).await;
    // Registry publication rides a detached spawn lane, so wait for the PUT
    // instead of assuming it lands with the checkpoint.
    wait_requests(&rest, 3).await;
    wait_connected(&state).await;
    assert_registry(&rest);
    gateway.send(slash(2, 20)).await;
    wait_sequence(&db.store, 2).await;
    gateway.send(prefix(3, 51)).await;
    wait_sequence(&db.store, 3).await;
    wait_requests(&rest, 6).await;
    wait_connected(&state).await;
    assert_rendered_replies(&rest, 51);
    assert_eq!(db.count().await, 1, "ordinary message capture still runs");
    wait_audit_ok(&db.pool, 2).await;

    // A terminal runtime failure is not a shard failure and does not block
    // capture/checkpoint; its already-committed reservation still fences replay.
    let mut failed_prefix = prefix(4, 52);
    // An independent actor bypasses neither moderation nor permanent replay,
    // but has their own text-trigger cooldown window.
    failed_prefix["d"]["author"]["id"] = json!("3334");
    gateway.send(failed_prefix).await;
    wait_sequence(&db.store, 4).await;
    wait_requests(&rest, 7).await;
    wait_connected(&state).await;
    let fact = wait_audit_result(&db.pool, "custom:text:result:52").await;
    assert_eq!(fact, ("failed".into(), "delivery_failed".into()));
    assert_eq!(rest.requests().len(), 7);
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn help_tracks_confirmed_ready_add_and_remove_publications() {
    fn interaction(
        id: u64,
        name: &str,
        permissions: &str,
        options: Value,
    ) -> twilight_model::application::interaction::Interaction {
        let mut packet = slash(0, id);
        packet["d"]["data"]["name"] = json!(name);
        packet["d"]["data"]["options"] = options;
        packet["d"]["member"]["permissions"] = json!(permissions);
        serde_json::from_value(packet["d"].take()).expect("interaction fixture")
    }
    fn manage_packet(sequence: u64, id: u64, name: &str, options: Value) -> Value {
        let mut packet = slash(sequence, id);
        packet["d"]["data"]["name"] = json!(name);
        packet["d"]["data"]["options"] = options;
        packet["d"]["member"]["permissions"] = json!("32");
        packet
    }
    fn help_content(rest: &MockRest, id: u64) -> String {
        let requests = rest.requests();
        let suffix = format!("/interactions/{id}/custom-command-fixture/callback");
        let replies: Vec<_> = requests
            .iter()
            .filter(|request| request.path.ends_with(&suffix))
            .collect();
        assert_eq!(replies.len(), 1, "one immediate help callback");
        let reply: Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert_eq!(reply["type"], 4);
        assert_eq!(reply["data"]["flags"], 64);
        assert_eq!(reply["data"]["allowed_mentions"]["parse"], json!([]));
        reply["data"]["content"].as_str().unwrap().to_owned()
    }
    let db = TestDb::new().await;
    seed(&db).await;
    let mut script = bootstrap_responses();
    script.extend([
        ScriptedResponse::status(200), // READY PUT: echo complete registry
        ScriptedResponse::status(204), // help
        ScriptedResponse::status(204), // add defer
        ScriptedResponse::status(200), // add PUT
        ScriptedResponse::json(200, json!({"id": "9001"})),
        ScriptedResponse::status(204), // help after add
        ScriptedResponse::status(204), // remove defer
        ScriptedResponse::status(200), // remove PUT
        ScriptedResponse::json(200, json!({"id": "9001"})),
        ScriptedResponse::status(204), // help after remove
    ]);
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = bootstrap(&db, &rest, &vars(Some("0"))).await;
    assert!(runtime.published_commands().is_none());
    let mut gateway = CommandGateway::start(45000).await;
    let (runner, state) =
        spawn_runner_with_commands(&db, &gateway.url, Some(Arc::clone(&runtime))).await;
    assert_eq!(gateway.authentication().await["op"], 2);
    gateway.send(ready(&gateway.url, "help-session")).await;
    wait_sequence(&db.store, 1).await;
    // Receiving a PUT is not enough: wait for its confirmed receipt/snapshot.
    tokio::time::timeout(BOUND, async {
        while runtime.published_commands().is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("confirmed READY publication deadline");
    wait_connected(&state).await;
    runtime
        .on_interaction(&interaction(80, "help", "0", json!([])))
        .await;
    let initial = help_content(&rest, 80);
    assert!(initial.contains("/faq"), "READY DB row is discoverable");
    assert!(!initial.contains("/newfaq"));
    // Management commands belong to the custom-command runtime, which only the
    // gateway dispatch path reaches; `on_interaction` alone would refuse them.
    gateway
        .send(manage_packet(
            2,
            81,
            "command",
            json!([
                {"name": "name", "type": 3, "value": "newfaq"},
                {"name": "template", "type": 3, "value": "New FAQ"}
            ]),
        ))
        .await;
    wait_sequence(&db.store, 2).await;
    // defer + PUT + completion edit: the completion follows the snapshot swap.
    wait_requests(&rest, 7).await;
    assert!(runtime
        .published_commands()
        .unwrap()
        .iter()
        .any(|command| command.name == "newfaq"));
    runtime
        .on_interaction(&interaction(82, "help", "0", json!([])))
        .await;
    let added = help_content(&rest, 82);
    assert!(added.contains("/faq") && added.contains("/newfaq"));
    gateway
        .send(manage_packet(
            3,
            83,
            "command-remove",
            json!([
                {"name": "name", "type": 3, "value": "newfaq"}
            ]),
        ))
        .await;
    wait_sequence(&db.store, 3).await;
    wait_requests(&rest, 11).await;
    assert!(!runtime
        .published_commands()
        .unwrap()
        .iter()
        .any(|command| command.name == "newfaq"));
    runtime
        .on_interaction(&interaction(84, "help", "0", json!([])))
        .await;
    let removed = help_content(&rest, 84);
    assert!(removed.contains("/faq") && !removed.contains("/newfaq"));
    assert_eq!(
        rest.requests().len(),
        12,
        "help adds no DB/registry REST reads"
    );
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    rest.shutdown().await;
    drop(runtime);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn shared_runtime_preserves_custom_sticky_and_feed_registry_and_single_replies() {
    let db = TestDb::new().await;
    seed(&db).await;
    let mut script = bootstrap_responses();
    script.push(ScriptedResponse::json(200, json!([])));
    for _ in 0..4 {
        script.push(ScriptedResponse::status(204));
        // Completion receipts must carry a message id (see above).
        script.push(ScriptedResponse::json(200, json!({"id": "9001"})));
    }
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let mut config = vars(Some("0"));
    config.insert("TWO_ANNOUNCEMENTS".into(), "1".into());
    let runtime = bootstrap(&db, &rest, &config).await;
    let mut gateway = CommandGateway::start(45000).await;
    let (runner, state) = spawn_runner_with_commands(&db, &gateway.url, Some(runtime)).await;
    assert_eq!(gateway.authentication().await["op"], 2);
    gateway.send(ready(&gateway.url, "combined-session")).await;
    wait_sequence(&db.store, 1).await;
    assert_registry(&rest);
    let registry: Vec<Value> = serde_json::from_slice(&rest.requests()[2].body).unwrap();
    for name in [
        "sticky",
        "sticky-remove",
        "feed-add",
        "feed-remove",
        "feed-list",
    ] {
        assert!(registry.iter().any(|command| command["name"] == name));
    }
    for (index, name) in ["faq", "command-list", "feed-list", "sticky-remove"]
        .into_iter()
        .enumerate()
    {
        let sequence = index as u64 + 2;
        let id = index as u64 + 60;
        let mut interaction = slash(sequence, id);
        interaction["d"]["data"]["name"] = json!(name);
        interaction["d"]["member"]["permissions"] = json!("32");
        gateway.send(interaction).await;
        wait_sequence(&db.store, sequence).await;
        let requests = rest.requests();
        let callback = format!("/interactions/{id}/custom-command-fixture/callback");
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.path.ends_with(&callback))
                .count(),
            1
        );
        assert_eq!(
            requests.iter().filter(|r| r.method == "PATCH").count(),
            index + 1
        );
        let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_ne!(
            body["content"],
            "This command is not available in this build yet."
        );
    }
    wait_connected(&state).await;
    assert_registry(&rest); // No competing builtin-only publisher erased faq.
    assert_eq!(rest.requests().len(), 11);
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn cold_resume_publishes_complete_registry_and_uses_bootstrap_guild_name() {
    let db = TestDb::new().await;
    seed(&db).await;
    let mut script = bootstrap_responses();
    script.extend([
        // RESUMED carries no application id: the registry sync resolves it
        // through the shared executor before the bulk overwrite.
        ScriptedResponse::json(200, json!({"id": "1111"})),
        ScriptedResponse::json(200, json!({})),
        ScriptedResponse::status(204),
        ScriptedResponse::json(200, json!({"id": "9001"})),
        ScriptedResponse::json(200, json!({"id": "9000"})),
    ]);
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let commands = bootstrap(&db, &rest, &vars(Some("0"))).await;
    let mut gateway = CommandGateway::start(45000).await;
    db.store
        .commit_dispatch(
            &checkpoint("cold-session", 42, &gateway.url),
            FunnelBatch::default(),
        )
        .await
        .unwrap();
    let (runner, state) = spawn_runner_with_commands(&db, "ws://127.0.0.1:1", Some(commands)).await;
    let auth = gateway.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["seq"], 42);
    assert_eq!(auth["d"]["session_id"], "cold-session");
    // No READY or GUILD_CREATE: the new pipeline's guild cache is empty.
    gateway.send(resumed(43)).await;
    wait_sequence(&db.store, 43).await;
    wait_requests(&rest, 4).await;
    wait_connected(&state).await;
    assert_registry(&rest);
    gateway.send(slash(44, 21)).await;
    wait_sequence(&db.store, 44).await;
    gateway.send(prefix(45, 53)).await;
    wait_sequence(&db.store, 45).await;
    wait_requests(&rest, 7).await;
    assert_rendered_replies(&rest, 53);
    assert_eq!(db.count().await, 1);
    wait_audit_ok(&db.pool, 2).await;
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn unavailable_automod_or_disabled_command_gates_capture_without_prefix_posts() {
    for (automod, automations, text) in [
        (None, "1", "1"),
        (Some("1"), "1", "1"),
        (Some("invalid"), "1", "1"),
        (Some("0"), "0", "1"),
        (Some("0"), "1", "0"),
    ] {
        let db = TestDb::new().await;
        seed(&db).await;
        let mut script = bootstrap_responses();
        script.push(ScriptedResponse::json(200, json!({})));
        let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
        let mut vars = vars(automod);
        vars.insert("TWO_AUTOMATIONS".into(), automations.into());
        vars.insert("TWO_TEXT_COMMANDS".into(), text.into());
        let commands = bootstrap(&db, &rest, &vars).await;
        let mut gateway = CommandGateway::start(45000).await;
        let (runner, state) = spawn_runner_with_commands(&db, &gateway.url, Some(commands)).await;
        assert_eq!(gateway.authentication().await["op"], 2);
        gateway.send(ready(&gateway.url, "gated-session")).await;
        wait_sequence(&db.store, 1).await;
        gateway.send(prefix(2, 54)).await;
        wait_sequence(&db.store, 2).await;
        wait_connected(&state).await;
        assert_eq!(db.count().await, 1, "capture is not automod acceptance");
        let attempts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM automation_audit_log WHERE action = 'command.text_attempt'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(attempts, 0);
        assert_eq!(rest.requests().len(), 3);
        assert!(rest
            .requests()
            .iter()
            .all(|request| request.method != "POST"));
        runner.abort();
        let _ = runner.await;
        gateway.stop().await;
        rest.shutdown().await;
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn slow_prefix_rest_commits_checkpoint_and_reservation_fences_resume() {
    let db = TestDb::new().await;
    seed(&db).await;
    let mut script = bootstrap_responses();
    script.extend([
        ScriptedResponse::json(200, json!({})),
        ScriptedResponse::json(200, json!({"id": "9000"})).delayed(Duration::from_secs(2)),
    ]);
    let rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let commands = bootstrap(&db, &rest, &vars(Some("0"))).await;
    let mut gateway = CommandGateway::start(45000).await;
    let (runner, state) = spawn_runner_with_commands(&db, &gateway.url, Some(commands)).await;
    assert_eq!(gateway.authentication().await["op"], 2);
    gateway.send(ready(&gateway.url, "slow-session")).await;
    wait_sequence(&db.store, 1).await;
    wait_requests(&rest, 3).await;
    wait_connected(&state).await;
    assert_registry(&rest);
    gateway.send(prefix(2, 60)).await;
    wait_requests(&rest, 4).await;
    // Detached command work never blocks the serial checkpoint writer: the
    // capture commits while the prefix POST is still in flight.
    wait_sequence(&db.store, 2).await;
    assert_eq!(
        *state.read().await,
        GatewayState::Connected,
        "checkpoint commits during slow detached REST"
    );
    assert_eq!(db.count().await, 1, "capture committed with REST pending");
    let attempt: String = sqlx::query_scalar(
        "SELECT outcome FROM automation_audit_log WHERE id = 'custom:text:attempt:60'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(attempt, "unknown");
    // Abandon the shard mid-send: the dispatch guard cancels the in-flight
    // POST, so the attempt stays unresolved instead of recording a guess.
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    let result: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM automation_audit_log WHERE id = 'custom:text:result:60'",
    )
    .fetch_optional(&db.pool)
    .await
    .unwrap();
    assert!(
        result.is_none(),
        "cancellation must not resolve an uncertain send"
    );
    assert_eq!(
        rest.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );

    // New runtime and empty pipeline, same DB checkpoint. Replay arrives before
    // RESUMED, so only the durable attempt can prevent another message POST.
    let mut script = bootstrap_responses();
    script.extend([
        ScriptedResponse::json(200, json!({"id": "1111"})),
        ScriptedResponse::json(200, json!({})),
    ]);
    let restarted_rest = MockRest::start(script, ScriptedResponse::status(500)).await;
    let commands = bootstrap(&db, &restarted_rest, &vars(Some("0"))).await;
    let mut gateway = CommandGateway::start(45000).await;
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&gateway.url)
        .execute(&db.pool)
        .await
        .unwrap();
    let (runner, state) = spawn_runner_with_commands(&db, "ws://127.0.0.1:1", Some(commands)).await;
    let auth = gateway.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["seq"], 2);
    gateway.send(prefix(3, 60)).await;
    wait_sequence(&db.store, 3).await;
    assert_eq!(
        db.count().await,
        1,
        "replayed capture committed exactly once"
    );
    gateway.send(resumed(4)).await;
    wait_sequence(&db.store, 4).await;
    wait_connected(&state).await;
    wait_requests(&restarted_rest, 4).await;
    assert_registry(&restarted_rest);
    assert_eq!(restarted_rest.requests().len(), 4);
    assert!(restarted_rest
        .requests()
        .iter()
        .all(|request| request.method != "POST"));
    runner.abort();
    let _ = runner.await;
    gateway.stop().await;
    restarted_rest.shutdown().await;
    rest.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn ready_registry_failure_or_application_mismatch_warns_and_remains_retryable() {
    for mismatch in [false, true] {
        let db = TestDb::new().await;
        let rest = MockRest::start(bootstrap_responses(), ScriptedResponse::status(403)).await;
        let commands = bootstrap(&db, &rest, &vars(Some("0"))).await;
        let mut gateway = CommandGateway::start(45000).await;
        // Keep a handle: the retryability probe below runs against the same
        // runtime the runner owns.
        let probe = Arc::clone(&commands);
        let (runner, state) = spawn_runner_with_commands(&db, &gateway.url, Some(commands)).await;
        assert_eq!(gateway.authentication().await["op"], 2);
        let mut packet = ready(&gateway.url, "failed-session");
        if mismatch {
            packet["d"]["application"]["id"] = json!("7777");
        }
        gateway.send(packet).await;
        // Detached publication warns instead of failing the runner: the
        // gateway still checkpoints and reports ready, nothing publishes, and
        // the failed sync stays eligible for a later connection event.
        wait_sequence(&db.store, 1).await;
        wait_connected(&state).await;
        if mismatch {
            // The context check fails before any REST write.
            assert_eq!(rest.requests().len(), 2);
            assert!(rest
                .requests()
                .iter()
                .all(|request| request.method != "PUT"));
            let error = probe
                .publish_registry_checked(Some(7777))
                .await
                .expect_err("mismatched application stays unpublished");
            assert_eq!(error, crate::command_runtime::RegistrySyncError::Publish);
        } else {
            wait_requests(&rest, 3).await;
            assert_eq!(rest.requests().len(), 3);
            assert_eq!(
                rest.requests()
                    .iter()
                    .filter(|request| request.method == "PUT")
                    .count(),
                1,
                "one failed registry write, then warn"
            );
            let error = probe
                .publish_registry_checked(Some(1111))
                .await
                .expect_err("refused registry write stays retryable");
            assert_eq!(error, crate::command_runtime::RegistrySyncError::Publish);
        }
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
        runner.abort();
        let _ = runner.await;
        gateway.stop().await;
        rest.shutdown().await;
        db.close().await;
    }
}
