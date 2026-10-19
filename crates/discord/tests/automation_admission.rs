#![cfg(feature = "db")]
//! Actor admission against disposable databases and loopback Discord only.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use twilight_model::{application::interaction::Interaction, channel::Message};
use two_bot_core::{
    custom_command_service, custom_commands::AutomationMessageAcceptance::Unmatched,
    custom_commands::PutCommandInput, lfg, lfg_store, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandRuntime, TextCommandOutcome},
    interactions::InteractionRuntime,
    lfg_interactions::{LfgInteractions, LfgRequest},
    ActionExecutor, DiscordError,
};
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("disposable migrated fixture")
}

fn gates(guild: u64) -> RouterGates {
    RouterGates {
        configured_guild: Some(guild),
        automations: true,
        announcements: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("automation-fixture".into(), Some(mock.origin())).unwrap()
}

fn text_runtime(pool: sqlx::PgPool, mock: &MockRest, guild: u64) -> CustomCommandRuntime {
    CustomCommandRuntime::new(
        pool,
        Arc::new(InteractionRouter::new(gates(guild))),
        executor(mock),
        1111,
    )
}

fn message(id: u64, actor: u64, guild: u64, content: &str) -> Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "guild_id": guild.to_string(), "channel_id": "4444", "type": 0,
        "author": {"id": actor.to_string(), "username": "member", "discriminator": "0", "avatar": null},
        "content": content, "timestamp": "2026-10-08T00:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false
    })).unwrap()
}

async fn advance_window() {
    // Do not pause during loopback socket/SQL I/O: it can auto-advance deadlines.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::time::resume();
}

async fn text(runtime: &CustomCommandRuntime, input: &Message) -> TextCommandOutcome {
    runtime
        .handle_message(input, Unmatched, true, Some("Fixture guild"))
        .await
        .unwrap()
}

#[tokio::test]
async fn text_burst_is_bounded_clones_share_window_and_restart_keeps_permanent_replay() {
    let db = database().await;
    for guild in [2222, 2223] {
        custom_command_service::put(
            db.pool(),
            true,
            &guild.to_string(),
            "3333",
            &PutCommandInput {
                name: "faq".into(),
                description: "FAQ".into(),
                template: "hello".into(),
                text_trigger: Some("!faq".into()),
            },
            &format!("seed-{guild}"),
            &two_bot_core::now_iso(),
        )
        .await
        .unwrap();
    }
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "9000"}))).await;
    let rt = text_runtime(db.pool().clone(), &mock, 2222);
    let clone = rt.clone();
    let original = message(100, 3333, 2222, "!faq");
    assert_eq!(text(&rt, &original).await, TextCommandOutcome::Delivered);
    for id in 101..201 {
        // Unknown candidates count too, so refusal precedes their SQL lookup.
        assert_eq!(
            text(&clone, &message(id, 3333, 2222, "!unknown")).await,
            TextCommandOutcome::CoolingDown
        );
    }
    assert_eq!(
        text(&rt, &original).await,
        TextCommandOutcome::AlreadyAttempted
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log WHERE id LIKE 'custom:text:%'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        rows, 2,
        "one permanent attempt plus one result, not one pair per refusal/replay"
    );
    assert_eq!(mock.requests().len(), 1);

    assert_eq!(
        text(&rt, &message(201, 3334, 2222, "!faq")).await,
        TextCommandOutcome::Delivered
    );
    let foreign = text_runtime(db.pool().clone(), &mock, 2223);
    assert_eq!(
        text(&foreign, &message(202, 3333, 2223, "!faq")).await,
        TextCommandOutcome::Delivered
    );
    advance_window().await;
    assert_eq!(
        text(&rt, &message(203, 3333, 2222, "!faq")).await,
        TextCommandOutcome::Delivered
    );
    let restarted = text_runtime(db.pool().clone(), &mock, 2222);
    assert_eq!(
        text(&restarted, &original).await,
        TextCommandOutcome::AlreadyAttempted
    );
    // Restart clears only the abuse window: a fresh event can be admitted.
    let fresh = text_runtime(db.pool().clone(), &mock, 2222);
    assert_eq!(
        text(&fresh, &message(204, 3333, 2222, "!faq")).await,
        TextCommandOutcome::Delivered
    );
    assert_eq!(mock.requests().len(), 5);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM automation_audit_log WHERE id LIKE 'custom:text:%'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(rows, 10);

    // A closed pool makes any accidental refused-event SQL access observable.
    db.pool().close().await;
    for id in 205..305 {
        assert_eq!(
            text(&fresh, &message(id, 3333, 2222, "!faq")).await,
            TextCommandOutcome::CoolingDown
        );
    }
    assert_eq!(mock.requests().len(), 5);
    mock.shutdown().await;
    db.close().await.unwrap();
}

fn select(id: u64, actor: u64, post: &str, role: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 3,
        "guild_id": "2222", "channel": {"id": "4444", "type": 0},
        "member": {"user": {"id": actor.to_string(), "username": "member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0, "permissions": "0"},
        "token": "automation-fixture", "version": 1, "entitlements": [], "authorizing_integration_owners": {},
        "data": {"custom_id": lfg::lfg_custom_id(post), "component_type": 3, "values": [role]}
    })).unwrap()
}

fn channel_writes(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .count()
}

