//! Exercise the refusal at the real shard boundary, not just the edit parser.
use super::*;

use std::collections::HashMap;

use crate::discord_test_common::{MockRest, ScriptedResponse};
use two_bot_core::automod_runtime::STAGING_GUILD_ID;
use two_bot_discord::ActionExecutor;

fn message() -> Value {
    json!({
        "id": "5555", "guild_id": STAGING_GUILD_ID, "channel_id": "4444", "type": 0,
        "author": {"id": "3333", "username": "fixture", "discriminator": "0", "bot": false},
        "content": "harmless edit", "timestamp": "2026-10-01T12:00:00+00:00",
        "edited_timestamp": "2026-10-01T12:00:01+00:00", "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false
    })
}

fn update(sequence: u64, data: &Value, extra_fields: &str) -> String {
    // Append raw fields, never round-trip the duplicates through Value.
    let data = data.to_string();
    let data = data.strip_suffix('}').expect("object fixture");
    format!(r#"{{"op":0,"s":{sequence},"t":"MESSAGE_UPDATE","d":{data},{extra_fields}}}}}"#)
}

struct RawGateway {
    url: String,
    frames: mpsc::Sender<String>,
    auth: mpsc::Receiver<Value>,
    task: JoinHandle<()>,
}

impl RawGateway {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (frames, mut incoming) = mpsc::channel::<String>(4);
        let (authenticated, auth) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(
                r#"{"op":10,"d":{"heartbeat_interval":45000}}"#.to_owned(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    packet = ws.next() => {
                        let Some(Ok(packet)) = packet else { break };
                        if !packet.is_text() { continue; }
                        let packet: Value = serde_json::from_str(packet.as_text().unwrap()).unwrap();
                        match packet["op"].as_u64() {
                            Some(1) => ws.send(Message::text(r#"{"op":11,"d":null}"#.to_owned())).await.unwrap(),
                            Some(2 | 6) => authenticated.send(packet).await.unwrap(),
                            _ => {}
                        }
                    }
                    Some(frame) = incoming.recv() => {
                        ws.send(Message::text(frame)).await.unwrap();
                    }
                }
            }
        });
        Self {
            url,
            frames,
            auth,
            task,
        }
    }

    async fn send(&self, frame: String) {
        tokio::time::timeout(Duration::from_secs(5), self.frames.send(frame))
            .await
            .expect("frame deadline")
            .expect("frame receiver");
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn duplicate_updates_commit_without_dispatching_to_automod() {
    // Mutant: make run_shard's duplicate_refused always false. Full unknown-key
    // duplicates then reach REST/automod; known-field duplicates stop the runner
    // before their checkpoint. The unique control proves automod is wired.
    let db = TestDb::new().await;
    let rest = MockRest::with_responder(|request| {
        if request.path.ends_with("/channels/4444/messages/5555") {
            ScriptedResponse::json(200, message())
        } else if request.path.ends_with("/members/3333") {
            ScriptedResponse::json(200, json!({"user": {"id": "3333"}, "roles": []}))
        } else {
            ScriptedResponse::json(404, json!({"message": "fixture route missing", "code": 0}))
        }
    })
    .await;
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).unwrap();
    let vars = HashMap::from([
        ("TWO_AUTOMOD".into(), "1".into()),
        ("TWO_OWEN_USER_ID".into(), STAGING_GUILD_ID.into()),
    ]);
    let resolved = crate::automod_gateway::resolve(&vars, STAGING_GUILD_ID.parse().unwrap())
        .expect("automod config")
        .expect("automod enabled");
    let automod = crate::automod_gateway::build(resolved, db.pool.clone(), executor);
    let mut mock = RawGateway::start().await;
    let (shutdown, mut stopping) = tokio::sync::watch::channel(false);
    let (runner, state) =
        spawn_runner_with_automod_until_shutdown(&db, &mock.url, None, Some(automod), async move {
            stopping.wait_for(|stopping| *stopping).await.unwrap();
        })
        .await;
    let auth = tokio::time::timeout(Duration::from_secs(20), mock.auth.recv())
        .await
        .expect("authentication deadline")
        .expect("authentication");
    assert_eq!(auth["op"], 2);
    mock.send(ready(&mock.url, "duplicate-refusal-session").to_string())
        .await;
    wait_sequence(&db.store, 1).await;
    wait_connected(&state).await;

    let partial = json!({"id": "5555", "guild_id": STAGING_GUILD_ID, "channel_id": "4444", "content": "harmless edit"});
    for (index, (data, fields, twilight_accepts)) in [
        (message(), r#""unknown":1,"unknown":2"#, true),
        (message(), r#""unknown":1,"unknown":1"#, true),
        (message(), r#""content":"other edit""#, false),
        (partial, r#""content":"other edit""#, false),
    ]
    .into_iter()
    .enumerate()
    {
        let sequence = index as u64 + 2;
        let frame = update(sequence, &data, fields);
        assert!(crate::automod_gateway::partial_edit(&frame, 0).is_none());
        assert_eq!(
            twilight_gateway::parse(frame.clone(), twilight_gateway::EventTypeFlags::all()).is_ok(),
            twilight_accepts,
            "fixture must exercise its intended Twilight path"
        );
        mock.send(frame).await;
        // This is the persisted cursor for the refused frame itself, not a
        // later sentinel that could conceal a failed or dropped checkpoint.
        wait_sequence(&db.store, sequence).await;
        assert!(
            rest.requests().is_empty(),
            "refused update reached automod enrichment"
        );
        let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM automod_delivery_claims")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(claims, 0, "refused update produced an automod claim");
        assert!(!runner.is_finished(), "refused update stopped the runner");
    }

    mock.send(json!({"op": 0, "s": 6, "t": "MESSAGE_UPDATE", "d": message()}).to_string())
        .await;
    wait_sequence(&db.store, 6).await;
    let requests = rest.requests();
    assert_eq!(
        requests.len(),
        2,
        "unique control must reach message and member enrichment"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    let completed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automod_delivery_claims WHERE completed_at IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        completed, 1,
        "unique control must produce a completed automod verdict"
    );
    assert_eq!(db.count().await, 0, "updates never advance the funnel");

    shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("runner shutdown deadline")
        .expect("runner join")
        .expect("runner result");
    mock.task.abort();
    let _ = mock.task.await;
    rest.shutdown().await;
    db.close().await;
}
