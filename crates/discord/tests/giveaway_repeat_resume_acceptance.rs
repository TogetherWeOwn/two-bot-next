#![cfg(feature = "db")]

//! Companion regression: both resumes on one giveaway must retain their actors.
//! Removing ONLY giveaway.resume from the recurring audit-ID suffix arms must
//! fail this case, even if pause and extend retain their unique suffixes.
//! Run with the documented disposable database bootstrap:
//! python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
//! --test giveaway_repeat_resume_acceptance -- --test-threads=1

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{types::time::OffsetDateTime, PgPool};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_GUILD, giveaway_store as store, ClassifierConfig, InteractionRouter,
    RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060901";
const CHANNEL: &str = "1550102030405060902";
const TARGET: &str = "gw_repeat_resume";
const CREATOR: &str = "1550102030405060910";
const FIRST_ACTOR: &str = "1550102030405060911";
const SECOND_ACTOR: &str = "1550102030405060912";

fn command(sequence: u64, actor: &str, action: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": sequence.to_string(), "token": "repeat-resume-fixture-token", "type": 2,
        "version": 1, "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": PERM_MANAGE_GUILD.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": actor, "username": "staff", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1,
            "options": [{"name": action, "type": 1,
                "options": [{"name": "id", "type": 3, "value": TARGET}]}]}
    }))
    .expect("authenticated manage-guild interaction")
}

async fn facts(pool: &PgPool) -> Vec<(Value, String)> {
    sqlx::query_as("SELECT to_jsonb(a), a.xmin::text FROM automation_audit_log a WHERE guild_id = $1 AND target_key = $2 ORDER BY id")
        .bind(GUILD).bind(TARGET).fetch_all(pool).await.expect("read same-target audit facts")
}

#[tokio::test]
async fn two_authenticated_resume_cycles_on_one_target_commit_distinct_actor_facts() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable database");
    store::create_giveaway(
        db.pool(),
        &store::NewGiveaway {
            guild_id: GUILD.into(),
            id: TARGET.into(),
            prize: "repeat-resume-private-prize".into(),
            channel_id: CHANNEL.into(),
            message_id: "1550102030405060903".into(),
            created_by: CREATOR.into(),
            winners_count: 1,
            ends_at: OffsetDateTime::from_unix_timestamp(1_900_000_000).expect("fixed deadline"),
            requirements_json: json!({"roles": [], "min_account_age_days": 0}),
            max_entries: 500,
            draw_nonce: "repeat-resume-private-nonce".into(),
            created_at: OffsetDateTime::from_unix_timestamp(1_760_000_000).expect("fixed creation"),
        },
    )
    .await
    .expect("seed one giveaway, never replace it between cycles");
    let baseline = facts(db.pool()).await;
    let mock = MockRest::with_responder(|request: &RestRequest| {
        if request.path.contains("/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::json(200, json!({"id": "1550102030405060903"}))
        }
    })
    .await;
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().expect("guild snowflake")),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    });
    let runtime = InteractionRuntime::with_router(
        router,
        db.pool().clone(),
        ActionExecutor::with_proxy("mock-token-not-a-credential".into(), Some(mock.origin()))
            .expect("loopback Discord transport only"),
        0,
        ClassifierConfig::default(),
    );
    let mut observations = Vec::new();
    for (sequence, actor, action) in [
        (9100, FIRST_ACTOR, "pause"),
        (9101, FIRST_ACTOR, "resume"),
        (9102, SECOND_ACTOR, "pause"),
        (9103, SECOND_ACTOR, "resume"),
    ] {
        let handled = runtime
            .handle(&command(sequence, actor, action))
            .await
            .expect("dispatch lifecycle command");
        let row = store::get_giveaway(db.pool(), GUILD, TARGET)
            .await
            .expect("read lifecycle")
            .expect("same target still exists");
        let transaction: String =
            sqlx::query_scalar("SELECT xmin::text FROM giveaways WHERE guild_id = $1 AND id = $2")
                .bind(GUILD)
                .bind(TARGET)
                .fetch_one(db.pool())
                .await
                .expect("read mutation transaction");
        observations.push((handled, row, facts(db.pool()).await, transaction));
    }
    let replies: Vec<String> = mock
        .requests()
        .iter()
        .filter(|request| request.path.contains("/webhooks/"))
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter_map(|body| body["content"].as_str().map(str::to_owned))
        .collect();
    db.close().await.expect("drop owned disposable database");
    mock.shutdown().await;

    assert_eq!(observations.len(), 4);
    for (index, (handled, row, _, _)) in observations.iter().enumerate() {
        assert!(
            *handled,
            "router must handle all four authenticated commands"
        );
        assert_eq!(row.id, TARGET);
        assert_eq!(
            row.status,
            if index % 2 == 0 { "paused" } else { "open" },
            "each cycle must really transition"
        );
        assert_eq!(row.paused_at.is_some(), index % 2 == 0);
    }
    assert_eq!(replies.len(), 4, "one completion per command");
    for (index, reply) in replies.iter().enumerate() {
        assert!(
            reply.contains(if index % 2 == 0 {
                "Giveaway paused"
            } else {
                "Giveaway resumed"
            }),
            "both resumes must report success, not silently refuse"
        );
    }

    let final_facts = &observations[3].2;
    let added: Vec<_> = final_facts
        .iter()
        .filter(|(fact, _)| {
            !baseline
                .iter()
                .any(|(before, _)| before["id"] == fact["id"])
        })
        .collect();
    assert_eq!(
        added.len(),
        4,
        "both complete pause/resume cycles need four success facts"
    );
    let resumed: Vec<_> = added
        .iter()
        .filter(|(fact, _)| fact["action"] == "giveaway.resume")
        .collect();
    assert_eq!(
        resumed.len(),
        2,
        "a second successful resume must not collide with the first fact ID"
    );
    assert_ne!(
        resumed[0].0["id"], resumed[1].0["id"],
        "distinct resume fact IDs on one stored target"
    );
    for (index, actor, expected_resume_count) in [(1, FIRST_ACTOR, 1), (3, SECOND_ACTOR, 2)] {
        let (_, _, audit_at_resume, transaction) = &observations[index];
        let matching: Vec<_> = audit_at_resume
            .iter()
            .filter(|(fact, _)| fact["action"] == "giveaway.resume")
            .collect();
        assert_eq!(matching.len(), expected_resume_count);
        let actor_facts: Vec<_> = matching
            .into_iter()
            .filter(|(fact, _)| fact["actor_id"] == actor)
            .collect();
        assert_eq!(actor_facts.len(), 1, "each resume retains its authenticated actor, not the creator or the other staff member");
        let (fact, audit_transaction) = actor_facts[0];
        assert_eq!(fact["guild_id"], GUILD);
        assert_eq!(fact["target_key"], TARGET);
        assert_eq!(fact["outcome"], "ok");
        assert_eq!(fact["reason"], Value::Null);
        assert!(fact["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(
            audit_transaction, transaction,
            "each actor fact shares its own resume transaction"
        );
    }
    assert_ne!(
        observations[1].3, observations[3].3,
        "the two successful resumes are independent committed mutations"
    );
    assert_eq!(observations[3].1.status, "open");
}
