#![cfg(test)]

//! Protected RED acceptance: show my open tickets and the staff queue.
//!
//! Real gateway dispatch (`CommandRuntime::with_tickets` + `on_interaction`) over a
//! disposable test database with loopback Discord only. No live Discord, staging or
//! production data. The builder must not edit this file.
//!
//! ## Contract under test (builder implements this)
//!
//! - `/mytickets` shows the invoking member their own non-closed tickets in the
//!   invoking guild: id, status, channel link only for a usable Open channel, and
//!   claim state. Never another member's tickets. Open to every member.
//! - `/ticket-queue` is staff-gated (today's ticket staff authorization: staff role,
//!   ManageChannels, or Administrator) and pages the guild's non-closed tickets with
//!   opener, status, Discord-localized created-at (`<t:...>`), and claim state.
//!   Optional `page` integer, default 1.
//! - Only `Open` + channel renders `<#channel>` as usable. `Creating`, `Closing` and
//!   `CleanupPending` render their status truthfully with no usable channel link,
//!   using the same no-effect vocabulary as the Open-refusal work (status shown, no
//!   ready link, no recovery promise). `Closed` is excluded from both views.
//!   "Recovery" in the card means Open rows whose `recovery_action` is
//!   `ReattachOpenControls` (and stale Creating/Closing rows): they render by status
//!   and lists never trigger recovery work.
//! - Usable = `Open` + `channel_id` present. List paths are DB-only: no per-ticket
//!   Discord reads to prove liveness (a deleted channel's `<#id>` defers truth to the
//!   Discord client, which renders #deleted-channel). Replies never claim readiness
//!   based on channel identity alone.
//! - Pure reads: no reserve/claim/close/repair/transcript/audit work. Ticket rows,
//!   transcript rows and audit counts are byte-identical before and after every list.
//!   Reserve decisions (`decide_open`) are identical before and after.
//! - Wrong-guild interactions stay silent (router `Ignore`, zero Discord requests),
//!   matching the custom-command list precedent. Tickets-disabled guilds, store
//!   outages, non-staff queue attempts and invalid pages render privately
//!   (ephemeral, mention-safe, bounded) with guidance and zero mutation. Store
//!   outages name a retry; disabled/non-staff/invalid-page name the remedy.
//! - Every reply is ephemeral (`flags: 64`), `allowed_mentions.parse == []`, no
//!   user/role allowlist, `replied_user != true`, and the whole content fits in
//!   2000 UTF-16 units (Discord's reply budget). Refusals fit in 500.
//! - Hostile opener ids render inert: no pings (`parse == []`), embedded newlines
//!   are sanitized so rows cannot inject lines, output stays bounded. Raw echo of a
//!   hostile id with suppression is allowed except for newlines.
//! - One active ticket per (guild, opener) is the standing store invariant (partial
//!   unique index), so "member with many" means the guild holds many tickets from
//!   many openers while the member sees only their own.
//!
//! ## RED expectation on unmodified main
//!
//! Both slash names are unknown to the router, so every dispatch below answers
//! `UNKNOWN_COMMAND_REPLY` and the content assertions fail. That failing assertion
//! is the RED proof that the commands/readers do not exist. After the builder lands
//! routing, store readers, renderer, pagination and help/reference updates, the same
//! suite passes (GREEN) with zero changes to this file.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::Interaction;
use twilight_model::guild::Permissions;
use twilight_model::id::marker::RoleMarker;
use twilight_model::id::{AnonymizableId, Id};
use two_bot_core::funnel::format_iso_millis;
use two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY;
use two_bot_core::tickets::{decide_open, COOLDOWN_SECONDS};
use two_bot_cutover::tickets::{OpenResult, TicketStore};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use crate::command_runtime::CommandRuntime;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::ticket_runtime::{TicketConfig, TicketRuntime};

const GUILD: &str = "100";
const GUILD_ID: u64 = 100;
const FOREIGN_GUILD: &str = "999";
const FOREIGN_GUILD_ID: u64 = 999;
const CHANNEL_BASE: u64 = 600;
const STAFF_ROLE: &str = "300";
const STAFF_ROLE_ID: u64 = 300;
const BOT_ID: u64 = 400;
const APP_ID: u64 = 400;

const MYTICKETS: &str = "mytickets";
const QUEUE: &str = "ticket-queue";

const BASE_MILLIS: i64 = 1_700_000_000_000;

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP ticket list acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable test database"),
    )
}

fn ticket_config() -> TicketConfig {
    TicketConfig {
        guild_id: GUILD.to_owned(),
        category_id: "200".into(),
        panel_channel_id: "700".into(),
        staff_role_id: STAFF_ROLE.into(),
        cooldown_seconds: COOLDOWN_SECONDS,
    }
}

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap()
}

