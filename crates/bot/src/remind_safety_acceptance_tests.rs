//! Protected supplemental `/remind` safety acceptance, authored from the handoff
//! contract without reading the implementation diff. The original acceptance
//! remains immutable. Breaks caught: stateful malformed-option refusals, replay
//! resurrection, foreign cancellation, staff CRUD crossing the namespace, lost
//! ids in long/held lists, duplicate concurrent delivery and armed-fence replay.
//! No live Discord; only the guarded disposable database and recording REST.
#![cfg(test)]

use serde_json::Value;
use std::sync::Arc;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::Interaction;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use two_bot_core::{message_safety::text_len, remind_store, scheduled_store, RouterGates};
use two_bot_testsupport::TestDatabase;

use super::{
    executor_at, from_member, option, slash, MockRest, CHANNEL, CHANNEL_S, GUILD, GUILD_S,
};
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::scheduled_jobs::tick;

const OWNER: u64 = 881_001_001_001;
const OTHER: u64 = 882_002_002_002;

fn gates() -> RouterGates {
    RouterGates {
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
    }
}

fn command(
    member: u64,
    invocation: u64,
    name: &str,
    options: Vec<CommandDataOption>,
) -> Interaction {
    let mut interaction = from_member(
        slash(
            "remind",
            Some(CHANNEL),
            vec![option(name, CommandOptionValue::SubCommand(options))],
        ),
        member,
        if name == "me" {
            Permissions::VIEW_CHANNEL | Permissions::USE_SLASH_COMMANDS | Permissions::SEND_MESSAGES
        } else {
            Permissions::empty()
        },
    );
    // Test-author fixture correction: successful creates have real posting authority.
    interaction.channel = Some(
        serde_json::from_value(serde_json::json!({"id": CHANNEL_S, "type": 0}))
            .expect("resolved ordinary text channel"),
    );
    interaction.id = Id::new(invocation);
    interaction
}

fn create(invocation: u64, text: &str) -> Interaction {
    command(
        OWNER,
        invocation,
        "me",
        vec![
            option("in", CommandOptionValue::Integer(30)),
            option("text", CommandOptionValue::String(text.to_owned())),
        ],
    )
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI requires guarded test DB"
        );
        eprintln!("SKIP reminder safety: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable DB"),
    )
}

async fn runtime(db: &TestDatabase, origin: &str) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        db.pool().clone(),
        executor_at(origin.to_owned()),
        router_with_commands(gates()),
        GUILD,
        true,
    )
}

