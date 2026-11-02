//! Protected worker regressions: registered slash identity is authoritative
//! after defer too. A raw-name worker must not drop or steal an invocation.
//! Only disposable agent-testdb and the existing mock Discord transport.
#![cfg(test)]

use super::*;
use twilight_model::application::command::Command;
use twilight_model::id::Id;
use two_bot_core::CommandDefinition;

fn registered(name: &str, id: u64) -> Command {
    let mut command = two_bot_discord::publish_commands(&[CommandDefinition::new(
        name,
        "Synthetic registered command.",
    )])
    .remove(0);
    command.id = Some(Id::new(id));
    command.application_id = Some(Id::new(111));
    command.guild_id = Some(Id::new(22));
    command
}

fn identity_runtime(db: &TestSchema, mock: &MockRest) -> OnboardingRuntime {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("identity-test-token".into(), Some(mock.origin())).unwrap();
    executor
        .command_identities()
        .replace_guild(
            111,
            22,
            &[registered("reopen", 9001), registered("help", 9002)],
        )
        .unwrap();
    OnboardingRuntime::new(db.pool.clone(), executor, &vars("legacy", false), 22, 999).unwrap()
}

fn invocation(id: u64, incoming_name: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"111", "authorizing_integration_owners":{"0":"22"},
        "id":"333", "token":"identity-callback", "type":2, "version":1,
        "guild_id":"22",
        "member":{
            "user":{"id":"44","username":"member","discriminator":"0","bot":false},
            "roles":[],"deaf":false,"mute":false,"flags":0,"permissions":"0"
        },
        "data":{
            "id":id.to_string(),"guild_id":"22","name":incoming_name,"type":1,
            "options":[{"name":"kind","type":3,"value":"games"}]
        }
    }))
    .unwrap()
}

async fn assert_no_opener_effects(db: &TestSchema, mock: &MockRest) {
    assert!(posts(mock).is_empty(), "no public message");
    assert!(
        mock.requests().iter().all(|request| {
            !matches!(request.method.as_str(), "PUT" | "DELETE")
                && !request.path.ends_with("/callback")
        }),
        "the acknowledged worker grants no roles and never re-ACKs"
    );
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(events, 0, "opening a menu emits no funnel events");
    assert!(!has_onboarding_prompt(&db.pool, "22", "44").await.unwrap());
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_worker_processes_registered_reopen_id_despite_stale_help_name() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        let runtime = identity_runtime(&db, &mock);
        // Deliberately bypass capture: ingress ownership and worker routing
        // are independent guards. The deferred worker must resolve again.
        runtime
            .handle_acknowledged(&invocation(9001, "help"), NOW)
            .await
            .unwrap();
        let edits = replies(&mock);
        assert_eq!(
            edits.len(),
            1,
            "registered reopen finishes its deferred menu"
        );
        assert_eq!(
            edits[0].path,
            "/api/v10/webhooks/111/identity-callback/messages/@original"
        );
        let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
        assert_eq!(
            body["components"][0]["components"][0]["custom_id"],
            GAME_SELECT_ID
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        assert_no_opener_effects(&db, &mock).await;
        mock.shutdown().await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

async fn worker_rejects(id: u64) {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
        let runtime = identity_runtime(&db, &mock);
        runtime
            .handle_acknowledged(&invocation(id, "reopen"), NOW)
            .await
            .unwrap();
        assert!(
            mock.requests().is_empty(),
            "a misdelivered non-reopen ID must not execute a raw-name menu"
        );
        assert_no_opener_effects(&db, &mock).await;
        mock.shutdown().await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_worker_does_not_steal_registered_help_id_with_reopen_name() {
    worker_rejects(9002).await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_worker_rejects_unknown_id_in_known_scope_despite_reopen_name() {
    worker_rejects(9999).await;
}

// Hydrating command identity must leave the existing select path untouched:
// same payload, one private defer, same grant and selection events.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn reopen_registered_snapshot_keeps_existing_component_execution_control() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let state = Arc::new(Mutex::new(DiscordState::default()));
        let mock = discord(Arc::clone(&state)).await;
        let runtime = identity_runtime(&db, &mock);
        let job = component(GAME_SELECT_ID, &["shooters"]);
        let OnboardingJob::Interaction(interaction) = &job else {
            panic!("component fixture");
        };
        let before = serde_json::to_value(interaction).unwrap();
        assert!(runtime.accepts_interaction(interaction));
        assert_eq!(serde_json::to_value(interaction).unwrap(), before);
        runtime.handle(job, NOW).await.unwrap();
        let callbacks: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|request| request.path.ends_with("/callback"))
            .collect();
        assert_eq!(callbacks.len(), 1);
        let defer: Value = serde_json::from_slice(&callbacks[0].body).unwrap();
        assert_eq!(defer["type"], 5);
        assert_eq!(defer["data"]["flags"], 64);
        let role = GAME_PICKS
            .iter()
            .find(|pick| pick.key == "shooters")
            .unwrap();
        assert!(state.lock().unwrap().roles.contains(role.role_id));
        let writes: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|request| {
                matches!(request.method.as_str(), "PUT" | "DELETE")
                    && request.path.contains("/roles/")
            })
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].method, "PUT");
        assert_eq!(
            writes[0].path,
            format!("/api/v10/guilds/22/members/44/roles/{}", role.role_id)
        );
        assert_eq!(db.count(EVENT_GAME_ROLES_SELECTED).await, 1);
        assert_eq!(db.count(EVENT_CHANNEL_ROUTED).await, 1);
        let edits = replies(&mock);
        assert_eq!(edits.len(), 1);
        let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
        assert!(body["content"].as_str().unwrap().contains("Done."));
        mock.shutdown().await;
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