async fn harness(db: &TestDatabase, mock: &MockRest) -> (Arc<TicketRuntime>, Arc<CommandRuntime>) {
    let executor = executor_for(mock);
    let tickets = Arc::new(
        TicketRuntime::new(
            db.pool().clone(),
            executor.clone(),
            ticket_config(),
            HashMap::new(),
        )
        .unwrap(),
    );
    tickets.set_bot_id(BOT_ID);
    let commands = CommandRuntime::with_tickets(db.pool().clone(), executor, Arc::clone(&tickets));
    (tickets, commands)
}

fn slash(
    name: &str,
    guild_id: u64,
    member_id: u64,
    roles: Vec<Id<RoleMarker>>,
    permissions: Permissions,
    options: Vec<CommandDataOption>,
    interaction_id: u64,
) -> Interaction {
    // Reuse the shared fixture and mutate only non-deprecated fields: the
    // deprecated `channel_id` literal stays inside the grandfathered helper,
    // so this file adds no silenced-checker waivers.
    let mut interaction = super::slash(name, Some(CHANNEL_BASE), options);
    interaction.application_id = Id::new(APP_ID);
    interaction.authorizing_integration_owners.guild = Some(AnonymizableId::Id(Id::new(guild_id)));
    interaction.guild_id = Some(Id::new(guild_id));
    interaction.id = Id::new(interaction_id);
    interaction.token = format!("ticket-list-test-{interaction_id}");
    let member = interaction.member.as_mut().expect("guild member");
    member.permissions = Some(permissions);
    member.roles = roles;
    member.user = Some(super::user(member_id, false));
    interaction.user = Some(super::user(member_id, false));
    interaction
}

fn member_slash(
    name: &str,
    member_id: u64,
    options: Vec<CommandDataOption>,
    interaction_id: u64,
) -> Interaction {
    slash(
        name,
        GUILD_ID,
        member_id,
        Vec::new(),
        Permissions::empty(),
        options,
        interaction_id,
    )
}

fn staff_slash(
    name: &str,
    staff_id: u64,
    options: Vec<CommandDataOption>,
    interaction_id: u64,
) -> Interaction {
    slash(
        name,
        GUILD_ID,
        staff_id,
        vec![Id::new(STAFF_ROLE_ID)],
        Permissions::empty(),
        options,
        interaction_id,
    )
}

fn page_option(page: i64) -> CommandDataOption {
    CommandDataOption {
        name: "page".to_owned(),
        value: CommandOptionValue::Integer(page),
    }
}

type TicketRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
);
type TranscriptRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i32,
    String,
    String,
);

async fn state(pool: &PgPool) -> (Vec<TicketRow>, Vec<TranscriptRow>, i64, i64, i64) {
    let tickets = sqlx::query_as::<_, TicketRow>(
        "SELECT id, guild_id, channel_id, opener_id, claimed_by, status, created_at, closing_started_at, closed_at FROM tickets ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .expect("snapshot tickets");
    let transcripts = sqlx::query_as::<_, TranscriptRow>(
        "SELECT ticket_id, guild_id, channel_id, opener_id, claimed_by, content, message_count, created_at, purge_after FROM ticket_transcripts ORDER BY ticket_id",
    )
    .fetch_all(pool)
    .await
    .expect("snapshot transcripts");
    let ops = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operational_audit_log")
        .fetch_one(pool)
        .await
        .expect("ops audit count");
    let automation = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .expect("automation audit count");
    let announcements =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(pool)
            .await
            .expect("announcements audit count");
    (tickets, transcripts, ops, automation, announcements)
}

/// Parse the one ephemeral reply (immediate type-4 or defer type-5 + PATCH edit).
/// Returns (content, full body). Asserts ephemeral + mention-safe + bounded.
fn reply(requests: &[RestRequest]) -> (String, Value) {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one initial acknowledgement: {requests:?}"
    );
    let initial: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    let body = if initial["type"] == 5 {
        assert_eq!(initial["data"]["flags"], 64, "defer is ephemeral");
        let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
        assert_eq!(
            edits.len(),
            1,
            "defer + one completion, no second reply: {requests:?}"
        );
        assert!(
            edits[0].path.ends_with("/messages/@original"),
            "completion edits @original: {}",
            edits[0].path
        );
        serde_json::from_slice::<Value>(&edits[0].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4, "immediate reply: {initial}");
        assert_eq!(
            initial["data"]["flags"], 64,
            "reply is ephemeral: {initial}"
        );
        assert_eq!(
            requests.len(),
            1,
            "immediate reply is the only request: {requests:?}"
        );
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([]),
            "no explicit ping allowlist in {body}"
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], json!(true));
    let content = body["content"]
        .as_str()
        .expect("human-readable reply")
        .to_owned();
    assert!(
        content.encode_utf16().count() <= 2000,
        "entire response fits Discord's 2000-unit budget: {content}"
    );
    (content, body)
}