async fn invoke(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    interaction: &Interaction,
) -> Vec<String> {
    let before = mock.requests().len();
    runtime.on_interaction(interaction).await;
    let fresh: Vec<_> = mock.requests().into_iter().skip(before).collect();
    let callbacks: Vec<_> = fresh
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one visible ephemeral acknowledgement");
    let acknowledgement: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(
        acknowledgement["data"]["flags"].as_u64().expect("flags") & 64,
        64
    );
    let texts: Vec<String> = fresh
        .iter()
        .filter_map(|r| {
            let json: Value = serde_json::from_slice(&r.body).ok()?;
            json.pointer("/data/content")
                .or_else(|| json.get("content"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    assert!(!texts.is_empty(), "silence is not refusal/completion");
    assert!(
        texts.iter().all(|text| text_len(text) <= 2000),
        "each reply fits Discord UTF-16 ceiling"
    );
    texts
}

async fn ids(db: &TestDatabase) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM scheduled_messages WHERE guild_id = $1 ORDER BY id")
        .bind(GUILD_S)
        .fetch_all(db.pool())
        .await
        .expect("scoped stored ids")
}

fn post_count(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|r| {
            r.method == "POST" && r.path == format!("/api/v10/channels/{CHANNEL_S}/messages")
        })
        .count()
}

#[tokio::test]
async fn malformed_options_and_invalid_targets_have_zero_state_effects() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    let integer = || option("in", CommandOptionValue::Integer(30));
    let text = || option("text", CommandOptionValue::String("valid text".to_owned()));
    let mut cases = vec![
        command(OWNER, 1, "me", vec![]),
        command(OWNER, 2, "me", vec![integer()]),
        command(OWNER, 3, "me", vec![text()]),
        command(
            OWNER,
            4,
            "me",
            vec![
                option("in", CommandOptionValue::String("30".to_owned())),
                text(),
            ],
        ),
        command(
            OWNER,
            5,
            "me",
            vec![integer(), option("text", CommandOptionValue::Integer(30))],
        ),
        command(OWNER, 6, "me", vec![integer(), integer(), text()]),
        command(OWNER, 7, "me", vec![integer(), text(), text()]),
        command(
            OWNER,
            8,
            "me",
            vec![
                integer(),
                text(),
                option("extra", CommandOptionValue::Boolean(true)),
            ],
        ),
        command(OWNER, 9, "cancel", vec![]),
        command(
            OWNER,
            10,
            "cancel",
            vec![option("target", CommandOptionValue::Integer(1))],
        ),
    ];
    for (index, (minutes, body)) in [
        (0, "valid".to_owned()),
        (-1, "valid".to_owned()),
        (525_601, "valid".to_owned()),
        (30, "".to_owned()),
        (30, " ".to_owned()),
        (30, "😀".repeat(121)),
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(command(
            OWNER,
            20 + index as u64,
            "me",
            vec![
                option("in", CommandOptionValue::Integer(minutes)),
                option("text", CommandOptionValue::String(body)),
            ],
        ));
    }
    for (index, target) in ["".to_owned(), " ".to_owned(), "x".repeat(241)]
        .into_iter()
        .enumerate()
    {
        cases.push(command(
            OWNER,
            40 + index as u64,
            "cancel",
            vec![option("target", CommandOptionValue::String(target))],
        ));
    }
    for interaction in cases {
        invoke(&runtime, &mock, &interaction).await;
        assert!(
            ids(&db).await.is_empty(),
            "invalid invocation {} must insert zero rows",
            interaction.id
        );
    }
    assert_eq!(post_count(&mock), 0);
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn known_ids_and_text_do_not_let_another_member_cancel() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    let text = "owner-only dentist";
    invoke(&runtime, &mock, &create(101, text)).await;
    let stored = ids(&db).await;
    assert_eq!(stored.len(), 1);
    for (index, target) in [&stored[0], text].into_iter().enumerate() {
        let cancel = command(
            OTHER,
            110 + index as u64,
            "cancel",
            vec![option(
                "target",
                CommandOptionValue::String(target.to_owned()),
            )],
        );
        invoke(&runtime, &mock, &cancel).await;
        assert_eq!(
            remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
                .await
                .expect("owner pending")
                .len(),
            1
        );
    }
    let listed = invoke(&runtime, &mock, &command(OTHER, 115, "list", vec![]))
        .await
        .join("\n");
    assert!(
        !listed.contains(&stored[0]) && !listed.contains(text),
        "foreign list reveals neither id nor text"
    );
    invoke(
        &runtime,
        &mock,
        &command(
            OWNER,
            116,
            "cancel",
            vec![option(
                "target",
                CommandOptionValue::String(stored[0].clone()),
            )],
        ),
    )
    .await;
    assert!(remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
        .await
        .expect("owner cancel")
        .is_empty());
    assert_eq!(ids(&db).await, stored, "cancel retains identity tombstone");
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn duplicate_dispatch_never_reenables_a_cancelled_or_completed_invocation() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    let first = create(201, "cancelled invocation");
    for _ in 0..2 {
        invoke(&runtime, &mock, &first).await;
    }
    let first_id = ids(&db).await;
    assert_eq!(first_id.len(), 1);
    invoke(
        &runtime,
        &mock,
        &command(
            OWNER,
            202,
            "cancel",
            vec![option(
                "target",
                CommandOptionValue::String(first_id[0].clone()),
            )],
        ),
    )
    .await;
    invoke(&runtime, &mock, &first).await;
    assert!(remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
        .await
        .expect("cancel replay")
        .is_empty());
    let second = create(203, "completed invocation");
    invoke(&runtime, &mock, &second).await;
    let due = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
        + 120 * 60_000;
    let executor = executor_at(origin);
    tick(db.pool(), &executor, GUILD_S, || due)
        .await
        .expect("complete second");
    assert_eq!(post_count(&mock), 1);
    invoke(&runtime, &mock, &second).await;
    tick(db.pool(), &executor, GUILD_S, || due + 3_600_000)
        .await
        .expect("replay tick");
    assert_eq!(
        post_count(&mock),
        1,
        "neither cancelled nor completed invocation posts again"
    );
    assert_eq!(ids(&db).await.len(), 2);
    assert!(remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
        .await
        .expect("no pending replay")
        .is_empty());
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn five_maximum_safe_utf16_bodies_list_every_id_even_when_delivery_is_held() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    // 238 input units; neutralizing the two mass mentions yields exactly 240.
    let body = format!("{}@everyone @here ab", "😀".repeat(110));
    for invocation in 301..306 {
        invoke(&runtime, &mock, &create(invocation, &body)).await;
    }
    let stored = ids(&db).await;
    assert_eq!(stored.len(), 5, "all five maximum-length bodies admitted");
    let bodies: Vec<String> =
        sqlx::query_scalar("SELECT body FROM scheduled_messages WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_all(db.pool())
            .await
            .expect("stored bodies");
    for text in bodies {
        assert!(text.contains('😀'));
        assert!(!text.contains("@everyone") && !text.contains("@here"));
    }
    for held in [false, true] {
        if held {
            sqlx::query(
                "UPDATE scheduled_messages SET delivery_uncertain = TRUE WHERE guild_id = $1",
            )
            .bind(GUILD_S)
            .execute(db.pool())
            .await
            .expect("fixture all held");
        }
        let texts = invoke(
            &runtime,
            &mock,
            &command(OWNER, 310 + u64::from(held), "list", vec![]),
        )
        .await;
        let joined = texts.join("\n");
        for id in &stored {
            assert!(
                joined.contains(id),
                "every id survives bounded list rendering: {id}"
            );
        }
        assert!(
            text_len(&joined) <= 2000,
            "five reminders need one bounded list"
        );
        assert!(!joined.contains("@everyone") && !joined.contains("@here"));
        if held {
            assert!(
                joined.to_lowercase().contains("held"),
                "uncertain delivery is labelled, not promised"
            );
        }
    }
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn staff_schedule_create_list_and_remove_never_cross_the_personal_namespace() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    invoke(&runtime, &mock, &create(401, "private reminder sentinel")).await;
    let personal = ids(&db).await[0].clone();
    let listed = invoke(
        &runtime,
        &mock,
        &slash("schedule-list", Some(CHANNEL), vec![]),
    )
    .await
    .join("\n");
    assert!(!listed.contains(&personal) && !listed.contains("private reminder sentinel"));
    for target in [personal.as_str(), "remind-"] {
        invoke(
            &runtime,
            &mock,
            &slash(
                "schedule-remove",
                Some(CHANNEL),
                vec![option("id", CommandOptionValue::String(target.to_owned()))],
            ),
        )
        .await;
        assert_eq!(
            remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
                .await
                .expect("personal preserved")
                .len(),
            1
        );
    }
    for index in 0..25 {
        let write = scheduled_store::ScheduledWrite {
            id: format!("staff-limit-{index:02}"),
            guild_id: GUILD_S.to_owned(),
            channel_id: CHANNEL_S.to_owned(),
            body: "staff schedule".to_owned(),
            next_run_at: "2099-01-01T00:00:00.000Z".to_owned(),
            interval_seconds: None,
            enabled: true,
            created_by: OWNER.to_string(),
            created_at: "2026-10-01T00:00:00.000Z".to_owned(),
            updated_by: OWNER.to_string(),
            updated_at: "2026-10-01T00:00:00.000Z".to_owned(),
        };
        assert!(scheduled_store::put_scheduled(db.pool(), &write)
            .await
            .expect("staff quota excludes personal"));
    }
    invoke(
        &runtime,
        &mock,
        &slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option(
                    "body",
                    CommandOptionValue::String("over staff quota".to_owned()),
                ),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ),
    )
    .await;
    assert_eq!(
        scheduled_store::count_scheduled(db.pool(), GUILD_S)
            .await
            .expect("staff count"),
        25
    );
    assert_eq!(ids(&db).await.len(), 26);
    assert_eq!(
        remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
            .await
            .expect("reminder intact")
            .len(),
        1
    );
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn independent_reminder_tickers_deliver_one_post() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    invoke(&runtime, &mock, &create(501, "contended reminder")).await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent ticker pool");
    let first = executor_at(origin.clone());
    let second = executor_at(origin);
    let due = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
        + 120 * 60_000;
    let (a, b) = tokio::join!(
        tick(db.pool(), &first, GUILD_S, || due),
        tick(&peer, &second, GUILD_S, || due)
    );
    a.expect("first ticker");
    b.expect("second ticker");
    assert_eq!(post_count(&mock), 1, "one reminder occurrence, one effect");
    assert!(remind_store::list(&peer, GUILD_S, &OWNER.to_string())
        .await
        .expect("completed")
        .is_empty());
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

#[tokio::test]
async fn restart_from_armed_before_send_state_holds_without_any_post_beyond_nonce_window() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    invoke(
        &runtime,
        &mock,
        &create(601, "armed before any remote effect"),
    )
    .await;
    let id = ids(&db).await[0].clone();
    let before = "2098-01-01T00:00:00.000Z";
    let lease = "2098-01-01T00:02:00.000Z";
    assert_eq!(
        scheduled_store::claim_due(
            db.pool(),
            GUILD_S,
            before,
            "crashed-owner",
            lease,
            "armed-nonce"
        )
        .await
        .expect("precrash claim")
        .len(),
        1
    );
    assert!(
        scheduled_store::begin_scheduled_delivery(db.pool(), GUILD_S, &id, "crashed-owner")
            .await
            .expect("durable pre-send arm")
    );
    drop(runtime);
    let peer = db.independent_pool().await.expect("fresh restart pool");
    let executor = executor_at(origin);
    let restart =
        two_bot_core::scheduled::parse_iso_ms("2098-01-01T01:00:00.000Z").expect("restart time");
    for time in [restart, restart + 120_000] {
        tick(&peer, &executor, GUILD_S, || time)
            .await
            .expect("restart tick");
    }
    assert_eq!(
        post_count(&mock),
        0,
        "durable armed state cannot prove unsent, even when this fixture never posted"
    );
    let retained = scheduled_store::get_scheduled(&peer, GUILD_S, &id)
        .await
        .expect("read")
        .expect("held row");
    assert!(retained.enabled && retained.delivery_uncertain);
    assert!(retained.last_message_id.is_none());
    assert_eq!(retained.occurrence_nonce.as_deref(), Some("armed-nonce"));
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

// Review regressions: permission bypass, hard-coded delay, and success-shaped
// store errors. Expectations come from the command contract, not its handler.
const STORE_FAILURE: &str = "Reminder command failed; try again.";

fn readable_command_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL | Permissions::USE_SLASH_COMMANDS
}

fn resolved_channel(interaction: &mut Interaction, kind: u8) {
    interaction.channel = Some(
        serde_json::from_value(serde_json::json!({"id": CHANNEL_S, "type": kind}))
            .expect("resolved invoking channel"),
    );
}

async fn stored_snapshot(db: &TestDatabase) -> Vec<Value> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(s)::text FROM scheduled_messages s ORDER BY guild_id, id",
    )
    .fetch_all(db.pool())
    .await
    .expect("all persisted columns, including delivery fences");
    rows.into_iter()
        .map(|row| serde_json::from_str(&row).expect("stored row JSON"))
        .collect()
}

