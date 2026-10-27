//! Protected acceptance: personal `/remind` with channel delivery.
//!
//! Keep this file verbatim in the implementation handoff. Only the
//! registration in `command_runtime_tests.rs` is unprotected. The tests drive
//! the product through its generic seams only — the shared interaction router,
//! the guild publish set, `CommandRuntime::on_interaction`, and the existing
//! scheduled ticker — with a recording REST double and an injected tick clock.
//! No live Discord, no production or staging database: the store-backed tests
//! use a disposable migrated database per test and skip vacuously when
//! `TWO_TEST_DATABASE_URL` is unset (CI always sets it).
//!
//! Contract under test (the builder implements to this; incorrect pins go back
//! to the acceptance author, never get edited on the build card):
//!
//! - Surface: top-level `remind`, guild-only (`dm_permission = false`, DMs out
//!   of scope), no permission gate (any member). Subcommands: `me` (required
//!   integer `in` = minutes, required string `text`), `list`, `cancel`
//!   (required string `target`: full id, unique id prefix, or unique reminder
//!   text; ambiguous or unknown targets refuse without cancelling anything).
//! - Delivery flows through the existing scheduled ticker (`tick`): one
//!   mention-safe channel post in the invoking channel at fire time, naming the
//!   requester, exactly once across ticks and across a mid-wait restart (fresh
//!   pool). Storage is the builder's choice; every store assertion here is
//!   observable (list output, channel posts), never a table read.
//! - Invalid durations (`in <= 0`, absurdly large), empty text, and over-long
//!   text get a visible refusal and leave zero deliverable state: no list
//!   marker, no post. Exact min/max/length bounds are the builder's within
//!   those sane edges (valid probes use 30/60 minutes and short texts).
//! - Per-user caps: 30 rapid creates by one member leave between 1 and 29
//!   live reminders (cap value free in that range); other members are
//!   unaffected and each member lists only their own.
//! - Hostile text (`@everyone`/`@here`, `discord.gg` links) is stored or
//!   refused, but what reaches the channel is inert per the shared
//!   `message_safety` predicates while the surrounding text survives.
//! - The ticker's closed mention policy holds: delivery posts carry
//!   `allowed_mentions.parse == []` and never create DM channels.
//!
//! Breaks caught: unrouted/unpublished command, unknown-command answers,
//! missing-requester or multi-post delivery, repost after delivery,
//! restart-duplicated or restart-lost reminders, silent or stateful refusals,
//! neutralizer bypasses, blanked hostile bodies, global (non-per-user) caps,
//! cross-member list leaks, DM or foreign-channel posts, and cross-guild
//! handling. Passing controls distinguish proven routing/publish wiring from
//! the red command path and pin the guild fence both layers.

#![cfg(test)]

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use twilight_model::application::interaction::application_command::CommandOptionValue;
use twilight_model::application::interaction::Interaction;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use two_bot_core::commands::CommandOptionType;
use two_bot_core::message_safety::{contains_mass_mention, has_message_text, text_len};
use two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY;
use two_bot_core::{InteractionRouter, RouterGates, SlashContext, SlashOutcome};
use two_bot_testsupport::TestDatabase;

use super::{
    executor_at, from_member, option, runtime_without_db, slash, MockRest, RestRequest, CHANNEL,
    CHANNEL_S, GUILD, GUILD_S,
};
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::scheduled_jobs::tick;

/// Distinct requester ids (long on purpose: delivery attribution asserts a
/// substring match, so short ids could match by coincidence).
const MEMBER_A: u64 = 881_001_001_001;
const MEMBER_B: u64 = 882_002_002_002;

/// Rapid-create probe count for the cap test. Any per-user cap below this
/// refuses at least once; the exact cap value stays the builder's.
const CAP_PROBE_CREATES: usize = 30;

fn all_gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn wall_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("wall clock")
            .as_millis(),
    )
    .expect("millis fit u64")
}