fn assert_refusal_bounded(content: &str) {
    assert!(
        content.encode_utf16().count() <= 500,
        "short refusal: {content}"
    );
}

/// Lists are DB-only: the only Discord requests are the ephemeral acknowledgement
/// and its completion edit. No channel posts, permission edits, deletions or reads.
fn assert_read_only(requests: &[RestRequest]) {
    for request in requests {
        if request.path.ends_with("/callback") || request.path.ends_with("/messages/@original") {
            continue;
        }
        panic!(
            "list must not touch Discord beyond its ephemeral reply: {} {}",
            request.method, request.path
        );
    }
}

fn channel(n: u64) -> String {
    (CHANNEL_BASE + n).to_string()
}

async fn seed_open(store: &TicketStore, id: &str, opener: &str, channel_id: &str, created_at: i64) {
    let created = matches!(
        store
            .reserve(id, opener, created_at, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    );
    assert!(created, "reserve {id} for {opener}");
    store.record_channel(id, channel_id).await.unwrap();
    store.activate(id, channel_id).await.unwrap();
}

async fn seed_creating(store: &TicketStore, id: &str, opener: &str, created_at: i64) {
    assert!(matches!(
        store
            .reserve(id, opener, created_at, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
}

async fn seed_closing(
    store: &TicketStore,
    id: &str,
    opener: &str,
    channel_id: &str,
    created_at: i64,
) {
    seed_open(store, id, opener, channel_id, created_at).await;
    store.begin_close(id, created_at + 5_000).await.unwrap();
}

async fn seed_cleanup(
    store: &TicketStore,
    id: &str,
    opener: &str,
    channel_id: &str,
    created_at: i64,
) {
    seed_open(store, id, opener, channel_id, created_at).await;
    store
        .queue_open_rollback(id, created_at + 5_000)
        .await
        .unwrap();
}

async fn seed_closed(
    store: &TicketStore,
    id: &str,
    opener: &str,
    channel_id: &str,
    created_at: i64,
) {
    seed_cleanup(store, id, opener, channel_id, created_at).await;
    store.finish_cleanup(id, created_at + 10_000).await.unwrap();
}

fn page_metadata(content: &str) -> Option<(usize, usize)> {
    let tail = content.split_once("Page ")?.1;
    let (current, total) = tail.split_once(" of ")?;
    let total: String = total.chars().take_while(char::is_ascii_digit).collect();
    Some((current.parse().ok()?, total.parse().ok()?))
}

#[tokio::test]
async fn mytickets_empty_member_sees_private_empty_with_zero_mutation() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    // Another member's ticket exists; the invoking member has none.
    seed_open(&store, "t-other", "501", &channel(1), BASE_MILLIS).await;
    let before = state(db.pool()).await;
    commands
        .on_interaction(&member_slash(MYTICKETS, 500, Vec::new(), 1))
        .await;
    let requests = mock.requests();
    let after = state(db.pool()).await;
    db.close().await.unwrap();
    let mock_requests = requests.clone();
    mock.shutdown().await;

    assert_eq!(before, after, "empty list mutates nothing");
    assert_read_only(&mock_requests);
    let (content, _) = reply(&mock_requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /mytickets is not wired"
    );
    assert!(
        content.to_ascii_lowercase().contains("no")
            && content.to_ascii_lowercase().contains("ticket"),
        "empty member sees no-tickets guidance: {content}"
    );
    assert!(
        !content.contains("t-other"),
        "never another member's ticket: {content}"
    );
    assert!(
        !content.contains("<#"),
        "empty view has no channel link: {content}"
    );
}

#[tokio::test]
async fn mytickets_single_open_renders_id_status_channel_and_claim() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    let chan = channel(1);
    seed_open(&store, "t-mine", "500", &chan, BASE_MILLIS).await;
    let before = state(db.pool()).await;
    commands
        .on_interaction(&member_slash(MYTICKETS, 500, Vec::new(), 1))
        .await;
    let requests = mock.requests();
    let after = state(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "member list mutates nothing");
    assert_read_only(&requests);
    let (content, _) = reply(&requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /mytickets is not wired"
    );
    assert!(content.contains("t-mine"), "shows ticket id: {content}");
    assert!(
        content.to_ascii_lowercase().contains("open"),
        "shows open status: {content}"
    );
    assert!(
        content.contains(&format!("<#{chan}>")),
        "usable Open channel links: {content}"
    );
    assert!(
        content.to_ascii_lowercase().contains("claim"),
        "shows claim state: {content}"
    );
    assert!(
        !content.contains("<@501>"),
        "unclaimed ticket names no claimant: {content}"
    );
}

#[tokio::test]
async fn mytickets_many_guild_tickets_shows_only_invoker() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    seed_open(&store, "t-mine", "500", &channel(1), BASE_MILLIS).await;
    for (index, opener) in (501..525).enumerate() {
        seed_open(
            &store,
            &format!("t-peer-{index}"),
            &opener.to_string(),
            &channel(10 + index as u64),
            BASE_MILLIS + (index as i64 + 1) * 1_000,
        )
        .await;
    }
    let before = state(db.pool()).await;
    commands
        .on_interaction(&member_slash(MYTICKETS, 500, Vec::new(), 1))
        .await;
    let requests = mock.requests();
    let after = state(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "member list mutates nothing");
    assert_read_only(&requests);
    let (content, _) = reply(&requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /mytickets is not wired"
    );
    assert!(content.contains("t-mine"), "shows own ticket: {content}");
    assert!(
        !content.contains("t-peer-"),
        "never another member's tickets: {content}"
    );
}

#[tokio::test]
async fn queue_pages_all_non_closed_exactly_once_at_capacity() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    // Fifty full rows cannot fit one 2000-unit reply for any truthful renderer
    // (id + opener + status + timestamp + claim state each), so paging is forced.
    for index in 0..50 {
        let opener = 500 + index;
        seed_open(
            &store,
            &format!("t-{index:02}"),
            &opener.to_string(),
            &channel(100 + index as u64),
            BASE_MILLIS + index as i64 * 1_000,
        )
        .await;
    }
    // One claimed row proves claim state survives paging.
    store.claim("t-07", "501").await.unwrap();
    // Closed rows are excluded from the queue.
    seed_closed(&store, "t-closed", "599", &channel(999), BASE_MILLIS).await;
    let before = state(db.pool()).await;
    // Reserve decisions before any list.
    let probe_active = store.get("t-00").await.unwrap().unwrap();
    let before_existing = decide_open(
        Some(&probe_active),
        Some(probe_active.created_at),
        BASE_MILLIS + 50_000,
        COOLDOWN_SECONDS,
    );
    let before_fresh = decide_open(None, None, BASE_MILLIS + 50_000, COOLDOWN_SECONDS);

    // Walk pages via the visible Page X of Y metadata (page size is the
    // builder's choice; the walk adapts to any size).
    let mut covered: Vec<String> = Vec::new();
    let mut total = 1usize;
    let mut first = true;
    let mut page_requests: Vec<Vec<RestRequest>> = Vec::new();
    for number in 1..=60 {
        let interaction = staff_slash(QUEUE, 501, vec![page_option(number)], 100 + number as u64);
        // Fresh mock per page keeps callbacks unambiguous; rebuild the harness
        // around the same database for each page.
        commands.on_interaction(&interaction).await;
        if first {
            let requests = mock.requests();
            let (content, _) = reply(&requests);
            assert_ne!(
                content, UNKNOWN_COMMAND_REPLY,
                "RED: /ticket-queue is not wired"
            );
            total = page_metadata(&content)
                .map(|(_, total)| total)
                .unwrap_or(1)
                .clamp(1, 50);
            assert!(total > 1, "50 rows cannot fit one reply: {content}");
            assert!(
                content.contains("<t:"),
                "staff queue localizes created-at: {content}"
            );
            assert!(
                !content.contains("t-closed"),
                "closed rows excluded: {content}"
            );
            page_requests.push(requests);
            for index in 0..50 {
                let id = format!("t-{index:02}");
                if content.contains(&id) {
                    covered.push(id);
                }
            }
            first = false;
            if total == 1 {
                break;
            }
            continue;
        }
        if number as usize > total {
            break;
        }
        // Later pages share the same mock; slice new requests since the last page.
        let requests = mock.requests();
        let prior: usize = page_requests.iter().map(Vec::len).sum();
        let fresh = requests[prior..].to_vec();
        let (content, _) = reply(&fresh);
        assert_eq!(
            page_metadata(&content),
            Some((number as usize, total)),
            "stable page totals: {content}"
        );
        assert!(
            !content.contains("t-closed"),
            "closed rows excluded on page {number}: {content}"
        );
        for index in 0..50 {
            let id = format!("t-{index:02}");
            if content.contains(&id) {
                covered.push(id);
            }
        }
        page_requests.push(fresh);
        if number as usize == total {
            break;
        }
    }
    // Past-end refuses privately.
    commands
        .on_interaction(&staff_slash(
            QUEUE,
            501,
            vec![page_option(total as i64 + 1)],
            500,
        ))
        .await;
    let all_requests = mock.requests();
    let prior: usize = page_requests.iter().map(Vec::len).sum();
    let past_end = all_requests[prior..].to_vec();
    let (past_content, _) = reply(&past_end);
    assert!(
        past_content.to_ascii_lowercase().contains("page") && past_content.contains('1'),
        "past-end names a valid page: {past_content}"
    );
    assert_refusal_bounded(&past_content);

    let after = state(db.pool()).await;
    // Reserve decisions after all lists.
    let probe_after = store.get("t-00").await.unwrap().unwrap();
    let after_existing = decide_open(
        Some(&probe_after),
        Some(probe_after.created_at),
        BASE_MILLIS + 50_000,
        COOLDOWN_SECONDS,
    );
    let after_fresh = decide_open(None, None, BASE_MILLIS + 50_000, COOLDOWN_SECONDS);
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "queue paging mutates nothing");
    assert_eq!(
        before_existing, after_existing,
        "reserve for active opener unchanged"
    );
    assert_eq!(
        before_fresh, after_fresh,
        "reserve for fresh opener unchanged"
    );
    for requests in &page_requests {
        assert_read_only(requests);
    }
    assert_read_only(&past_end);
    covered.sort();
    covered.dedup();
    assert_eq!(covered.len(), 50, "every non-closed row exactly once");
    for index in 0..50 {
        assert!(
            covered.contains(&format!("t-{index:02}")),
            "missing t-{index:02}"
        );
    }
}