async fn posting_authority_case(kind: Option<u8>, permissions: Option<Permissions>, allowed: bool) {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    let mut interaction = create(701, "posting authority sentinel");
    if let Some(kind) = kind {
        resolved_channel(&mut interaction, kind);
    } else {
        interaction.channel = None;
    }
    interaction.member.as_mut().expect("requester").permissions = permissions;
    // Bot posting authority must never substitute for the requester's authority.
    interaction.app_permissions = Some(
        readable_command_permissions()
            | Permissions::SEND_MESSAGES
            | Permissions::SEND_MESSAGES_IN_THREADS,
    );
    let texts = invoke(&runtime, &mock, &interaction).await;
    let stored = stored_snapshot(&db).await;
    let immediate = post_count(&mock);
    let executor = executor_at(origin);
    let due = two_bot_core::scheduled::parse_iso_ms("2098-01-01T00:00:00.000Z")
        .expect("injected future clock");
    tick(db.pool(), &executor, GUILD_S, || due)
        .await
        .expect("tick after admission decision");
    let posts = post_count(&mock);
    mock.shutdown().await;
    db.close()
        .await
        .expect("drop disposable DB before RED assertion");

    assert_eq!(immediate, 0, "create never posts immediately");
    assert_eq!(
        stored.len(),
        usize::from(allowed),
        "requester posting authority must be checked before inserting any row; kind={kind:?}, permissions={permissions:?}, replies={texts:?}, posts_after_tick={posts}"
    );
    assert_eq!(
        posts,
        usize::from(allowed),
        "a refused create never becomes deliverable"
    );
    assert!(
        texts.iter().all(|text| text != STORE_FAILURE),
        "permission decisions are not database failures"
    );
}

