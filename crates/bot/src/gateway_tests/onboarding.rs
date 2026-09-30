//! Durable onboarding recovery through a real local shard and mock REST.
//! Only the parent's explicitly opted-in, isolated TestDb is used.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;

use crate::mock_rest::{MockRest, RestRequest, ScriptedResponse};
use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};
use twilight_model::id::Id;
use two_bot_core::onboarding::{
    days_in_guild, goodbye_text, MembershipTrigger, EVENT_CHANNEL_ROUTED,
    EVENT_GAME_ROLES_SELECTED, EVENT_ONBOARDING_PROMPTED, GAME_PICKS, GAME_SELECT_ID,
    SESSION_SELECT_ID,
};
use two_bot_cutover::gateway_session::GatewayJob;
use two_bot_discord::ActionExecutor;

use super::*;
use crate::gateway::GatewayPipeline;
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const SESSION: &str = "onboarding-session";
const DEADLINE: Duration = Duration::from_secs(20);
const RECEIPT_AT: i64 = 1_790_780_400_000;

/// Cancel shard tasks even when an assertion unwinds or the test times out.
struct Runner {
    task: JoinHandle<Result<(), sqlx::Error>>,
    pipeline: Arc<GatewayPipeline>,
}

impl Runner {
    async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct GatewayFixture {
    mock: MockGateway,
    dispatch: mpsc::Sender<Value>,
}

impl GatewayFixture {
    async fn send(&self, packet: Value) {
        self.dispatch.send(packet).await.expect("mock dispatch");
    }
}

impl Drop for GatewayFixture {
    fn drop(&mut self) {
        self.mock.task.abort();
    }
}

/// Reuse the parent's MockGateway/authentication helper, but let each test
/// choose dispatches and their sequence numbers. No live gateway discovery.
async fn gateway(resume: bool) -> GatewayFixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let ready_url = url.clone();
    let (auth_tx, auth) = mpsc::channel(4);
    let (dispatch, mut packets) = mpsc::channel::<Value>(8);
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
        ws.send(Message::text(
            json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
        ))
        .await
        .unwrap();
        while let Some(Ok(message)) = ws.next().await {
            if !message.is_text() {
                continue;
            }
            let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
            match packet["op"].as_u64() {
                Some(1) => ws
                    .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                    .await
                    .unwrap(),
                Some(2 | 6) => {
                    auth_tx.send(packet).await.unwrap();
                    if !resume {
                        ws.send(Message::text(ready(&ready_url, SESSION).to_string()))
                            .await
                            .unwrap();
                    }
                    break;
                }
                _ => {}
            }
        }
        loop {
            tokio::select! {
                packet = packets.recv() => {
                    let Some(packet) = packet else { return };
                    if ws.send(Message::text(packet.to_string())).await.is_err() {
                        return;
                    }
                }
                message = ws.next() => {
                    let Some(Ok(message)) = message else { return };
                    if message.is_text() {
                        let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                        if packet["op"] == 1 && ws.send(Message::text(
                            "{\"op\":11,\"d\":null}".to_owned(),
                        )).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    GatewayFixture {
        mock: MockGateway { url, auth, task },
        dispatch,
    }
}

#[derive(Clone, Copy)]
enum PauseAt {
    PermissionRead,
    Callback,
}

/// Delay before any channel POST (or before defer completion). Aborting the
/// first worker here avoids claiming exactly-once across an uncertain HTTP
/// acceptance: Discord and Postgres are deliberately not one transaction.
async fn discord(paused: Arc<AtomicBool>, pause_at: PauseAt) -> MockRest {
    MockRest::with_responder(move |request| {
        let path = request.path.strip_prefix("/api/v10").unwrap();
        let response = match (request.method.as_str(), path) {
            ("GET", "/guilds/2222") => {
                ScriptedResponse::json(200, json!({"id":GUILD,"owner_id":"888"}))
            }
            ("GET", "/guilds/2222/roles") => ScriptedResponse::json(
                200,
                json!([
                    {"id":GUILD,"permissions":"3072"},
                    {"id":GAME_PICKS[0].role_id,"permissions":"0"}
                ]),
            ),
            ("GET", "/guilds/2222/members/999") => {
                ScriptedResponse::json(200, json!({"user":{"id":"999"},"roles":[]}))
            }
            ("GET", "/guilds/2222/members/77") => {
                ScriptedResponse::json(200, json!({"user":{"id":"77"},"roles":[]}))
            }
            ("GET", "/channels/10" | "/channels/11" | "/channels/12" | "/channels/13") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"id":id,"guild_id":GUILD,"type":if id == "11" { 2 } else { 0 },"permission_overwrites":[]}),
                )
            }
            ("POST", "/channels/12/messages" | "/channels/13/messages") => {
                ScriptedResponse::json(201, json!({"id":"99"}))
            }
            ("POST", path) if path.starts_with("/interactions/") && path.ends_with("/callback") => {
                ScriptedResponse::status(204)
            }
            ("PATCH", path) if path.starts_with("/webhooks/1111/") && path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => ScriptedResponse::status(404),
        };
        let pause = match pause_at {
            PauseAt::PermissionRead => request.method == "GET" && path == "/guilds/2222",
            PauseAt::Callback => request.method == "POST" && path.ends_with("/callback"),
        };
        if pause && paused.load(Ordering::SeqCst) {
            response.delayed(Duration::from_secs(30))
        } else {
            response
        }
    })
    .await
}