#[tokio::test]
async fn every_status_renders_truthfully_and_closed_excluded() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    seed_creating(&store, "t-creating", "510", BASE_MILLIS).await;
    let open_chan = channel(21);
    seed_open(&store, "t-open", "511", &open_chan, BASE_MILLIS + 1_000).await;
    let closing_chan = channel(22);
    seed_closing(
        &store,
        "t-closing",
        "512",
        &closing_chan,
        BASE_MILLIS + 2_000,
    )
    .await;
    let cleanup_chan = channel(23);
    seed_cleanup(
        &store,
        "t-cleanup",
        "513",
        &cleanup_chan,
        BASE_MILLIS + 3_000,
    )
    .await;
    seed_closed(&store, "t-closed", "514", &channel(24), BASE_MILLIS + 4_000).await;
    let before = state(db.pool()).await;

    commands
        .on_interaction(&staff_slash(QUEUE, 501, vec![page_option(1)], 1))
        .await;
    let queue_requests = mock.requests();
    let (queue, _) = reply(&queue_requests);
    assert_ne!(
        queue, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue is not wired"
    );
    for (id, status) in [
        ("t-creating", "creating"),
        ("t-open", "open"),
        ("t-closing", "closing"),
        ("t-cleanup", "cleanup"),
    ] {
        assert!(queue.contains(id), "queue shows {id}: {queue}");
        assert!(
            queue.to_ascii_lowercase().contains(status),
            "queue renders {status} for {id}: {queue}"
        );
    }
    assert!(
        !queue.contains("t-closed"),
        "closed excluded from queue: {queue}"
    );
    assert!(
        queue.contains(&format!("<#{open_chan}>")),
        "Open + channel links: {queue}"
    );
    assert!(
        !queue.contains(&format!("<#{closing_chan}>")),
        "Closing renders no usable link: {queue}"
    );
    assert!(
        !queue.contains(&format!("<#{cleanup_chan}>")),
        "Cleanup renders no usable link: {queue}"
    );

    // Each member sees only their own row with the same truthful status.
    for (opener, id, status) in [
        (510u64, "t-creating", "creating"),
        (511, "t-open", "open"),
        (512, "t-closing", "closing"),
        (513, "t-cleanup", "cleanup"),
    ] {
        let prior = mock.requests().len();
        commands
            .on_interaction(&member_slash(MYTICKETS, opener, Vec::new(), 10 + opener))
            .await;
        let fresh = mock.requests()[prior..].to_vec();
        let (content, _) = reply(&fresh);
        assert_ne!(
            content, UNKNOWN_COMMAND_REPLY,
            "RED: /mytickets is not wired"
        );
        assert!(content.contains(id), "member {opener} sees {id}: {content}");
        assert!(
            content.to_ascii_lowercase().contains(status),
            "member {opener} sees {status}: {content}"
        );
        assert!(
            !content.contains("t-closed"),
            "closed excluded from member view: {content}"
        );
    }
    // Closed member sees empty, not their closed row.
    let prior = mock.requests().len();
    commands
        .on_interaction(&member_slash(MYTICKETS, 514, Vec::new(), 99))
        .await;
    let fresh = mock.requests()[prior..].to_vec();
    let (closed_member, _) = reply(&fresh);
    assert!(
        !closed_member.contains("t-closed"),
        "closed member sees no closed row: {closed_member}"
    );

    let after = state(db.pool()).await;
    let all = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "status renders mutate nothing");
    assert_read_only(&all);
}

