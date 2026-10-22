//! Acceptance: `/invites` per-inviter attribution + leaderboard.
//!
//! Failing-first spec for the invites build. The builder implements on this
//! branch until every test below passes. Change NO expectation to make them
//! pass; change the product. The builder MUST NOT edit this file.
//!
//! Premise (checked 2026-10-22): pipeline-level attribution is already
//! pinned GREEN by `crates/discord/tests/invite_attribution_acceptance.rs`
//! (single-growth credit, vanity/unknown, deleted-before-check, ambiguous,
//! pre-install). Those pins are NOT duplicated here. This file pins the new
//! user-visible surface only: the `/invites` command, the top-inviters
//! board, restart durability, permission degradation, the leaver rule, and
//! mention-safe naming.
//!
//! Contract (all pinned):
//! - `/invites [member]`: no option answers the invoker; `member` answers
//!   that member. One reply shape: a count line for the target plus the
//!   top-inviters board, always in the same reply.
//! - Replies are PUBLIC (never ephemeral: the board is a shared
//!   celebration) and mention-suppressed (`allowed_mentions.parse == []`,
//!   like the XP leaderboard). Counts render as plain numerals.
//! - Zero reads truthfully: the count line carries `0`, `yet`, `No`, `no `
//!   or `none` — never a positive count, never a silent blank.
//! - The board ranks by attributed count descending and names each inviter
//!   by display name (fetched member name; hostile names must not become a
//!   ping vector — the `parse == []` pin is the safety invariant, exact
//!   sanitization is the builder's choice).
//! - Counts live in Postgres and survive a runtime rebuild over the same
//!   pool with no re-seed (restart = drop + rebuild, the `persistent.rs`
//!   precedent; cross-process durability is the same rows).
//! - Leavers decrement (NET rule): when an attributed member leaves, the
//!   inviter's count goes down, floored at zero. Duplicate or stranger
//!   leaves never drive a count negative. Rationale: the Invite
//!   Tracker/ProBot net-invite convention; churn must not inflate the
//!   board. Fake-invite heuristics stay out of scope.
//! - Denied invite reads (Discord 403 `50013`) degrade to a truthful admin
//!   notice naming the missing permission — never a zero-count claim. A
//!   stale count shown WITH the notice also passes.
//! - `/invites` is published (router + definition + `member` option) and
//!   listed by `/help` with a concise (at most 100 chars) description.
//!
//! Accepted GREEN shapes: immediate (type 4) or deferred-then-edit replies;
//! live invite-read checks or persisted read-health for degradation; board
//! rows in any layout that keeps one line per inviter with name + numeral.
//!
//! Flow the join/leave tests drive (the builder MUST satisfy it): gateway
//! `MemberAdd`/`MemberRemove` through the discord pipeline with scripted
//! invite counters, batches committed via `GatewaySessionStore`, then
//! `/invites` through `CommandRuntime` on the same pool. Counts therefore
//! flow pipeline -> Postgres -> command; any hook point on that path that
//! preserves the observables passes.
//!
//! Controls: `leaderboard_stays_published_control` is GREEN before and
//! after (the XP board must not regress while its neighbours change).
//!
//! Isolated agent-testdb only; no live Discord, no staging/production data.

use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_gateway::{Event, EventTypeFlags};
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::Interaction;
use twilight_model::gateway::payload::incoming::{InteractionCreate, MemberRemove};
use twilight_model::id::Id;
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::gateway_session::{dispatch_action, DispatchAction};
use two_bot_core::help::help_text;
use two_bot_core::{InviteState, NoopFacts, NoopLeveling, RouterGates};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{ActionExecutor, NoClassification, Pipeline, ScriptedInvites};

use super::{checkpoint, TestDb, GUILD};
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::command_runtime_tests::slash;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};

const GUILD_ID: u64 = 2222;

// Fixture ids avoid the digits 1/2/3: count pins look for plain numerals,
// so an echoed snowflake must never satisfy them by accident.
const ALICE: u64 = 9004;
const BRAM: u64 = 9005;
const CIARA: u64 = 9006;
const DAVE: u64 = 9555;
const HOSTILE: u64 = 9777;
const JOIN_BASE: u64 = 9401;

// ---------------------------------------------------------------------------
// Command fixtures (mirror command_runtime_tests; that module's helpers are
// private to it, so the few this file needs are reproduced here)
// ---------------------------------------------------------------------------

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
        scorecard: false,
        automations: false,
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

fn option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

fn interaction_event(interaction: Interaction) -> Event {
    Event::InteractionCreate(Box::new(InteractionCreate(interaction)))
}