#[tokio::test]
async fn signup_burst_has_no_audit_or_refresh_and_leave_close_and_retry_remain_usable() {
    let db = database().await;
    let post = lfg::LfgPost {
        id: "admission-post".into(),
        guild_id: "2222".into(),
        channel_id: "4444".into(),
        message_id: Some("5000".into()),
        title: "Fixture".into(),
        starts_at: "2099-10-08T00:00:00Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "3333".into(),
        created_at: two_bot_core::now_iso(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(
        &post.id,
        &lfg::parse_role_spec("tank:Tank:2,dps:DPS:2").unwrap(),
    );
    lfg_store::put_lfg(db.pool(), &post, &roles, true)
        .await
        .unwrap();
    let acknowledged = Mutex::new(HashSet::new());
    let mock = MockRest::with_responder(move |request| {
        if request.path.ends_with("/callback") {
            if !acknowledged.lock().unwrap().insert(request.path.clone()) {
                return ScriptedResponse::json(
                    400,
                    json!({"code": 40060, "message": "Interaction has already been acknowledged"}),
                );
            }
            return ScriptedResponse::status(204);
        }
        ScriptedResponse::json(200, json!({"id": "5000"}))
    })
    .await;
    // A resumed runtime has no bot identity. Signup must not perform GET /users/@me.
    let rt = InteractionRuntime::new(
        gates(2222),
        db.pool().clone(),
        executor(&mock),
        0,
        Default::default(),
    );
    rt.set_application_id(1111);
    let original = select(1000, 3333, &post.id, "tank");
    assert!(rt.handle(&original).await.unwrap());
    assert_eq!(channel_writes(&mock), 1);
    for id in 1001..1101 {
        assert!(rt.handle(&select(id, 3333, &post.id, "dps")).await.unwrap());
        let requests = mock.requests();
        let reply: serde_json::Value =
            serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(reply["type"], 4, "refuse without defer or completion");
        assert_eq!(reply["data"]["flags"], 64);
        assert_eq!(
            reply["data"]["content"],
            "Wait 5 seconds before joining or changing another LFG role."
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(channel_writes(&mock), 1, "refusals do not refresh the post");
    assert!(!mock.requests().iter().any(|r| r.method == "GET"));
    assert_eq!(
        lfg_store::list_lfg_signups(db.pool(), &post.id)
            .await
            .unwrap()[0]
            .role_key,
        "tank"
    );

    // Independent actors and leave recovery are not throttled.
    rt.handle(&select(1101, 3334, &post.id, "dps"))
        .await
        .unwrap();
    rt.handle(&select(1102, 3333, &post.id, "__leave__"))
        .await
        .unwrap();
    let before = mock.requests().len();
    let refreshes = channel_writes(&mock);
    assert!(matches!(
        rt.handle(&original).await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(mock.requests().len(), before + 1, "no second completion");
    assert_eq!(channel_writes(&mock), refreshes, "no service execution");

    // Direct service replay, also across runtime recreation, reuses the audit.
    // It is distinct from routed replay rejected at Discord's acknowledgement.
    let service = LfgInteractions::new(db.pool().clone());
    let reply = service
        .execute(
            &executor(&mock),
            LfgRequest::Select(lfg::LfgSelectAction::Signup {
                post_id: post.id.clone(),
                role_key: "tank".into(),
            }),
            "2222",
            "3333",
            original.id.get(),
            0,
        )
        .await
        .unwrap();
    assert_eq!(reply, "LFG joined.");
    assert_eq!(channel_writes(&mock), refreshes + 1);
    assert_eq!(
        lfg_store::list_lfg_signups(db.pool(), &post.id)
            .await
            .unwrap()
            .len(),
        1,
        "retry must not rejoin after leave"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log WHERE action = 'lfg.signup' AND actor_id = '3333'")
        .fetch_one(db.pool()).await.unwrap();
    assert_eq!(rows, 1, "same-event retry reuses its outcome audit");
    advance_window().await;
    rt.handle(&select(1103, 3333, &post.id, "dps"))
        .await
        .unwrap();
    assert_eq!(
        lfg_store::list_lfg_signups(db.pool(), &post.id)
            .await
            .unwrap()
            .len(),
        2
    );

    // Close is a privileged slash route, independent of the signup window.
    let close: Interaction = serde_json::from_value(json!({
        "id": "1104", "application_id": "1111", "type": 2, "guild_id": "2222", "channel": {"id": "4444", "type": 0},
        "member": {"user": {"id": "3333", "username": "member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
            "permissions": two_bot_core::commands::PERM_MANAGE_EVENTS.to_string()},
        "token": "automation-fixture", "version": 1, "entitlements": [], "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "lfg-close", "type": 1, "options": [{"type": 3, "name": "id", "value": post.id}]}
    })).unwrap();
    rt.handle(&close).await.unwrap();
    assert_eq!(
        lfg_store::get_lfg(db.pool(), "2222", &post.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        lfg::LfgStatus::Closed
    );

    // Refusal still succeeds when the pool is closed, before connection admission.
    let before = channel_writes(&mock);
    db.pool().close().await;
    rt.handle(&select(1105, 3333, &post.id, "tank"))
        .await
        .unwrap();
    assert_eq!(channel_writes(&mock), before);
    mock.shutdown().await;
    db.close().await.unwrap();
}