#[tokio::test]
async fn opener_isolation_across_members_and_guilds() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    let foreign = TicketStore::new(db.pool().clone(), FOREIGN_GUILD.into()).unwrap();
    seed_open(&store, "t-a", "500", &channel(1), BASE_MILLIS).await;
    seed_open(&store, "t-b", "501", &channel(2), BASE_MILLIS + 1_000).await;
    seed_open(&foreign, "t-foreign-same", "500", &channel(3), BASE_MILLIS).await;
    seed_open(&foreign, "t-foreign-other", "502", &channel(4), BASE_MILLIS).await;
    let before = state(db.pool()).await;

    commands
        .on_interaction(&member_slash(MYTICKETS, 500, Vec::new(), 1))
        .await;
    let member_requests = mock.requests();
    let (member, _) = reply(&member_requests);
    assert_ne!(
        member, UNKNOWN_COMMAND_REPLY,
        "RED: /mytickets is not wired"
    );
    assert!(member.contains("t-a"), "member sees own: {member}");
    assert!(!member.contains("t-b"), "not peer's: {member}");
    assert!(!member.contains("t-foreign"), "not other guild's: {member}");

    let prior = mock.requests().len();
    commands
        .on_interaction(&staff_slash(QUEUE, 501, vec![page_option(1)], 2))
        .await;
    let queue_requests = mock.requests()[prior..].to_vec();
    let (queue, _) = reply(&queue_requests);
    assert_ne!(
        queue, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue is not wired"
    );
    assert!(
        queue.contains("t-a") && queue.contains("t-b"),
        "queue sees guild: {queue}"
    );
    assert!(
        !queue.contains("t-foreign"),
        "queue is guild-scoped: {queue}"
    );

    // Wrong-guild invocation stays silent (router Ignore, zero requests).
    let prior = mock.requests().len();
    let foreign_interaction = slash(
        MYTICKETS,
        FOREIGN_GUILD_ID,
        500,
        Vec::new(),
        Permissions::empty(),
        Vec::new(),
        3,
    );
    commands.on_interaction(&foreign_interaction).await;
    let foreign_requests = mock.requests()[prior..].to_vec();
    assert!(
        foreign_requests.is_empty(),
        "wrong guild stays silent: {foreign_requests:?}"
    );

    let after = state(db.pool()).await;
    let all = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "isolation checks mutate nothing");
    assert_read_only(&all);
}