fn remind_me(member: u64, minutes: i64, text: &str) -> Interaction {
    let mut interaction = from_member(
        slash(
            "remind",
            Some(CHANNEL),
            vec![option(
                "me",
                CommandOptionValue::SubCommand(vec![
                    option("in", CommandOptionValue::Integer(minutes)),
                    option("text", CommandOptionValue::String(text.to_owned())),
                ]),
            )],
        ),
        member,
        Permissions::VIEW_CHANNEL | Permissions::USE_SLASH_COMMANDS | Permissions::SEND_MESSAGES,
    );
    // Fixture-only authorization correction; all original assertions stay frozen.
    interaction.channel = Some(
        serde_json::from_value(serde_json::json!({"id": CHANNEL_S, "type": 0}))
            .expect("resolved ordinary text channel"),
    );
    interaction
}

fn remind_list(member: u64) -> Interaction {
    from_member(
        slash(
            "remind",
            Some(CHANNEL),
            vec![option("list", CommandOptionValue::SubCommand(Vec::new()))],
        ),
        member,
        Permissions::empty(),
    )
}

fn remind_cancel(member: u64, target: &str) -> Interaction {
    from_member(
        slash(
            "remind",
            Some(CHANNEL),
            vec![option(
                "cancel",
                CommandOptionValue::SubCommand(vec![option(
                    "target",
                    CommandOptionValue::String(target.to_owned()),
                )]),
            )],
        ),
        member,
        Permissions::empty(),
    )
}

async fn fixture_db() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP personal-remind acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn db_runtime(pool: sqlx::PgPool, origin: String) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(all_gates()),
        GUILD,
        true,
    )
}

/// Dispatch one interaction and return only the REST requests it caused, so
/// each step attributes its own reply without a fresh mock per call.
async fn interact(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    interaction: &Interaction,
) -> Vec<RestRequest> {
    let before = mock.requests().len();
    runtime.on_interaction(interaction).await;
    mock.requests().into_iter().skip(before).collect()
}

fn channel_post_path() -> String {
    format!("/api/v10/channels/{CHANNEL_S}/messages")
}

fn is_channel_post(request: &RestRequest) -> bool {
    request.method == "POST" && request.path == channel_post_path()
}

fn channel_posts(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|request| is_channel_post(request))
        .map(|request| serde_json::from_slice(&request.body).expect("JSON post body"))
        .collect()
}

/// Every member-visible reply text in these requests: immediate callbacks
/// (`data.content`) plus deferred edits and followups (top-level `content`).
/// Channel posts are deliveries, not replies, and stay out.
fn reply_texts(requests: &[RestRequest]) -> Vec<String> {
    let mut out = Vec::new();
    for request in requests {
        if is_channel_post(request) {
            continue;
        }
        let Ok(json) = serde_json::from_slice::<Value>(&request.body) else {
            continue;
        };
        if let Some(content) = json.pointer("/data/content").and_then(Value::as_str) {
            out.push(content.to_owned());
        } else if let Some(content) = json.get("content").and_then(Value::as_str) {
            out.push(content.to_owned());
        }
    }
    out
}

/// The command is wired: it answers (silence is never a refusal) and the
/// answer is its own, not the shared unknown-command reply. Confirm/refuse
/// copy stays the builder's; behavior pins (markers, posts) tell them apart.
fn assert_recognized(texts: &[String], case: &str) {
    assert!(
        !texts.is_empty(),
        "{case}: /remind must answer; silence is not a refusal"
    );
    for text in texts {
        assert_ne!(
            text.as_str(),
            UNKNOWN_COMMAND_REPLY,
            "{case}: /remind must route to its handler, got the unknown-command reply"
        );
    }
}

fn markers_present(texts: &[String], markers: &[&str]) -> Vec<String> {
    markers
        .iter()
        .filter(|marker| texts.iter().any(|text| text.contains(**marker)))
        .map(|marker| (*marker).to_owned())
        .collect()
}

/// Inert on the wire, mirroring the scheduled-ticker safety screen: nothing
/// left for the shared mention neutralizer to rewrite, no clickable invite
/// host, still sendable text within the content ceiling.
fn assert_inert(output: &str, case: &str) {
    assert!(
        !contains_mass_mention(output),
        "{case:?}: delivered body still carries a mass mention: {output:?}"
    );
    assert!(
        !output.contains("@everyone") && !output.contains("@here"),
        "{case:?}: delivered body still contains a pingable mass mention: {output:?}"
    );
    assert!(
        !output.to_lowercase().contains("discord.gg"),
        "{case:?}: delivered body still contains a linkable invite host: {output:?}"
    );
    assert!(
        has_message_text(output),
        "{case:?}: delivered body must still be sendable text, got {output:?}"
    );
    assert!(
        text_len(output) <= 2000,
        "{case:?}: delivered body exceeds the 2000-unit ceiling: {} units",
        text_len(output)
    );
}