async fn spawn_onboarding(db: &TestDb, mock: &MockRest, url: &str, mode: &str) -> Runner {
    ensure_crypto_provider();
    let vars = HashMap::from([
        ("DISCORD_GUILD_ID".into(), GUILD.into()),
        ("TWO_ONBOARDING_MODE".into(), mode.into()),
        ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "12".into()),
        ("DISCORD_GOODBYE_CHANNEL_IDS".into(), "13".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ]);
    let runtime = Arc::new(
        OnboardingRuntime::new(
            db.pool.clone(),
            ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).unwrap(),
            &vars,
            2222,
            999,
        )
        .unwrap(),
    );
    let saved = load_boot_session(&db.store).await.unwrap();
    let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved.as_ref());
    let shard = Shard::with_config(
        ShardId::ONE,
        ConfigBuilder::from(config)
            .proxy_url(url.to_owned())
            .build(),
    );
    // Both the runtime and cache are newly constructed, not reused at restart.
    let pipeline = Arc::new(build_pipeline(db.store.milestones().await.unwrap()));
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let task = tokio::spawn(run_shard(
        shard,
        pipeline.clone(),
        state,
        db.store.clone(),
        Some(runtime),
        None,
    ));
    Runner { task, pipeline }
}

fn member_add(seq: u64, pending: bool, joined_at: &str) -> Value {
    json!({"op":0,"s":seq,"t":"GUILD_MEMBER_ADD","d":{
        "guild_id":GUILD,"user":{"id":"77","username":"mock-member","discriminator":"0","bot":false},
        "roles":[GAME_PICKS[0].role_id],"deaf":false,"mute":false,"flags":0,
        "pending":pending,"joined_at":joined_at
    }})
}

fn gate_clear(seq: u64, joined_at: &str) -> Value {
    json!({"op":0,"s":seq,"t":"GUILD_MEMBER_UPDATE","d":{
        "guild_id":GUILD,"user":{"id":"77","username":"mock-member","discriminator":"0","bot":false},
        "roles":[GAME_PICKS[0].role_id],"pending":false,"joined_at":joined_at
    }})
}

fn resumed(seq: u64) -> Value {
    json!({"op":0,"s":seq,"t":"RESUMED","d":{}})
}

fn component(seq: u64, id: &str, token: &str) -> Value {
    json!({"op":0,"s":seq,"t":"INTERACTION_CREATE","d":{
        "application_id":"1111","authorizing_integration_owners":{"0":GUILD},
        "id":id,"token":token,"type":3,"version":1,"guild_id":GUILD,
        "member":{"user":{"id":"77","username":"mock-member","discriminator":"0"},
            "roles":[],"deaf":false,"mute":false,"flags":0},
        "data":{"custom_id":SESSION_SELECT_ID,"component_type":3,"values":["find-players"]}
    }})
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| {
            request.method == "POST" && request.path.starts_with("/api/v10/channels/")
        })
        .collect()
}