#[tokio::test]
async fn tickets_disabled_guild_renders_private_guidance_with_zero_mutation() {
    // Refusal precedes storage: a never-connecting pool proves the gate runs first.
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-used pool");
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let executor = executor_for(&mock);
    let router = two_bot_core::InteractionRouter::new(two_bot_core::RouterGates {
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
    });
    let commands = CommandRuntime::new(pool, executor, router, GUILD_ID, false);
    for (name, interaction_id) in [(MYTICKETS, 1u64), (QUEUE, 2u64)] {
        let interaction = slash(
            name,
            GUILD_ID,
            500,
            Vec::new(),
            Permissions::empty(),
            Vec::new(),
            interaction_id,
        );
        let prior = mock.requests().len();
        commands.on_interaction(&interaction).await;
        let fresh = mock.requests()[prior..].to_vec();
        // Disabled must answer (not stay silent like a foreign guild) with a
        // tickets-specific refusal, never the generic unknown-command reply.
        assert_eq!(fresh.len(), 1, "disabled answers once: {fresh:?}");
        let (content, _) = reply(&fresh);
        assert_ne!(
            content, UNKNOWN_COMMAND_REPLY,
            "RED: /{name} has no tickets-disabled refusal"
        );
        assert!(
            content.to_ascii_lowercase().contains("ticket"),
            "disabled names tickets: {content}"
        );
        assert_refusal_bounded(&content);
    }
    let all = mock.requests();
    mock.shutdown().await;
    assert_read_only(&all);
}

#[tokio::test]
async fn store_outage_renders_private_retry_with_zero_mutation() {
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-connecting pool");
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let executor = executor_for(&mock);
    // TicketRuntime over a dead pool: construction stores the pool without connecting.
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            ticket_config(),
            HashMap::new(),
        )
        .unwrap(),
    );
    tickets.set_bot_id(BOT_ID);
    let commands = CommandRuntime::with_tickets(pool, executor, tickets);
    for (index, interaction) in [
        member_slash(MYTICKETS, 500, Vec::new(), 1),
        staff_slash(QUEUE, 501, vec![page_option(1)], 2),
    ]
    .into_iter()
    .enumerate()
    {
        let prior = mock.requests().len();
        commands.on_interaction(&interaction).await;
        let fresh = mock.requests()[prior..].to_vec();
        assert_eq!(fresh.len(), 1, "outage answers once: {fresh:?}");
        let (content, _) = reply(&fresh);
        assert_ne!(
            content, UNKNOWN_COMMAND_REPLY,
            "RED: list {index} has no store-outage reply"
        );
        let lower = content.to_ascii_lowercase();
        assert!(
            lower.contains("try again") || lower.contains("retry"),
            "outage names a retry: {content}"
        );
        assert_refusal_bounded(&content);
    }
    let all = mock.requests();
    mock.shutdown().await;
    assert_read_only(&all);
}