#[test]
fn remind_routes_to_a_handler_for_any_member_in_the_configured_guild() {
    let router = InteractionRouter::new(all_gates());
    let ctx = SlashContext {
        name: "remind",
        guild_id: Some(GUILD),
        actor_permissions: Some(0),
        custom_row: None,
    };
    assert!(
        matches!(router.route_slash(&ctx), SlashOutcome::Handled { .. }),
        "/remind must route to its handler for a member with no privileges"
    );
}

#[test]
fn remind_is_published_guild_only_with_me_list_cancel() {
    let router = InteractionRouter::new(all_gates());
    let set = router.publish_set(&[]).expect("publish set builds");
    let remind = set
        .iter()
        .find(|definition| definition.name == "remind")
        .expect("/remind is published");
    assert!(
        !remind.dm_permission,
        "reminders deliver to guild channels; DMs are out of scope"
    );
    assert_eq!(
        remind.default_member_permissions, None,
        "any member may use /remind"
    );
    let subcommand = |name: &str| {
        remind
            .options
            .iter()
            .find(|opt| opt.name == name)
            .unwrap_or_else(|| panic!("/remind has a {name} subcommand"))
    };
    let me = subcommand("me");
    assert_eq!(me.kind, CommandOptionType::SubCommand.as_u8());
    for (name, kind) in [
        ("in", CommandOptionType::Integer),
        ("text", CommandOptionType::String),
    ] {
        let opt = me
            .options
            .iter()
            .find(|opt| opt.name == name)
            .unwrap_or_else(|| panic!("/remind me has an {name} option"));
        assert_eq!(opt.kind, kind.as_u8(), "/remind me {name} option type");
        assert_eq!(opt.required, Some(true), "/remind me {name} is required");
    }
    let list = subcommand("list");
    assert_eq!(list.kind, CommandOptionType::SubCommand.as_u8());
    let cancel = subcommand("cancel");
    assert_eq!(cancel.kind, CommandOptionType::SubCommand.as_u8());
    let target = cancel
        .options
        .iter()
        .find(|opt| opt.name == "target")
        .expect("/remind cancel has a target option");
    assert_eq!(target.kind, CommandOptionType::String.as_u8());
    assert_eq!(
        target.required,
        Some(true),
        "/remind cancel target is required"
    );
}

/// Control: outside the configured guild the router stays silent, so a wired
/// `/remind` can never answer cross-guild.
#[test]
fn remind_stays_silent_outside_the_configured_guild() {
    let router = InteractionRouter::new(all_gates());
    for guild_id in [Some(9999), None] {
        let ctx = SlashContext {
            name: "remind",
            guild_id,
            actor_permissions: Some(0),
            custom_row: None,
        };
        assert_eq!(
            router.route_slash(&ctx),
            SlashOutcome::Ignore,
            "foreign/missing guild stays silent for /remind"
        );
    }
}

/// Control: the harness still reports genuinely unknown names as unknown.
#[test]
fn unknown_names_still_route_unknown() {
    let router = InteractionRouter::new(all_gates());
    let ctx = SlashContext {
        name: "not-a-command",
        guild_id: Some(GUILD),
        actor_permissions: Some(0),
        custom_row: None,
    };
    assert_eq!(router.route_slash(&ctx), SlashOutcome::Unknown);
}

/// The full dispatch path recognizes `/remind me` in the guild without any
/// database: the store is unreachable here, so the builder's handler answers
/// with its store-failure reply — still its own reply, never unknown.
#[tokio::test]
async fn remind_dispatch_is_recognized_in_the_configured_guild() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(all_gates(), true, origin);
    let fresh = interact(
        &runtime,
        &mock,
        &remind_me(MEMBER_A, 30, "remind-accept-dispatch-01"),
    )
    .await;
    assert_recognized(&reply_texts(&fresh), "create");
    mock.shutdown().await;
}