#[tokio::test]
async fn create_refuses_readable_command_accessible_but_read_only_text_channel() {
    posting_authority_case(Some(0), Some(readable_command_permissions()), false).await;
}

#[tokio::test]
async fn create_refuses_public_thread_with_only_ordinary_send_permission() {
    posting_authority_case(
        Some(11),
        Some(readable_command_permissions() | Permissions::SEND_MESSAGES),
        false,
    )
    .await;
}

#[tokio::test]
async fn create_refuses_private_thread_with_only_ordinary_send_permission() {
    posting_authority_case(
        Some(12),
        Some(readable_command_permissions() | Permissions::SEND_MESSAGES),
        false,
    )
    .await;
}

#[tokio::test]
async fn create_refuses_text_channel_with_only_thread_send_permission() {
    posting_authority_case(
        Some(0),
        Some(readable_command_permissions() | Permissions::SEND_MESSAGES_IN_THREADS),
        false,
    )
    .await;
}

#[tokio::test]
async fn create_refuses_missing_resolved_requester_permissions() {
    posting_authority_case(Some(0), None, false).await;
}

#[tokio::test]
async fn create_refuses_missing_resolved_channel_metadata() {
    posting_authority_case(
        None,
        Some(readable_command_permissions() | Permissions::SEND_MESSAGES),
        false,
    )
    .await;
}