fn executor_at(origin: String) -> ActionExecutor {
    ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor")
}

async fn wait_for<F: Fn() -> bool>(predicate: F, what: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deadline waiting for {what}"));
}

/// `/invites` with an optional `member` target, as Discord would deliver it.
fn invites_for(member: Option<u64>) -> Interaction {
    let options = member
        .map(|id| vec![option("member", CommandOptionValue::User(Id::new(id)))])
        .unwrap_or_default();
    slash("invites", None, options)
}

fn set_invoker(interaction: &mut Interaction, id: u64, name: &str) {
    if let Some(member) = interaction.member.as_mut() {
        if let Some(user) = member.user.as_mut() {
            user.id = Id::new(id);
            user.name = name.to_owned();
        }
    }
    if let Some(user) = interaction.user.as_mut() {
        user.id = Id::new(id);
        user.name = name.to_owned();
    }
}

async fn dispatch_invites(pool: &PgPool, mock: &MockRest, interaction: Interaction) {
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(mock.origin()),
        router_with_commands(gates()),
        GUILD_ID,
        false,
    );
    assert!(
        runtime.dispatch(&interaction_event(interaction)),
        "gateway admits the /invites interaction"
    );
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "/invites settles",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Join/leave journey (mirror member_journey; its dispatch is private to it)
// ---------------------------------------------------------------------------

type Journey = Pipeline<GatewayFunnelBuffer, NoopLeveling, NoopFacts, ScriptedInvites>;

fn journey() -> Journey {
    Pipeline::new(
        GatewayFunnelBuffer::default(),
        None,
        None,
        ScriptedInvites::new(),
        NoClassification,
    )
}

fn stamp(clock: &str) -> String {
    format!("2026-09-30T{clock}.000Z")
}

fn counter(code: &str, uses: u64, inviter: u64) -> InviteState {
    InviteState {
        code: code.into(),
        uses,
        inviter_id: Some(inviter),
        channel_id: Some(4444),
        max_uses: 0,
        temporary: false,
    }
}

fn member_join(user_id: u64, clock: &str) -> Value {
    json!({
        "guild_id": GUILD,
        "user": {"id": user_id.to_string(), "username": "fixture-member", "discriminator": "0", "bot": false},
        "roles": [],
        "joined_at": stamp(clock).replace('Z', "+00:00"),
        "pending": false, "deaf": false, "mute": false, "flags": 0,
    })
}

async fn commit_guard(
    store: &GatewaySessionStore,
    seq: u64,
) -> two_bot_core::gateway_session::GatewaySession {
    let saved = checkpoint("invites", seq, "ws://fixture");
    assert_ne!(
        dispatch_action(
            store.load().await.expect("checkpoint").as_ref(),
            &saved.session_id,
            seq
        ),
        DispatchAction::Duplicate,
        "fixture sequences are unique"
    );
    saved
}

async fn dispatch_join(
    pipeline: &Journey,
    store: &GatewaySessionStore,
    seq: u64,
    user_id: u64,
    clock: &str,
) {
    let saved = commit_guard(store, seq).await;
    let packet =
        json!({"op": 0, "s": seq, "t": "GUILD_MEMBER_ADD", "d": member_join(user_id, clock)})
            .to_string();
    let parsed = twilight_gateway::parse(packet, EventTypeFlags::all())
        .expect("join parses")
        .expect("known dispatch");
    pipeline.handle_at(&Event::from(parsed), &stamp(clock));
    store
        .commit_dispatch(&saved, pipeline.handlers().store().take_batch())
        .await
        .expect("commit join");
}

async fn dispatch_leave(
    pipeline: &Journey,
    store: &GatewaySessionStore,
    seq: u64,
    user_id: u64,
    clock: &str,
) {
    let saved = commit_guard(store, seq).await;
    let user = serde_json::from_value(json!({
        "id": user_id.to_string(), "username": "fixture-member",
        "discriminator": "0", "bot": false,
    }))
    .expect("leave user");
    let remove = Event::MemberRemove(MemberRemove {
        guild_id: Id::new(GUILD_ID),
        user,
    });
    pipeline.handle_at(&remove, &stamp(clock));
    store
        .commit_dispatch(&saved, pipeline.handlers().store().take_batch())
        .await
        .expect("commit leave");
}

// ---------------------------------------------------------------------------
// Mock Discord (path-routed responder: order-independent 403s)
// ---------------------------------------------------------------------------