#[tokio::test]
async fn hostile_opener_ids_render_inert_and_mention_safe() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let pool = db.pool().clone();
    // Corrupt/hostile opener ids inserted directly: the queue must show their rows
    // without pings, injected lines or unbounded output.
    for (id, opener, created) in [
        ("h-everyone", "@everyone <@&300> **bold**", BASE_MILLIS),
        ("h-newline", "x\nINJECTED_LINE\n", BASE_MILLIS + 1_000),
        ("h-code", "`code` <@123> _ital_", BASE_MILLIS + 2_000),
    ] {
        sqlx::query(
            "INSERT INTO tickets (id, guild_id, opener_id, status, created_at) VALUES ($1, $2, $3, 'creating', $4)",
        )
        .bind(id)
        .bind(GUILD)
        .bind(opener)
        .bind(format_iso_millis(created))
        .execute(&pool)
        .await
        .unwrap();
    }
    let store = TicketStore::new(pool.clone(), GUILD.into()).unwrap();
    seed_open(&store, "t-normal", "500", &channel(50), BASE_MILLIS + 3_000).await;
    let before = state(&pool).await;
    commands
        .on_interaction(&staff_slash(QUEUE, 501, vec![page_option(1)], 1))
        .await;
    let requests = mock.requests();
    let after = state(&pool).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(before, after, "hostile list mutates nothing");
    assert_read_only(&requests);
    let (content, _) = reply(&requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue is not wired"
    );
    for id in ["h-everyone", "h-newline", "h-code", "t-normal"] {
        assert!(
            content.contains(id),
            "hostile rows still listed ({id}): {content}"
        );
    }
    assert!(
        !content.contains("\nINJECTED_LINE\n"),
        "embedded newlines sanitized: {content:?}"
    );
    assert!(
        !content.contains("x\nINJECTED_LINE"),
        "no raw newline payload: {content:?}"
    );
}

#[tokio::test]
async fn concurrent_reserve_during_list_stays_ordered_and_unmutated() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    for index in 0..5 {
        seed_open(
            &store,
            &format!("t-{index}"),
            &(500 + index).to_string(),
            &channel(200 + index as u64),
            BASE_MILLIS + index as i64 * 1_000,
        )
        .await;
    }
    let before = state(db.pool()).await;
    // Race a fresh reserve against a staff queue list.
    let racing_store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    let reserve = async move {
        racing_store
            .reserve("t-race", "599", BASE_MILLIS + 50_000, COOLDOWN_SECONDS)
            .await
            .unwrap()
    };
    let list = async {
        commands
            .on_interaction(&staff_slash(QUEUE, 501, vec![page_option(1)], 1))
            .await;
    };
    let (reserve_outcome, ()) = tokio::join!(reserve, list);
    assert!(
        matches!(reserve_outcome, OpenResult::Created(_)),
        "racing reserve creates: {reserve_outcome:?}"
    );
    let requests = mock.requests();
    let after = state(db.pool()).await;
    db.close().await.unwrap();
    mock.shutdown().await;

    // List added nothing: after == before + the racing row only.
    assert_eq!(
        after.0.len(),
        before.0.len() + 1,
        "only the racing reserve adds a row"
    );
    assert!(
        after.0.iter().any(|row| row.0 == "t-race"),
        "racing row present"
    );
    assert_eq!(after.1, before.1, "no transcripts from list/racing reserve");
    assert_eq!(
        (after.2, after.3, after.4),
        (before.2, before.3, before.4),
        "no audits from list"
    );
    assert_read_only(&requests);
    let (content, _) = reply(&requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue is not wired"
    );
    // Snapshot is either side of the race, but never duplicated or corrupt.
    for index in 0..5 {
        let id = format!("t-{index}");
        assert_eq!(
            content.matches(&id).count(),
            1,
            "steady rows appear exactly once: {content}"
        );
    }
    assert!(
        content.matches("t-race").count() <= 1,
        "racing row at most once: {content}"
    );
    // Order follows recoverable(): created_at then id for the steady rows.
    let positions: Vec<usize> = (0..5)
        .map(|index| {
            content
                .find(&format!("t-{index}"))
                .expect("steady row present")
        })
        .collect();
    let mut ordered = positions.clone();
    ordered.sort_unstable();
    assert_eq!(
        positions, ordered,
        "queue stays ordered under race: {content}"
    );
}