#[tokio::test]
async fn create_accepts_ordinary_send_without_thread_send_permission() {
    posting_authority_case(
        Some(0),
        Some(readable_command_permissions() | Permissions::SEND_MESSAGES),
        true,
    )
    .await;
}

#[tokio::test]
async fn create_accepts_thread_send_without_ordinary_send_permission() {
    for kind in [10, 11, 12] {
        posting_authority_case(
            Some(kind),
            Some(readable_command_permissions() | Permissions::SEND_MESSAGES_IN_THREADS),
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn create_accepts_discord_administrator_in_text_and_thread_channels() {
    for kind in [0, 10, 11, 12] {
        posting_authority_case(Some(kind), Some(Permissions::ADMINISTRATOR), true).await;
    }
}

#[tokio::test]
async fn list_and_cancel_remain_available_without_resolved_posting_permissions() {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    for (index, permissions) in [Some(Permissions::empty()), None].into_iter().enumerate() {
        let invocation = 720 + 10 * index as u64;
        let text = format!("permissionless cancellation {index}");
        invoke(&runtime, &mock, &create(invocation, &text)).await;
        let mut list = command(OWNER, invocation + 1, "list", vec![]);
        list.member.as_mut().expect("requester").permissions = permissions;
        let listed = invoke(&runtime, &mock, &list).await;
        assert!(
            listed.iter().any(|reply| reply.contains(&text)),
            "lost posting authority must not hide existing reminders"
        );
        let mut cancel = command(
            OWNER,
            invocation + 2,
            "cancel",
            vec![option("target", CommandOptionValue::String(text))],
        );
        cancel.member.as_mut().expect("requester").permissions = permissions;
        invoke(&runtime, &mock, &cancel).await;
        assert!(remind_store::list(db.pool(), GUILD_S, &OWNER.to_string())
            .await
            .expect("pending after permissionless cancel")
            .is_empty());
    }
    let retained = stored_snapshot(&db).await;
    assert_eq!(
        retained.len(),
        2,
        "both cancellations retain replay tombstones"
    );
    assert!(retained.iter().all(|row| row["enabled"] == false));
    let executor = executor_at(origin);
    tick(db.pool(), &executor, GUILD_S, || 4_039_372_800_000)
        .await
        .expect("post-cancel tick");
    assert_eq!(post_count(&mock), 0, "cancelled reminders never post");
    mock.shutdown().await;
    db.close().await.expect("drop disposable DB");
}

async fn exact_minute_delay(minutes: i64, expected_delta_ms: u64) {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime(&db, &origin).await;
    let interaction = command(
        OWNER,
        751,
        "me",
        vec![
            option("in", CommandOptionValue::Integer(minutes)),
            option(
                "text",
                CommandOptionValue::String("exact due sentinel".to_owned()),
            ),
        ],
    );
    invoke(&runtime, &mock, &interaction).await;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT created_at, next_run_at FROM scheduled_messages WHERE guild_id = $1",
    )
    .bind(GUILD_S)
    .fetch_all(db.pool())
    .await
    .expect("runtime-persisted schedule");
    assert_eq!(rows.len(), 1, "non-vacuous runtime admission");
    let created = two_bot_core::scheduled::parse_iso_ms(&rows[0].0).expect("created_at");
    let due = two_bot_core::scheduled::parse_iso_ms(&rows[0].1).expect("next_run_at");
    let immediate = post_count(&mock);
    let executor = executor_at(origin);
    tick(db.pool(), &executor, GUILD_S, || due - 1)
        .await
        .expect("one millisecond before actual due");
    let before_due = post_count(&mock);
    tick(db.pool(), &executor, GUILD_S, || due)
        .await
        .expect("exact persisted due");
    let at_due = post_count(&mock);
    tick(db.pool(), &executor, GUILD_S, || due + 1)
        .await
        .expect("one millisecond after actual due");
    let after_due = post_count(&mock);
    mock.shutdown().await;
    db.close()
        .await
        .expect("drop disposable DB before assertions");
    assert_eq!(
        due.checked_sub(created),
        Some(expected_delta_ms),
        "requested {minutes} minutes must be persisted exactly, not a constant delay"
    );
    assert_eq!(
        (immediate, before_due, at_due, after_due),
        (0, 0, 1, 1),
        "delivery is due-inclusive and exactly once"
    );
}

#[tokio::test]
async fn thirty_minutes_is_persisted_and_fires_only_at_the_actual_due_boundary() {
    exact_minute_delay(30, 1_800_000).await;
}

#[tokio::test]
async fn sixty_minutes_is_persisted_and_fires_only_at_the_actual_due_boundary() {
    exact_minute_delay(60, 3_600_000).await;
}

async fn runtime_store_failure(operation: &str) {
    let Some(db) = fixture().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let healthy = runtime(&db, &origin).await;
    let sentinel = "healthy reminder survives store failure";
    invoke(&healthy, &mock, &create(801, sentinel)).await;
    let before = stored_snapshot(&db).await;
    assert_eq!(before.len(), 1, "seeded healthy reminder");

    let mut closed_pool = None;
    let interaction = match operation {
        "create" | "cancel" => {
            // Only the database owned by TestDatabase is faulted. The failure
            // occurs at the real INSERT/UPDATE, after normal runtime admission.
            sqlx::query(
                "CREATE FUNCTION reminder_acceptance_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture reminder store failure'; END; $$",
            )
            .execute(db.pool())
            .await
            .expect("install isolated failure function");
            let trigger = if operation == "create" {
                "CREATE TRIGGER reminder_acceptance_failure BEFORE INSERT ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION reminder_acceptance_fail()"
            } else {
                "CREATE TRIGGER reminder_acceptance_failure BEFORE UPDATE ON scheduled_messages FOR EACH ROW EXECUTE FUNCTION reminder_acceptance_fail()"
            };
            sqlx::query(trigger)
                .execute(db.pool())
                .await
                .expect("install targeted store failure");
            if operation == "create" {
                create(802, "failed create must not be stored")
            } else {
                command(
                    OWNER,
                    803,
                    "cancel",
                    vec![option(
                        "target",
                        CommandOptionValue::String(sentinel.to_owned()),
                    )],
                )
            }
        }
        "list" => {
            let pool = db.independent_pool().await.expect("guarded list pool");
            pool.close().await;
            closed_pool = Some(pool);
            command(OWNER, 804, "list", vec![])
        }
        _ => panic!("unknown fixture operation"),
    };
    let failing = if let Some(pool) = closed_pool.as_ref() {
        CommandRuntime::new(
            pool.clone(),
            executor_at(origin.clone()),
            router_with_commands(gates()),
            GUILD,
            true,
        )
    } else {
        healthy.clone()
    };
    let replies = invoke(&failing, &mock, &interaction).await;
    let after_failure = stored_snapshot(&db).await;
    let posts_during_failure = post_count(&mock);
    if operation != "list" {
        sqlx::query("DROP TRIGGER reminder_acceptance_failure ON scheduled_messages")
            .execute(db.pool())
            .await
            .expect("restore healthy insert/update");
        sqlx::query("DROP FUNCTION reminder_acceptance_fail()")
            .execute(db.pool())
            .await
            .expect("remove fixture failure function");
    }
    let recovered_list = invoke(&healthy, &mock, &command(OWNER, 805, "list", vec![])).await;
    if operation == "create" {
        invoke(&healthy, &mock, &interaction).await;
    }
    let executor = executor_at(origin);
    tick(db.pool(), &executor, GUILD_S, || 4_039_372_800_000)
        .await
        .expect("healthy delivery after restored store");
    let recovered_posts = post_count(&mock);
    mock.shutdown().await;
    db.close()
        .await
        .expect("drop disposable DB before assertions");

    assert_eq!(replies, vec![STORE_FAILURE.to_owned()], "{operation} must report exactly the store-failure reply, never created/empty-list/cancelled success");
    assert_eq!(
        after_failure, before,
        "{operation} failure preserves every persisted column"
    );
    assert_eq!(
        posts_during_failure, 0,
        "store failures never cause an immediate post"
    );
    assert!(
        recovered_list.iter().any(|text| text.contains(sentinel)),
        "healthy list still exposes the preserved reminder"
    );
    assert_eq!(
        recovered_posts,
        if operation == "create" { 2 } else { 1 },
        "restored store delivers existing rows and permits retry after failed admission"
    );
}

#[tokio::test]
async fn create_insert_failure_returns_exact_failure_text_and_preserves_healthy_delivery() {
    runtime_store_failure("create").await;
}

#[tokio::test]
async fn list_pool_failure_returns_exact_failure_text_without_false_empty_list() {
    runtime_store_failure("list").await;
}

#[tokio::test]
async fn cancel_update_failure_returns_exact_failure_text_and_preserves_healthy_delivery() {
    runtime_store_failure("cancel").await;
}