async fn mock_discord(invites_denied: bool) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let path = request
            .path
            .strip_prefix("/api/v10")
            .unwrap_or(request.path.as_str());
        if request.method == "GET" && path.contains("/invites") {
            if invites_denied {
                return ScriptedResponse::json(
                    403,
                    json!({"message": "Missing Permissions", "code": 50013}),
                );
            }
            return ScriptedResponse::json(200, json!([]));
        }
        if request.method == "GET" && path.contains("/members/") {
            let id = path.rsplit('/').next().unwrap_or_default();
            let (username, display) = match id {
                "9004" => ("alice", "Alice"),
                "9005" => ("bram", "Bram"),
                "9006" => ("ciara", "Ciara"),
                "9555" => ("dave", "Dave"),
                "9777" => ("quietuser", "@everyone"),
                _ => ("someone", "Someone"),
            };
            return ScriptedResponse::json(
                200,
                json!({
                    "user": {"id": id, "username": username, "discriminator": "0",
                             "global_name": display, "bot": false},
                    "nick": null, "roles": [], "deaf": false, "mute": false,
                }),
            );
        }
        if request.method == "POST" && path.ends_with("/callback") {
            return ScriptedResponse::status(200);
        }
        if request.method == "PATCH" && path.ends_with("/messages/@original") {
            return ScriptedResponse::json(200, json!({"id": "98"}));
        }
        ScriptedResponse::status(200)
    })
    .await
}

// ---------------------------------------------------------------------------
// Reply extraction (immediate type-4 or deferred-then-edit both pass)
// ---------------------------------------------------------------------------

struct Reply {
    content: String,
    ephemeral: bool,
    mention_parse: Option<Vec<String>>,
}

fn reply_from_data(data: &Value) -> Reply {
    Reply {
        content: data["content"].as_str().unwrap_or_default().to_owned(),
        ephemeral: data["flags"].as_u64().is_some_and(|flags| flags & 64 != 0),
        mention_parse: data["allowed_mentions"]["parse"].as_array().map(|parse| {
            parse
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        }),
    }
}

/// The latest `/invites` answer: the deferred `@original` edit when present,
/// else the immediate callback.
fn invites_reply(requests: &[RestRequest]) -> Reply {
    if let Some(edit) = requests
        .iter()
        .rev()
        .find(|request| request.method == "PATCH" && request.path.ends_with("/messages/@original"))
    {
        return reply_from_data(&serde_json::from_slice(&edit.body).expect("edit is json"));
    }
    let callback = requests
        .iter()
        .rev()
        .find(|request| request.method == "POST" && request.path.ends_with("/callback"))
        .expect("an /invites reply callback or deferred edit");
    let value: Value = serde_json::from_slice(&callback.body).expect("callback is json");
    assert_eq!(
        value["type"].as_u64().unwrap_or(0),
        4,
        "immediate /invites answers use type 4 (deferred answers must PATCH @original instead): {value}"
    );
    reply_from_data(&value["data"])
}

fn line_naming<'a>(content: &'a str, name: &str) -> &'a str {
    content
        .lines()
        .find(|line| line.contains(name))
        .unwrap_or_else(|| panic!("a reply line names {name}: {content:?}"))
}

/// The count reads as a plain numeral: space- or paren-delimited, so board
/// positions (`1. Alice`) and snowflakes never satisfy it by accident.
fn assert_count(line: &str, count: u64) {
    let plain = format!(" {count} ");
    let paren = format!("({count})");
    assert!(
        line.contains(&plain) || line.contains(&paren),
        "line claims count {count} as a plain numeral: {line:?}"
    );
}

/// Zero reads truthfully — never as a positive count, never a silent blank.
fn assert_zero_claim(line: &str) {
    let lower = line.to_lowercase();
    assert!(
        line.contains('0')
            || lower.contains("yet")
            || line.contains("No")
            || line.contains("no ")
            || lower.contains("none"),
        "zero reads truthfully (0/yet/No/no/none), never as a positive count: {line:?}"
    );
}

// ---------------------------------------------------------------------------
// In-memory pins (no database)
// ---------------------------------------------------------------------------

#[test]
fn invites_publish_set_lists_invites() {
    let router = router_with_commands(gates());
    let defs = router.publish_set(&[]).expect("publish set builds");
    let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
    assert!(
        names.contains(&"invites"),
        "/invites is published: {names:?}"
    );
    let def = defs
        .iter()
        .find(|def| def.name == "invites")
        .expect("invites def");
    assert!(
        !def.description.is_empty() && def.description.len() <= 100,
        "concise /invites description: {:?}",
        def.description
    );
    assert!(
        def.options.iter().any(|opt| opt.name == "member"),
        "/invites takes an optional `member`: {:?}",
        def.options
    );
}