/// Control: a foreign or missing guild gets silence through the live dispatch
/// path too, not a refusal and never an answer.
#[tokio::test]
async fn remind_dispatch_silent_outside_the_configured_guild() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(all_gates(), true, origin);
    for guild_id in [Some(Id::new(9999)), None] {
        let mut interaction = remind_me(MEMBER_A, 30, "remind-accept-foreign-01");
        interaction.guild_id = guild_id;
        runtime.on_interaction(&interaction).await;
    }
    assert!(
        mock.requests().is_empty(),
        "foreign/missing guild stays silent for /remind"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn create_delivers_exactly_once_at_fire_time_in_the_invoking_channel() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();
    let text = "remind-accept-once standup notes";

    let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 30, text)).await;
    assert_recognized(&reply_texts(&fresh), "create");

    tick(&pool, &rest, GUILD_S, || created_at + 60_000)
        .await
        .expect("pre-fire tick");
    assert!(
        channel_posts(&mock.requests()).is_empty(),
        "no delivery before fire time"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 120 * 60_000)
        .await
        .expect("fire tick");
    let posts = channel_posts(&mock.requests());
    assert_eq!(posts.len(), 1, "exactly one delivery");
    let content = posts[0]["content"].as_str().expect("post content");
    assert!(
        content.contains(&MEMBER_A.to_string()),
        "delivery names the requester: {content}"
    );
    assert!(
        content.contains(text),
        "delivery carries the reminder text: {content}"
    );
    assert_eq!(
        posts[0]["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "closed mention policy: delivery never pings"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 121 * 60_000)
        .await
        .expect("settle tick");
    assert_eq!(
        channel_posts(&mock.requests()).len(),
        1,
        "no repost after delivery"
    );

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn list_and_cancel_round_trip() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();
    let alpha = "remind-accept-alpha dentist";
    let beta = "remind-accept-beta standup";

    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let texts = reply_texts(&fresh);
    assert_recognized(&texts, "empty list");
    assert!(
        markers_present(&texts, &[alpha, beta]).is_empty(),
        "empty list shows no reminders"
    );

    for text in [alpha, beta] {
        let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 30, text)).await;
        assert_recognized(&reply_texts(&fresh), "create");
    }
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let texts = reply_texts(&fresh);
    assert_recognized(&texts, "list");
    assert_eq!(
        markers_present(&texts, &[alpha, beta]).len(),
        2,
        "list shows both reminders"
    );

    let fresh = interact(&runtime, &mock, &remind_cancel(MEMBER_A, alpha)).await;
    assert_recognized(&reply_texts(&fresh), "cancel");
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let live = markers_present(&reply_texts(&fresh), &[alpha, beta]);
    assert_eq!(
        live,
        vec![beta.to_owned()],
        "cancel removes only its target"
    );

    let fresh = interact(
        &runtime,
        &mock,
        &remind_cancel(MEMBER_A, "no-such-reminder-xyz"),
    )
    .await;
    assert_recognized(&reply_texts(&fresh), "cancel unknown");
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    assert_eq!(
        markers_present(&reply_texts(&fresh), &[alpha, beta]),
        vec![beta.to_owned()],
        "cancelling an unknown target changes nothing"
    );

    let morning = "remind-accept dentist morning";
    let evening = "remind-accept dentist evening";
    for text in [morning, evening] {
        let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 60, text)).await;
        assert_recognized(&reply_texts(&fresh), "create");
    }
    let fresh = interact(&runtime, &mock, &remind_cancel(MEMBER_A, "dentist")).await;
    assert_recognized(&reply_texts(&fresh), "cancel ambiguous");
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    assert_eq!(
        markers_present(&reply_texts(&fresh), &[morning, evening]).len(),
        2,
        "an ambiguous cancel refuses and keeps both reminders"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 180 * 60_000)
        .await
        .expect("fire tick");
    let posts = channel_posts(&mock.requests());
    assert_eq!(
        posts.len(),
        3,
        "beta plus both dentists fire; alpha never does"
    );
    let bodies: Vec<&str> = posts
        .iter()
        .map(|post| post["content"].as_str().expect("post content"))
        .collect();
    for marker in [beta, morning, evening] {
        assert_eq!(
            bodies.iter().filter(|body| body.contains(marker)).count(),
            1,
            "{marker} fires exactly once"
        );
    }
    assert!(
        bodies.iter().all(|body| !body.contains(alpha)),
        "the cancelled reminder never fires"
    );

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn restart_mid_wait_still_delivers_exactly_once() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin.clone());
    let created_at = wall_ms();
    let text = "remind-accept-restart deploy window";

    let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 60, text)).await;
    assert_recognized(&reply_texts(&fresh), "create");

    tick(&pool, &rest, GUILD_S, || created_at + 60_000)
        .await
        .expect("pre-restart tick");
    assert!(
        channel_posts(&mock.requests()).is_empty(),
        "no delivery before fire time"
    );

    // Injected restart: drop the runtime and its pool, then tick again over a
    // fresh pool to the same database, like a redeployed process.
    drop(runtime);
    drop(pool);
    let pool = db.independent_pool().await.expect("post-restart pool");
    let rest = executor_at(origin);

    tick(&pool, &rest, GUILD_S, || created_at + 120 * 60_000)
        .await
        .expect("post-restart fire tick");
    let posts = channel_posts(&mock.requests());
    assert_eq!(posts.len(), 1, "the reminder survives the restart");
    let content = posts[0]["content"].as_str().expect("post content");
    assert!(
        content.contains(text),
        "post-restart delivery carries the text: {content}"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 121 * 60_000)
        .await
        .expect("settle tick");
    assert_eq!(
        channel_posts(&mock.requests()).len(),
        1,
        "the restart never duplicates the delivery"
    );

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn invalid_inputs_are_refused_with_zero_deliverable_state() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();
    let long_text = format!("{}remind-accept-invalid-longtail", "x".repeat(4900));

    let cases: Vec<(i64, String)> = vec![
        (0, "remind-accept-invalid-zero".to_owned()),
        (-5, "remind-accept-invalid-negative".to_owned()),
        (1_000_000_000, "remind-accept-invalid-huge".to_owned()),
        (30, String::new()),
        (30, long_text),
    ];
    for (minutes, text) in &cases {
        let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, *minutes, text)).await;
        assert_recognized(&reply_texts(&fresh), "invalid input");
    }
    assert!(
        channel_posts(&mock.requests()).is_empty(),
        "refused input never posts on create"
    );

    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let texts = reply_texts(&fresh);
    assert_recognized(&texts, "list after refusals");
    let tokens: Vec<&str> = cases
        .iter()
        .filter_map(|(_, text)| {
            if text.is_empty() {
                None
            } else {
                Some(text.as_str())
            }
        })
        .collect();
    assert!(
        markers_present(&texts, &tokens).is_empty(),
        "refused input leaves no list marker"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 240 * 60_000)
        .await
        .expect("settle tick");
    assert!(
        channel_posts(&mock.requests()).is_empty(),
        "refused input never fires"
    );

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn hostile_text_is_rendered_inert_in_delivery() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();
    let text = "Game night @everyone bring snacks remind-accept-hostile-01 claim at discord.gg/free-nitro @here";

    let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 30, text)).await;
    assert_recognized(&reply_texts(&fresh), "create");

    tick(&pool, &rest, GUILD_S, || created_at + 120 * 60_000)
        .await
        .expect("fire tick");
    let posts = channel_posts(&mock.requests());
    assert_eq!(posts.len(), 1, "the hostile reminder still fires once");
    let content = posts[0]["content"].as_str().expect("post content");
    for fragment in ["Game night", "bring snacks", "remind-accept-hostile-01"] {
        assert!(
            content.contains(fragment),
            "neutralization defuses the payload, not the reminder around it: {content}"
        );
    }
    assert_inert(content, "hostile delivery");

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn per_user_caps_bind_each_member_without_cross_talk() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();

    let markers_a: Vec<String> = (0..CAP_PROBE_CREATES)
        .map(|index| format!("remind-cap-a-{index:02}"))
        .collect();
    for marker in &markers_a {
        let fresh = interact(&runtime, &mock, &remind_me(MEMBER_A, 30, marker)).await;
        assert_recognized(&reply_texts(&fresh), "create");
    }
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let refs_a: Vec<&str> = markers_a.iter().map(String::as_str).collect();
    let live_a = markers_present(&reply_texts(&fresh), &refs_a);
    let max_live = CAP_PROBE_CREATES - 1;
    assert!(
        !live_a.is_empty() && live_a.len() < CAP_PROBE_CREATES,
        "one member keeps 1..{max_live} live reminders; the rest refuse"
    );

    // A second member starts empty, sees none of the first member's rows, and
    // can still create while the first member sits at their cap.
    let marker_b = "remind-cap-b-00";
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_B)).await;
    let texts_b = reply_texts(&fresh);
    assert_recognized(&texts_b, "second member list");
    assert!(
        markers_present(&texts_b, &refs_a).is_empty(),
        "members list only their own reminders"
    );
    let fresh = interact(&runtime, &mock, &remind_me(MEMBER_B, 30, marker_b)).await;
    assert_recognized(&reply_texts(&fresh), "second member create");
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_B)).await;
    let texts_b = reply_texts(&fresh);
    assert_eq!(
        markers_present(&texts_b, &[marker_b]),
        vec![marker_b.to_owned()],
        "the second member's reminder lands"
    );
    assert!(
        markers_present(&texts_b, &refs_a).is_empty(),
        "the second member still sees none of the first member's rows"
    );
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    let texts_a = reply_texts(&fresh);
    assert_eq!(
        markers_present(&texts_a, &refs_a).len(),
        live_a.len(),
        "the second member's create disturbs nothing of the first"
    );
    assert!(
        markers_present(&texts_a, &[marker_b]).is_empty(),
        "listing stays per-member in both directions"
    );

    tick(&pool, &rest, GUILD_S, || created_at + 120 * 60_000)
        .await
        .expect("fire tick");
    let posts = channel_posts(&mock.requests());
    assert_eq!(
        posts.len(),
        live_a.len() + 1,
        "every live reminder fires exactly once; refusals add nothing"
    );
    let bodies: Vec<&str> = posts
        .iter()
        .map(|post| post["content"].as_str().expect("post content"))
        .collect();
    let mut expected = live_a.clone();
    expected.push(marker_b.to_owned());
    for marker in &expected {
        assert_eq!(
            bodies.iter().filter(|body| body.contains(marker)).count(),
            1,
            "{marker} fires exactly once"
        );
    }
    for marker in markers_a.iter().filter(|marker| !live_a.contains(marker)) {
        assert!(
            bodies.iter().all(|body| !body.contains(marker)),
            "refused create {marker} never fires"
        );
    }

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}

