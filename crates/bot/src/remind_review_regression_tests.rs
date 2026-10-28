//! Regression coverage for reminder admission, requested deadlines and truthful failures.

use std::sync::Arc;

use twilight_model::application::interaction::application_command::CommandOptionValue;
use twilight_model::application::interaction::Interaction;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use two_bot_core::{remind_store, scheduled::parse_iso_ms, scheduled_store, RouterGates};
use two_bot_testsupport::TestDatabase;

use super::{
    executor_at, from_member, option, slash, MockRest, CHANNEL, CHANNEL_S, GUILD, GUILD_S,
};
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::scheduled_jobs::tick;

const OWNER: u64 = 881_001_001_001;
const DENIED: &str = "You need permission to send messages in this channel to create a reminder.";
const FAILED: &str = "Reminder command failed; try again.";

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI requires test DB"
        );
        eprintln!("SKIP reminder regressions: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable database"),
    )
}

fn command(
    id: u64,
    name: &str,
    options: Vec<twilight_model::application::interaction::application_command::CommandDataOption>,
) -> Interaction {
    let mut interaction = from_member(
        slash(
            "remind",
            Some(CHANNEL),
            vec![option(name, CommandOptionValue::SubCommand(options))],
        ),
        OWNER,
        Permissions::VIEW_CHANNEL | Permissions::USE_SLASH_COMMANDS | Permissions::SEND_MESSAGES,
    );
    interaction.id = Id::new(id);
    interaction
}

fn create(id: u64, minutes: i64) -> Interaction {
    command(
        id,
        "me",
        vec![
            option("in", CommandOptionValue::Integer(minutes)),
            option(
                "text",
                CommandOptionValue::String(format!("deadline-{minutes}")),
            ),
        ],
    )
}

fn runtime(pool: sqlx::PgPool, origin: &str) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        pool,
        executor_at(origin.to_owned()),
        router_with_commands(RouterGates {
            configured_guild: Some(GUILD),
            automations: true,
            scorecard: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        GUILD,
        true,
    )
}

async fn invoke(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    interaction: &Interaction,
) -> String {
    let before = mock.requests().len();
    runtime.on_interaction(interaction).await;
    let fresh: Vec<_> = mock.requests().into_iter().skip(before).collect();
    let callbacks: Vec<_> = fresh
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one acknowledgement");
    let ack: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(ack["data"]["flags"], 64, "private acknowledgement");
    let edits: Vec<_> = fresh.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 1, "one private completion");
    let edit: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert_eq!(edit["allowed_mentions"]["parse"], serde_json::json!([]));
    edit["content"].as_str().unwrap().to_owned()
}

fn posts(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|r| {
            r.method == "POST" && r.path == format!("/api/v10/channels/{CHANNEL_S}/messages")
        })
        .count()
}