#[test]
fn help_lists_invites_command() {
    let router = router_with_commands(gates());
    let defs = router.publish_set(&[]).expect("publish set builds");
    let text = help_text(&defs);
    assert!(text.contains("invites"), "/help lists /invites:\n{text}");
}

#[test]
fn leaderboard_stays_published_control() {
    let router = router_with_commands(gates());
    let defs = router.publish_set(&[]).expect("publish set builds");
    let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
    assert!(
        names.contains(&"leaderboard"),
        "XP /leaderboard stays published: {names:?}"
    );
    assert!(
        help_text(&defs).contains("leaderboard"),
        "XP /leaderboard stays in /help"
    );
}

// ---------------------------------------------------------------------------
// Database pins (disposable agent-testdb only)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn join_then_invites_counts_attributed() {
    let db = TestDb::new().await;
    let pipeline = journey();
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 5, ALICE)]);
    dispatch_join(&pipeline, &db.store, 1, JOIN_BASE, "12:00:00").await;
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 6, ALICE)]);
    dispatch_join(&pipeline, &db.store, 2, JOIN_BASE + 1, "12:30:00").await;

    let mock = mock_discord(false).await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert!(
        !reply.ephemeral,
        "/invites answers publicly, like the XP board"
    );
    let line = line_naming(&reply.content, "Alice");
    assert!(
        line.to_lowercase().contains("invite"),
        "the count line is about invites: {line:?}"
    );
    assert_count(line, 1);

    // No option answers the invoker themself.
    let mut me = invites_for(None);
    set_invoker(&mut me, ALICE, "Alice");
    dispatch_invites(&db.pool, &mock, me).await;
    let reply = invites_reply(&mock.requests());
    let mine = reply
        .content
        .lines()
        .find(|line| line.to_lowercase().contains("invite"))
        .expect("an invite line in the self reply");
    assert_count(mine, 1);

    // Expired: the code vanishes before the next join — nobody is credited,
    // and Alice's count is unchanged (never a wrong inviter).
    pipeline.invite_source().push(GUILD_ID, vec![]);
    dispatch_join(&pipeline, &db.store, 3, JOIN_BASE + 2, "13:00:00").await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert_count(line_naming(&reply.content, "Alice"), 1);
    dispatch_invites(&db.pool, &mock, invites_for(Some(DAVE))).await;
    let reply = invites_reply(&mock.requests());
    assert_zero_claim(line_naming(&reply.content, "Dave"));

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn invites_leaderboard_orders_top_inviters() {
    let db = TestDb::new().await;
    let pipeline = journey();
    let push = |uses: (u64, u64, u64)| {
        pipeline.invite_source().push(
            GUILD_ID,
            vec![
                counter("alice-code", uses.0, ALICE),
                counter("bram-code", uses.1, BRAM),
                counter("ciara-code", uses.2, CIARA),
            ],
        );
    };
    push((0, 0, 0));
    dispatch_join(&pipeline, &db.store, 1, JOIN_BASE, "12:00:00").await;
    for (seq, uses) in [
        (2, (1, 0, 0)),
        (3, (2, 0, 0)),
        (4, (3, 0, 0)),
        (5, (3, 1, 0)),
        (6, (3, 2, 0)),
        (7, (3, 2, 1)),
    ] {
        push(uses);
        dispatch_join(&pipeline, &db.store, seq, JOIN_BASE + seq, "12:30:00").await;
    }

    // Query a zero-count outsider so the board names each inviter exactly once.
    let mock = mock_discord(false).await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(DAVE))).await;
    let reply = invites_reply(&mock.requests());
    assert!(
        !reply.ephemeral,
        "the leaderboard answers publicly, like the XP board"
    );
    assert_eq!(
        reply.mention_parse,
        Some(Vec::new()),
        "the board suppresses mentions"
    );
    assert_zero_claim(line_naming(&reply.content, "Dave"));
    let alice_at = reply.content.find("Alice").expect("Alice ranks");
    let bram_at = reply.content.find("Bram").expect("Bram ranks");
    let ciara_at = reply.content.find("Ciara").expect("Ciara ranks");
    assert!(
        alice_at < bram_at && bram_at < ciara_at,
        "board ranks 3 > 2 > 1: {:?}",
        reply.content
    );
    assert_count(line_naming(&reply.content, "Alice"), 3);
    assert_count(line_naming(&reply.content, "Bram"), 2);
    assert_count(line_naming(&reply.content, "Ciara"), 1);

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn invite_counts_survive_runtime_rebuild() {
    let db = TestDb::new().await;
    let pipeline = journey();
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 5, ALICE)]);
    dispatch_join(&pipeline, &db.store, 1, JOIN_BASE, "12:00:00").await;
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 6, ALICE)]);
    dispatch_join(&pipeline, &db.store, 2, JOIN_BASE + 1, "12:30:00").await;
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 7, ALICE)]);
    dispatch_join(&pipeline, &db.store, 3, JOIN_BASE + 2, "13:00:00").await;

    let mock = mock_discord(false).await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert_count(line_naming(&reply.content, "Alice"), 2);

    // Restart: a fresh runtime over the same pool, with no re-seed, reads
    // the same counts.
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert_count(line_naming(&reply.content, "Alice"), 2);

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn leaver_decrements_inviter_net() {
    let db = TestDb::new().await;
    let pipeline = journey();
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 5, ALICE)]);
    dispatch_join(&pipeline, &db.store, 1, JOIN_BASE, "12:00:00").await;
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 6, ALICE)]);
    dispatch_join(&pipeline, &db.store, 2, JOIN_BASE + 1, "12:30:00").await;

    // The attributed joiner leaves: net rule, Alice returns to zero.
    dispatch_leave(&pipeline, &db.store, 3, JOIN_BASE + 1, "13:00:00").await;
    let mock = mock_discord(false).await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert_zero_claim(line_naming(&reply.content, "Alice"));

    // Duplicate and stranger leaves never drive a count negative.
    dispatch_leave(&pipeline, &db.store, 4, JOIN_BASE + 1, "13:30:00").await;
    dispatch_leave(&pipeline, &db.store, 5, 9499, "14:00:00").await;
    dispatch_invites(&db.pool, &mock, invites_for(Some(ALICE))).await;
    let reply = invites_reply(&mock.requests());
    assert_zero_claim(line_naming(&reply.content, "Alice"));

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn denied_invite_read_degrades_truthfully() {
    let db = TestDb::new().await;
    let pipeline = journey();
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 5, ALICE)]);
    dispatch_join(&pipeline, &db.store, 1, JOIN_BASE, "12:00:00").await;
    pipeline
        .invite_source()
        .push(GUILD_ID, vec![counter("alice-code", 6, ALICE)]);
    dispatch_join(&pipeline, &db.store, 2, JOIN_BASE + 1, "12:30:00").await;

    // Invite reads are now denied: the reply names the missing permission
    // instead of claiming zeros (a stale count WITH the notice also passes).
    let mock = mock_discord(true).await;
    for member in [ALICE, DAVE] {
        dispatch_invites(&db.pool, &mock, invites_for(Some(member))).await;
        let reply = invites_reply(&mock.requests());
        assert!(
            reply.content.to_lowercase().contains("permission"),
            "denied reads name the missing permission: {:?}",
            reply.content
        );
        assert!(
            reply.content.to_lowercase().contains("invite"),
            "the notice is about invites: {:?}",
            reply.content
        );
        assert!(
            !reply.content.to_lowercase().contains("0 invite"),
            "denied reads never claim zero invites: {:?}",
            reply.content
        );
    }

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb; never live Discord or DATABASE_URL"]
async fn invites_reports_zero_and_names_member_safely() {
    let db = TestDb::new().await;
    let mock = mock_discord(false).await;

    // Fresh member, no joins anywhere: zero reads truthfully, named,
    // public, mention-suppressed.
    dispatch_invites(&db.pool, &mock, invites_for(Some(DAVE))).await;
    let reply = invites_reply(&mock.requests());
    assert!(
        !reply.ephemeral,
        "/invites answers publicly, like the XP board"
    );
    assert_eq!(
        reply.mention_parse,
        Some(Vec::new()),
        "replies suppress mentions"
    );
    assert_zero_claim(line_naming(&reply.content, "Dave"));

    // Hostile display name: still posts, never a ping vector. The exact
    // sanitization is the builder's choice; `parse == []` is the invariant.
    dispatch_invites(&db.pool, &mock, invites_for(Some(HOSTILE))).await;
    let reply = invites_reply(&mock.requests());
    assert!(!reply.ephemeral, "hostile names still answer publicly");
    assert_eq!(
        reply.mention_parse,
        Some(Vec::new()),
        "hostile names never become a ping vector"
    );
    assert!(!reply.content.is_empty(), "hostile names still get a reply");

    mock.shutdown().await;
    db.close().await;
}