#[tokio::test]
async fn lists_preserve_claim_close_and_reserve_behavior() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    seed_open(&store, "t-work", "500", &channel(1), BASE_MILLIS).await;
    let before = state(db.pool()).await;
    commands
        .on_interaction(&member_slash(MYTICKETS, 500, Vec::new(), 1))
        .await;
    let member_requests = mock.requests();
    let (member_content, _) = reply(&member_requests);
    assert_ne!(
        member_content, UNKNOWN_COMMAND_REPLY,
        "RED: /mytickets is not wired"
    );
    assert!(
        member_content.contains("t-work"),
        "member sees own before claim/close: {member_content}"
    );
    let prior = mock.requests().len();
    commands
        .on_interaction(&staff_slash(QUEUE, 501, vec![page_option(1)], 2))
        .await;
    let queue_requests = mock.requests()[prior..].to_vec();
    let (queue_content, _) = reply(&queue_requests);
    assert_ne!(
        queue_content, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue is not wired"
    );
    let mid = state(db.pool()).await;
    assert_eq!(before, mid, "lists mutate nothing");
    // Claim/close receipts still work after lists (store-level proof that list
    // paths leave rows, locks and decisions untouched).
    store.claim("t-work", "501").await.unwrap();
    let claimed = store.get("t-work").await.unwrap().unwrap();
    assert_eq!(claimed.claimed_by.as_deref(), Some("501"));
    store
        .begin_close("t-work", BASE_MILLIS + 5_000)
        .await
        .unwrap();
    let closing = store.get("t-work").await.unwrap().unwrap();
    assert_eq!(closing.status, two_bot_core::tickets::TicketStatus::Closing);
    // Active opener still refuses a second reserve with the same decision.
    let refuse = store
        .reserve("t-second", "500", BASE_MILLIS + 6_000, COOLDOWN_SECONDS)
        .await
        .unwrap();
    assert!(
        matches!(refuse, OpenResult::Refused(_)),
        "active opener still refused after lists: {refuse:?}"
    );
    let requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;
    assert_read_only(&requests);
}

#[tokio::test]
async fn invalid_queue_pages_refuse_privately_without_mutation() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let (_tickets, commands) = harness(&db, &mock).await;
    let store = TicketStore::new(db.pool().clone(), GUILD.into()).unwrap();
    seed_open(&store, "t-one", "500", &channel(1), BASE_MILLIS).await;
    let before = state(db.pool()).await;
    let invalid: Vec<Vec<CommandDataOption>> = vec![
        vec![page_option(0)],
        vec![page_option(-1)],
        vec![page_option(i64::MIN)],
        vec![page_option(i64::MAX)],
        vec![page_option(50)],
        vec![CommandDataOption {
            name: "page".to_owned(),
            value: CommandOptionValue::String("2".to_owned()),
        }],
        vec![CommandDataOption {
            name: "page".to_owned(),
            value: CommandOptionValue::String("@everyone invalid".to_owned()),
        }],
        vec![CommandDataOption {
            name: "page".to_owned(),
            value: CommandOptionValue::Number(1.5),
        }],
        vec![CommandDataOption {
            name: "page".to_owned(),
            value: CommandOptionValue::Boolean(true),
        }],
    ];
    for (index, options) in invalid.into_iter().enumerate() {
        let prior = mock.requests().len();
        commands
            .on_interaction(&staff_slash(QUEUE, 501, options, 100 + index as u64))
            .await;
        let fresh = mock.requests()[prior..].to_vec();
        let (content, _) = reply(&fresh);
        assert_ne!(
            content, UNKNOWN_COMMAND_REPLY,
            "RED: /ticket-queue has no invalid-page refusal"
        );
        assert!(
            content.to_ascii_lowercase().contains("page"),
            "invalid page explains pages: {content}"
        );
        assert!(
            content.contains('1'),
            "invalid page names a valid start: {content}"
        );
        assert!(
            !content.contains("invalid-page") && !content.contains("@everyone invalid"),
            "never echo malformed input: {content}"
        );
        assert!(
            !content.contains("t-one"),
            "refusal serves no rows: {content}"
        );
        assert_refusal_bounded(&content);
    }
    let after = state(db.pool()).await;
    let all = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;
    assert_eq!(before, after, "invalid pages mutate nothing");
    assert_read_only(&all);
}

#[tokio::test]
async fn non_staff_queue_attempt_refuses_before_store() {
    // Staff gate precedes storage: a never-connecting pool proves auth runs first.
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-used pool");
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let executor = executor_for(&mock);
    let tickets = Arc::new(
        TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            ticket_config(),
            HashMap::new(),
        )
        .unwrap(),
    );
    tickets.set_bot_id(BOT_ID);
    let commands = CommandRuntime::with_tickets(pool, executor, tickets);
    // No staff role, no ManageChannels, no Administrator.
    let interaction = member_slash(QUEUE, 500, vec![page_option(1)], 1);
    commands.on_interaction(&interaction).await;
    let requests = mock.requests();
    mock.shutdown().await;

    assert_eq!(requests.len(), 1, "staff refusal answers once");
    assert_read_only(&requests);
    let (content, _) = reply(&requests);
    assert_ne!(
        content, UNKNOWN_COMMAND_REPLY,
        "RED: /ticket-queue has no staff refusal"
    );
    assert!(
        content.to_ascii_lowercase().contains("staff"),
        "non-staff queue names the staff gate: {content}"
    );
    assert_refusal_bounded(&content);
}