#[tokio::test]
async fn missing_channel_post_authority_leaves_no_rows_or_eventual_posts() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(db.pool().clone(), &origin);
    let usable = Permissions::VIEW_CHANNEL | Permissions::USE_SLASH_COMMANDS;
    for (index, (kind, permissions)) in [
        (0, Some(usable)),
        (0, None),
        (0, Some(Permissions::SEND_MESSAGES)),
        (10, Some(usable | Permissions::SEND_MESSAGES)),
        (11, Some(usable | Permissions::SEND_MESSAGES)),
        (12, Some(usable | Permissions::SEND_MESSAGES)),
        (11, None),
        (4, Some(Permissions::all())),
    ]
    .into_iter()
    .enumerate()
    {
        let mut interaction = create(100 + index as u64, 30);
        interaction.member.as_mut().unwrap().permissions = permissions;
        interaction.channel = Some(
            serde_json::from_value(serde_json::json!({
                "id": CHANNEL_S, "guild_id": GUILD_S, "type": kind
            }))
            .unwrap(),
        );
        assert_eq!(
            invoke(&runtime, &mock, &interaction).await,
            DENIED,
            "channel kind {kind} must refuse without its posting authority"
        );
    }
    for (index, malformed) in ["missing", "foreign-guild", "unknown-kind"]
        .into_iter()
        .enumerate()
    {
        let mut interaction = create(200 + index as u64, 30);
        match malformed {
            "missing" => interaction.channel = None,
            "foreign-guild" => interaction.channel.as_mut().unwrap().guild_id = Some(Id::new(9999)),
            _ => {
                interaction.channel.as_mut().unwrap().kind =
                    twilight_model::channel::ChannelType::Unknown(255)
            }
        }
        assert_eq!(invoke(&runtime, &mock, &interaction).await, DENIED);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduled_messages")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0, "refused creates cannot even leave tombstones");
    tick(db.pool(), &executor_at(origin), GUILD_S, || {
        two_bot_core::funnel::now_millis_for_test().max(0) as u64 + 120 * 60_000
    })
    .await
    .unwrap();
    assert_eq!(
        posts(&mock),
        0,
        "no later bot post bypasses member permissions"
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn permitted_regular_and_thread_members_can_create_and_manage_without_staff_permissions() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(db.pool().clone(), &origin);
    for (index, (kind, permission)) in [
        (0, Permissions::SEND_MESSAGES),
        (10, Permissions::SEND_MESSAGES_IN_THREADS),
        (11, Permissions::SEND_MESSAGES_IN_THREADS),
        (12, Permissions::SEND_MESSAGES_IN_THREADS),
    ]
    .into_iter()
    .enumerate()
    {
        let mut interaction = create(300 + index as u64, 30);
        interaction.member.as_mut().unwrap().permissions =
            Some(Permissions::VIEW_CHANNEL | permission);
        interaction.channel = Some(
            serde_json::from_value(
                serde_json::json!({"id": CHANNEL_S, "guild_id": GUILD_S, "type": kind}),
            )
            .unwrap(),
        );
        if index == 3 {
            // Discord may omit guild_id from the interaction channel object;
            // the authenticated interaction's top-level guild still fences it.
            interaction.channel.as_mut().unwrap().guild_id = None;
        }
        assert!(invoke(&runtime, &mock, &interaction)
            .await
            .starts_with("Reminder `"));
    }
    let pending = remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
        .await
        .unwrap();
    assert_eq!(pending.len(), 4);
    let mut list = command(400, "list", vec![]);
    list.member.as_mut().unwrap().permissions = None;
    let reply = invoke(&runtime, &mock, &list).await;
    for row in &pending {
        assert!(reply.contains(&row.id));
    }
    let mut cancel = command(
        401,
        "cancel",
        vec![option(
            "target",
            CommandOptionValue::String(pending[0].id.clone()),
        )],
    );
    cancel.member.as_mut().unwrap().permissions = Some(Permissions::empty());
    assert_eq!(
        invoke(&runtime, &mock, &cancel).await,
        "Reminder cancelled."
    );
    assert_eq!(
        remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
            .await
            .unwrap()
            .len(),
        3
    );
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn requested_minutes_are_persisted_and_delivered_at_the_exact_deadline() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(db.pool().clone(), &origin);
    for (index, minutes) in [30, 60].into_iter().enumerate() {
        assert!(
            invoke(&runtime, &mock, &create(500 + index as u64, minutes))
                .await
                .starts_with("Reminder `")
        );
    }
    let mut rows: Vec<scheduled_store::ScheduledMessageRow> =
        sqlx::query_as("SELECT * FROM scheduled_messages ORDER BY next_run_at")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    for (index, (row, minutes)) in rows.drain(..).zip([30, 60]).enumerate() {
        let created = parse_iso_ms(&row.created_at).unwrap();
        let due = parse_iso_ms(&row.next_run_at).unwrap();
        assert_eq!(
            due,
            created + minutes * 60_000,
            "requested delay survives runtime wiring"
        );
        tick(db.pool(), &executor_at(origin.clone()), GUILD_S, || due - 1)
            .await
            .unwrap();
        assert_eq!(posts(&mock), index, "no early post");
        tick(db.pool(), &executor_at(origin.clone()), GUILD_S, || due)
            .await
            .unwrap();
        assert_eq!(posts(&mock), index + 1, "exactly one post at deadline");
        tick(db.pool(), &executor_at(origin.clone()), GUILD_S, || due + 1)
            .await
            .unwrap();
        assert_eq!(posts(&mock), index + 1, "no repeat");
    }
    mock.shutdown().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn store_failures_are_not_reported_as_empty_lists_or_successful_mutations() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let healthy = runtime(db.pool().clone(), &origin);
    assert!(invoke(&healthy, &mock, &create(600, 30))
        .await
        .starts_with("Reminder `"));
    let before: Vec<scheduled_store::ScheduledMessageRow> =
        sqlx::query_as("SELECT * FROM scheduled_messages ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let broken = db.independent_pool().await.unwrap();
    broken.close().await;
    let unavailable = runtime(broken, &origin);
    for interaction in [
        create(601, 60),
        command(602, "list", vec![]),
        command(
            603,
            "cancel",
            vec![option(
                "target",
                CommandOptionValue::String(before[0].id.clone()),
            )],
        ),
    ] {
        assert_eq!(invoke(&unavailable, &mock, &interaction).await, FAILED);
        let after: Vec<scheduled_store::ScheduledMessageRow> =
            sqlx::query_as("SELECT * FROM scheduled_messages ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert!(
            before == after,
            "failed operation leaves the complete row unchanged"
        );
    }
    assert_eq!(posts(&mock), 0);
    mock.shutdown().await;
    db.close().await.unwrap();
}