async fn wait_request(mock: &MockRest, method: &str, path: &str) -> RestRequest {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| request.method == method && request.path == path)
            {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock REST deadline")
}

async fn count_jobs(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gateway_onboarding_jobs")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn event_count(db: &TestDb, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = $1")
        .bind(kind)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn receipt(db: &TestDb, seq: i64) -> (String, i32, Option<String>, i64) {
    sqlx::query_as(
        "SELECT state, attempts, payload, occurred_at_ms FROM gateway_onboarding_jobs
         WHERE session_id = $1 AND seq = $2",
    )
    .bind(SESSION)
    .bind(seq)
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

async fn wait_receipt(db: &TestDb, seq: i64, state: &str) -> (String, i32, Option<String>, i64) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let row = receipt(db, seq).await;
            if row.0 == state {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("outbox receipt deadline")
}

async fn repoint_resume(db: &TestDb, mock: &MockGateway) {
    // Only replace the old ephemeral mock port, preserving the sequence fence.
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1 WHERE guild_id = $2 AND shard_id = 0")
        .bind(&mock.url)
        .bind(GUILD)
        .execute(&db.pool)
        .await
        .unwrap();
}

async fn bounded(future: impl Future<Output = ()>) -> std::thread::Result<()> {
    AssertUnwindSafe(async {
        tokio::time::timeout(DEADLINE, future)
            .await
            .expect("bounded onboarding gateway test");
    })
    .catch_unwind()
    .await
}

async fn cleanup(db: TestDb, mock: Option<MockRest>, result: std::thread::Result<()>) {
    if let Some(mock) = mock {
        mock.shutdown().await;
    }
    // All runners/WS fixtures have dropped before closing the pool, including
    // on assertion failure; never strand their schema or an advisory lock.
    tokio::time::timeout(DEADLINE, db.close())
        .await
        .expect("isolated schema cleanup deadline");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

async fn welcome_restart(pending: bool) {
    let db = TestDb::new().await;
    let paused = Arc::new(AtomicBool::new(true));
    let mock = discord(paused.clone(), PauseAt::PermissionRead).await;
    let result = bounded(async {
        let joined_at = two_bot_core::format_iso_millis(RECEIPT_AT);
        let mut first = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &first.mock.url, "legacy").await;
        assert_eq!(first.mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 1).await;
        first.send(member_add(2, pending, &joined_at)).await;
        wait_sequence(&db.store, 2).await;
        let (seq, replay) = if pending {
            wait_receipt(&db, 2, "completed").await;
            assert!(mock.requests().is_empty(), "pending joins do not prompt");
            assert!(runner
                .pipeline
                .cache()
                .member(Id::new(2222), Id::new(77))
                .unwrap()
                .pending());
            let update = gate_clear(3, &joined_at);
            first.send(update.clone()).await;
            (3, update)
        } else {
            (2, member_add(2, false, &joined_at))
        };
        wait_sequence(&db.store, seq).await;
        wait_request(&mock, "GET", "/api/v10/guilds/2222").await;
        assert!(posts(&mock).is_empty(), "interruption is before any POST");
        let saved = receipt(&db, seq as i64).await;
        assert_eq!(saved.0, "running");
        assert_eq!(saved.1, 1);
        let payload = saved.2.as_deref().unwrap();
        match OnboardingJob::recover(payload).unwrap().unwrap() {
            OnboardingJob::Welcome {
                guild_id,
                member_id,
                pending: captured_pending,
                trigger,
                roles,
                ..
            } => {
                assert_eq!((guild_id, member_id, captured_pending), (2222, 77, false));
                assert_eq!(roles, vec![GAME_PICKS[0].role_id.to_owned()]);
                assert!(matches!(
                    (pending, trigger),
                    (true, MembershipTrigger::GateCleared)
                        | (false, MembershipTrigger::Joined { pending: false })
                ));
            }
            _ => panic!("expected captured welcome"),
        }
        assert!(!runner
            .pipeline
            .cache()
            .member(Id::new(2222), Id::new(77))
            .unwrap()
            .pending());
        assert_eq!(event_count(&db, "member_join").await, 1);
        assert_eq!(event_count(&db, "gate_cleared").await, 1);
        runner.stop().await;
        drop(first);

        paused.store(false, Ordering::SeqCst);
        let mut second = gateway(true).await;
        repoint_resume(&db, &second.mock).await;
        let runner = spawn_onboarding(&db, &mock, &second.mock.url, "legacy").await;
        assert!(
            runner
                .pipeline
                .cache()
                .member(Id::new(2222), Id::new(77))
                .is_none(),
            "cold cache has no transition evidence"
        );
        let auth = second.mock.authentication().await;
        assert_eq!(auth["op"], 6);
        assert_eq!(auth["d"]["session_id"], SESSION);
        assert_eq!(auth["d"]["seq"], seq);
        second.send(replay).await;
        second.send(resumed(seq + 1)).await;
        wait_sequence(&db.store, seq + 1).await;
        let delivered = wait_receipt(&db, seq as i64, "completed").await;
        assert_eq!(delivered.1, 2, "running work was recovered once");
        assert!(
            delivered.2.is_none(),
            "terminal rows retain no member payload"
        );
        assert_eq!(
            delivered.3, saved.3,
            "original occurrence time survives restart"
        );
        let messages = posts(&mock);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].path, "/api/v10/channels/12/messages");
        let body: Value = serde_json::from_slice(&messages[0].body).unwrap();
        let menu = &body["components"][0]["components"][0];
        assert_eq!(menu["custom_id"], GAME_SELECT_ID);
        assert_eq!(
            menu["options"][0]["default"], true,
            "captured roles survive a cold cache"
        );
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse":[],"users":["77"],"roles":[],"replied_user":false})
        );
        assert_eq!(event_count(&db, EVENT_ONBOARDING_PROMPTED).await, 1);
        assert_eq!(event_count(&db, "member_join").await, 1);
        assert_eq!(event_count(&db, "gate_cleared").await, 1);
        assert_eq!(
            count_jobs(&db).await,
            if pending { 2 } else { 1 },
            "same-sequence replay creates no new job"
        );
        assert!(
            runner
                .pipeline
                .cache()
                .member(Id::new(2222), Id::new(77))
                .is_none(),
            "fenced dispatch never repopulates the cold cache"
        );
        let prompted_at: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM occurred_at) * 1000)::bigint FROM events
             WHERE event_type = 'onboarding_prompted'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(prompted_at, saved.3);
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    cleanup(db, Some(mock), result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; local mock Discord only"]
async fn onboarding_gateway_cold_restart_recovers_ungated_join_once() {
    welcome_restart(false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; local mock Discord only"]
async fn onboarding_gateway_cold_restart_recovers_cached_gate_clear_once() {
    welcome_restart(true).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; local mock Discord only"]
async fn onboarding_gateway_session_goodbye_restart_keeps_captured_joined_at() {
    let db = TestDb::new().await;
    let paused = Arc::new(AtomicBool::new(true));
    let mock = discord(paused.clone(), PauseAt::PermissionRead).await;
    let result = bounded(async {
        let joined_ms = two_bot_core::funnel::now_millis_for_test() - 2 * 86_400_000 - 3_600_000;
        let joined_at = two_bot_core::format_iso_millis(joined_ms);
        let mut first = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &first.mock.url, "session").await;
        assert_eq!(first.mock.authentication().await["op"], 2);
        first.send(member_add(2, true, &joined_at)).await;
        wait_sequence(&db.store, 2).await;
        wait_receipt(&db, 2, "completed").await;
        assert!(mock.requests().is_empty());
        let username = "@everyone <@77> <@&88>";
        let mut departure = leave(3);
        departure["d"]["user"]["username"] = json!(username);
        first.send(departure.clone()).await;
        wait_sequence(&db.store, 3).await;
        wait_request(&mock, "GET", "/api/v10/guilds/2222").await;
        let captured = receipt(&db, 3).await;
        assert_eq!((captured.0.as_str(), captured.1), ("running", 1));
        match OnboardingJob::recover(captured.2.as_deref().unwrap())
            .unwrap()
            .unwrap()
        {
            OnboardingJob::Goodbye {
                guild_id,
                username: name,
                bot,
                joined_at_ms,
            } => {
                assert_eq!(
                    (guild_id, name.as_str(), bot, joined_at_ms),
                    (2222, username, false, Some(joined_ms))
                );
            }
            _ => panic!("expected captured goodbye"),
        }
        assert!(
            runner
                .pipeline
                .cache()
                .member(Id::new(2222), Id::new(77))
                .is_none(),
            "pipeline already removed the member"
        );
        assert!(posts(&mock).is_empty());
        assert_eq!(event_count(&db, "member_leave").await, 1);
        runner.stop().await;
        drop(first);

        paused.store(false, Ordering::SeqCst);
        let mut second = gateway(true).await;
        repoint_resume(&db, &second.mock).await;
        let runner = spawn_onboarding(&db, &mock, &second.mock.url, "session").await;
        assert!(runner
            .pipeline
            .cache()
            .member(Id::new(2222), Id::new(77))
            .is_none());
        let auth = second.mock.authentication().await;
        assert_eq!(auth["op"], 6);
        assert_eq!(auth["d"]["seq"], 3);
        second.send(departure).await;
        second.send(resumed(4)).await;
        wait_sequence(&db.store, 4).await;
        let delivered = wait_receipt(&db, 3, "completed").await;
        assert_eq!(delivered.1, 2);
        assert_eq!(delivered.3, captured.3);
        assert!(delivered.2.is_none());
        let messages = posts(&mock);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].path, "/api/v10/channels/13/messages");
        let body: Value = serde_json::from_slice(&messages[0].body).unwrap();
        let days = days_in_guild(Some(joined_ms), Some(captured.3));
        assert_eq!(days, Some(2));
        assert_eq!(body["content"], goodbye_text(username, days));
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse":[],"users":[],"roles":[],"replied_user":false})
        );
        assert_eq!(event_count(&db, "member_join").await, 1);
        assert_eq!(event_count(&db, "member_leave").await, 1);
        assert_eq!(count_jobs(&db).await, 2);
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        assert!(!mock
            .requests()
            .iter()
            .any(|request| matches!(request.method.as_str(), "PUT" | "DELETE")));
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    cleanup(db, Some(mock), result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; local mock Discord only"]
async fn onboarding_gateway_interrupted_callback_is_token_free_and_requires_reselection() {
    let db = TestDb::new().await;
    let paused = Arc::new(AtomicBool::new(true));
    let mock = discord(paused.clone(), PauseAt::Callback).await;
    let result = bounded(async {
        let mut first = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &first.mock.url, "session").await;
        assert_eq!(first.mock.authentication().await["op"], 2);
        let old_click = component(2, "3333", "interrupted-mock-token");
        first.send(old_click.clone()).await;
        wait_sequence(&db.store, 2).await;
        let callback = wait_request(
            &mock,
            "POST",
            "/api/v10/interactions/3333/interrupted-mock-token/callback",
        )
        .await;
        let defer: Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(defer["type"], 5);
        assert_eq!(
            defer["data"]["flags"], 64,
            "the live in-memory callback was attempted"
        );
        let saved = receipt(&db, 2).await;
        assert_eq!((saved.0.as_str(), saved.1), ("running", 1));
        let payload = saved.2.as_deref().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(payload).unwrap(),
            json!({"interrupted_interaction":"3333"})
        );
        assert!(!payload.contains("interrupted-mock-token"));
        assert!(!payload.contains(TOKEN));
        assert!(OnboardingJob::recover(payload).unwrap().is_none());
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 0);
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        runner.stop().await;
        drop(first);

        paused.store(false, Ordering::SeqCst);
        let mut second = gateway(true).await;
        repoint_resume(&db, &second.mock).await;
        let runner = spawn_onboarding(&db, &mock, &second.mock.url, "session").await;
        let auth = second.mock.authentication().await;
        assert_eq!(auth["op"], 6);
        assert_eq!(auth["d"]["seq"], 2);
        second.send(old_click).await;
        second.send(resumed(3)).await;
        wait_sequence(&db.store, 3).await;
        let interrupted = wait_receipt(&db, 2, "interrupted").await;
        assert_eq!(interrupted.1, 2);
        assert!(interrupted.2.is_none());
        assert_eq!(
            mock.requests().len(),
            1,
            "restart never uses the old token, edits, or mutates roles"
        );
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 0);
        assert_eq!(count_jobs(&db).await, 1, "replayed click is fenced");

        second.send(component(4, "3334", "fresh-mock-token")).await;
        wait_sequence(&db.store, 4).await;
        let completed = wait_receipt(&db, 4, "completed").await;
        assert_eq!(completed.1, 1);
        assert!(completed.2.is_none());
        wait_request(
            &mock,
            "POST",
            "/api/v10/interactions/3334/fresh-mock-token/callback",
        )
        .await;
        let reply = wait_request(
            &mock,
            "PATCH",
            "/api/v10/webhooks/1111/fresh-mock-token/messages/@original",
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(body["content"].as_str().unwrap().contains("10"));
        assert!(reply.header("authorization").is_none());
        assert_eq!(
            event_count(&db, EVENT_CHANNEL_ROUTED).await,
            1,
            "only a fresh member submission succeeds"
        );
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        let source: String =
            sqlx::query_scalar("SELECT source FROM events WHERE event_type = 'channel_routed'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(source, "session-picker");
        assert_eq!(count_jobs(&db).await, 2);
        assert!(posts(&mock).is_empty());
        assert!(!mock
            .requests()
            .iter()
            .any(|request| matches!(request.method.as_str(), "PUT" | "DELETE")));
        assert!(!runner.task.is_finished());
        runner.stop().await;
    })
    .await;
    cleanup(db, Some(mock), result).await;
}

fn durable_welcome() -> GatewayJob {
    GatewayJob {
        payload: OnboardingJob::Welcome {
            guild_id: 2222,
            member_id: 77,
            bot: false,
            pending: false,
            trigger: MembershipTrigger::GateCleared,
            roles: vec![GAME_PICKS[0].role_id.to_owned()],
        }
        .durable_payload()
        .unwrap(),
        occurred_at_ms: RECEIPT_AT,
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_outbox_transaction_rollback_duplicate_and_capacity_32() {
    let db = TestDb::new().await;
    let result = bounded(async {
        db.store
            .commit_dispatch(&checkpoint(SESSION, 1, "ws://mock"), FunnelBatch::default())
            .await
            .unwrap();
        let mut bad = event(EventType::FirstMessage, "2026-09-29T12:00:00.000Z");
        bad.occurred_at = "not-a-timestamp".into();
        assert!(db
            .store
            .commit_dispatch_with_job(
                &checkpoint(SESSION, 2, "ws://mock"),
                FunnelBatch {
                    events: vec![bad],
                    ..Default::default()
                },
                Some(durable_welcome()),
            )
            .await
            .is_err());
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
        assert_eq!(db.count().await, 0);
        assert_eq!(
            count_jobs(&db).await,
            0,
            "job insertion rolls back with the funnel/checkpoint"
        );

        let (action, id) = db
            .store
            .commit_dispatch_with_job(
                &checkpoint(SESSION, 2, "ws://mock"),
                FunnelBatch {
                    events: vec![event(EventType::FirstMessage, "2026-09-29T12:00:00.000Z")],
                    ..Default::default()
                },
                Some(durable_welcome()),
            )
            .await
            .unwrap();
        assert_eq!(action, DispatchAction::Apply);
        let id = id.unwrap();
        let saved = receipt(&db, 2).await;
        assert_eq!(
            (saved.0.as_str(), saved.1, saved.3),
            ("pending", 0, RECEIPT_AT)
        );
        assert_eq!(saved.2.as_deref(), Some(durable_welcome().payload.as_str()));
        let transactions: Vec<String> = sqlx::query_scalar(
            "SELECT xmin::text FROM gateway_sessions UNION ALL
             SELECT xmin::text FROM gateway_onboarding_jobs UNION ALL
             SELECT xmin::text FROM events",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(transactions.len(), 3);
        assert!(
            transactions.iter().all(|value| value == &transactions[0]),
            "checkpoint, facts and job commit together"
        );
        for seq in [2, 1] {
            let result = db
                .store
                .commit_dispatch_with_job(
                    &checkpoint(SESSION, seq, "ws://mock"),
                    FunnelBatch::default(),
                    Some(durable_welcome()),
                )
                .await
                .unwrap();
            assert_eq!(result, (DispatchAction::Duplicate, None));
        }
        assert_eq!(db.count().await, 1);
        assert_eq!(count_jobs(&db).await, 1);
        for seq in 3..=33 {
            assert_eq!(
                db.store
                    .commit_dispatch_with_job(
                        &checkpoint(SESSION, seq, "ws://mock"),
                        FunnelBatch::default(),
                        Some(durable_welcome()),
                    )
                    .await
                    .unwrap()
                    .0,
                DispatchAction::Apply
            );
        }
        let claimed = db.store.claim_onboarding_job().await.unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.payload, durable_welcome().payload);
        assert_eq!(claimed.occurred_at_ms, RECEIPT_AT);
        assert_eq!(count_jobs(&db).await, 32);
        assert!(
            db.store
                .commit_dispatch_with_job(
                    &checkpoint(SESSION, 34, "ws://mock"),
                    FunnelBatch {
                        events: vec![event(EventType::SecondMessage, "2026-09-29T12:00:01.000Z")],
                        ..Default::default()
                    },
                    Some(durable_welcome()),
                )
                .await
                .is_err(),
            "running jobs also occupy the 32-slot bound"
        );
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 33);
        assert_eq!(db.count().await, 1);
        assert_eq!(count_jobs(&db).await, 32);
        db.store.finish_onboarding_job(id, false).await.unwrap();
        assert!(receipt(&db, 2).await.2.is_none());
        assert!(
            db.store.finish_onboarding_job(id, false).await.is_err(),
            "a receipt can finish only a running job"
        );
        let result = db
            .store
            .commit_dispatch_with_job(
                &checkpoint(SESSION, 34, "ws://mock"),
                FunnelBatch::default(),
                Some(durable_welcome()),
            )
            .await
            .unwrap();
        assert_eq!(
            result.0,
            DispatchAction::Apply,
            "terminal receipts release capacity"
        );
        assert!(result.1.is_some());
        assert_eq!(count_jobs(&db).await, 33);
    })
    .await;
    cleanup(db, None, result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_outbox_recovery_stops_after_three_claims() {
    let db = TestDb::new().await;
    let result = bounded(async {
        let (_, id) = db
            .store
            .commit_dispatch_with_job(
                &checkpoint(SESSION, 1, "ws://mock"),
                FunnelBatch::default(),
                Some(durable_welcome()),
            )
            .await
            .unwrap();
        let id = id.unwrap();
        db.store.recover_onboarding_jobs().await.unwrap();
        assert_eq!(
            (receipt(&db, 1).await.0, receipt(&db, 1).await.1),
            ("pending".into(), 0)
        );
        for attempt in 1..=3 {
            let job = db.store.claim_onboarding_job().await.unwrap().unwrap();
            assert_eq!(job.id, id);
            assert_eq!(job.payload, durable_welcome().payload);
            assert_eq!(job.occurred_at_ms, RECEIPT_AT);
            let row = receipt(&db, 1).await;
            assert_eq!((row.0.as_str(), row.1), ("running", attempt));
            assert!(row.2.is_some());
            assert!(
                db.store.claim_onboarding_job().await.unwrap().is_none(),
                "running jobs cannot be claimed twice"
            );
            let recovered = db.store.recover_onboarding_jobs().await;
            let row = receipt(&db, 1).await;
            if attempt < 3 {
                recovered.unwrap();
                assert_eq!((row.0.as_str(), row.1), ("pending", attempt));
            } else {
                assert!(
                    recovered.is_err(),
                    "attempt-limit failure stops boot instead of silently losing work"
                );
                assert_eq!((row.0.as_str(), row.1), ("failed", 3));
                assert!(
                    row.2.is_some(),
                    "failed work retains diagnostic/recovery data"
                );
            }
        }
        assert!(db.store.claim_onboarding_job().await.unwrap().is_none());
        assert!(
            db.store.recover_onboarding_jobs().await.is_err(),
            "failure remains visible on later boots"
        );
        assert!(db.store.finish_onboarding_job(id, false).await.is_err());
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
        assert_eq!(db.count().await, 0);
        assert_eq!(count_jobs(&db).await, 1);
    })
    .await;
    cleanup(db, None, result).await;
}