#[tokio::test]
async fn delivery_touches_no_dm_or_foreign_channel_path() {
    let Some(db) = fixture_db().await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = db.pool().clone();
    let runtime = db_runtime(pool.clone(), origin.clone()).await;
    let rest = executor_at(origin);
    let created_at = wall_ms();

    let fresh = interact(
        &runtime,
        &mock,
        &remind_me(MEMBER_A, 30, "remind-accept-channelscope deploy"),
    )
    .await;
    assert_recognized(&reply_texts(&fresh), "create");
    let fresh = interact(&runtime, &mock, &remind_list(MEMBER_A)).await;
    assert_recognized(&reply_texts(&fresh), "list");
    tick(&pool, &rest, GUILD_S, || created_at + 120 * 60_000)
        .await
        .expect("fire tick");

    let requests = mock.requests();
    for request in &requests {
        let method = request.method.as_str();
        let path = request.path.as_str();
        assert!(
            !path.contains("/users/@me/channels"),
            "no DM channel is ever opened: {method} {path}"
        );
        if request.method == "POST" && request.path.contains("/channels/") {
            assert_eq!(
                request.path,
                channel_post_path(),
                "channel posts go only to the invoking channel"
            );
        }
    }
    assert_eq!(
        channel_posts(&requests).len(),
        1,
        "the scope pin is non-vacuous: one invoking-channel delivery happened"
    );

    mock.shutdown().await;
    db.close().await.expect("drop fixture database");
}
