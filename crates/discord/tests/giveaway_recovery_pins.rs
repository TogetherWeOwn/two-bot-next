//! Builder pins for the giveaway review hardening: each test kills the
//! revert mutant of one round-1 review fix.
//!
//! The protected `giveaway_timed_draw_acceptance` corpus cannot produce the
//! discriminating inputs (its member table answers only 200/404 and every
//! write succeeds), and the builder must never edit that file. These tests
//! re-create the same MockRest + disposable-database shape in a
//! builder-owned module: a member outage aborts the draw instead of
//! shrinking it, a two-winner single reroll keeps the full winner set, a
//! corrupt row holds its claim while later-due rows still draw, and a
//! refused or pending entry message never wedges the winners post. The
//! reroll recovery pin also checks that the interaction reopens the pending
//! marker before a refused winners post; the store-level atomicity pin lives
//! in `crates/core/tests/giveaway_store.rs`.

#[allow(dead_code)]
mod common;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{Pool, Postgres, Row};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_GUILD, giveaway_store, ClassifierConfig, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    giveaway::GiveawayJobError, interactions::InteractionRuntime, ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060801";
const CHANNEL: &str = "1550102030405060802";
const MOD: &str = "1550102030405060811";
const ALICE: &str = "1550102030405060821";
const BOB: &str = "1550102030405060822";
const CAROL: &str = "1550102030405060823";

fn router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
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
    })
}

fn slash(id: u64, user: &str, name: &str, permissions: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-giveaway-token", "type": 2,
        "version": 1, "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": permissions.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": user, "username": "member", "discriminator": "0"}},
        "data": {"id": "4444", "name": name, "type": 1, "options": options,
            "resolved": {"users": {
                MOD: {"id": MOD, "username": "mod", "discriminator": "0"},
                ALICE: {"id": ALICE, "username": "alice", "discriminator": "0"},
                BOB: {"id": BOB, "username": "bob", "discriminator": "0"},
                CAROL: {"id": CAROL, "username": "carol", "discriminator": "0"}
            }}}
    }))
    .expect("wire interaction")
}

fn button(id: u64, user: &str, custom_id: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-giveaway-token", "type": 3,
        "version": 1, "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": "0", "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": user, "username": "member", "discriminator": "0"}},
        "data": {"custom_id": custom_id, "component_type": 2}
    }))
    .expect("wire component interaction")
}

fn subcommand(name: &str, options: Value) -> Value {
    json!([{"name": name, "type": 1, "options": options}])
}

fn create_options(prize: &str, duration_minutes: i64, winners: i64) -> Value {
    subcommand(
        "create",
        json!([
            {"name": "prize", "type": 3, "value": prize},
            {"name": "duration-minutes", "type": 4, "value": duration_minutes},
            {"name": "winners", "type": 4, "value": winners},
            {"name": "channel", "type": 7, "value": CHANNEL},
        ]),
    )
}

fn id_options(sub: &str, giveaway: &str) -> Value {
    subcommand(sub, json!([{"name": "id", "type": 3, "value": giveaway}]))
}

fn single_reroll_options(giveaway: &str, user: &str) -> Value {
    subcommand(
        "reroll",
        json!([
            {"name": "id", "type": 3, "value": giveaway},
            {"name": "user", "type": 6, "value": user},
        ]),
    )
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy(
        "mock-token-not-a-credential".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

fn runtime(pool: &Pool<Postgres>, mock: &MockRest) -> InteractionRuntime {
    InteractionRuntime::with_router(
        router(),
        pool.clone(),
        executor(mock),
        0,
        ClassifierConfig::default(),
    )
}

#[derive(Clone)]
enum MemberAnswer {
    Current(Vec<String>),
    Outage,
}

/// Path-routed mock: interaction callbacks 204, membership reads per the
/// table (unlisted members read back as current with no roles), every other
/// write an ID-bearing 200 receipt. When `fail_edits` is set, every PATCH
/// answers 404, simulating a deleted entry message.
async fn start_mock(
    members: HashMap<String, MemberAnswer>,
    fail_edits: Arc<AtomicBool>,
) -> MockRest {
    start_mock_with_post_failure(members, fail_edits, Arc::new(AtomicBool::new(false))).await
}

/// Refuse only channel posts, not the interaction callback, response edit,
/// or entry-message edit. Toggle after round zero has actually announced.
async fn start_mock_with_post_failure(
    members: HashMap<String, MemberAnswer>,
    fail_edits: Arc<AtomicBool>,
    fail_posts: Arc<AtomicBool>,
) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        if request.method == "POST"
            && request.path == channel_path()
            && fail_posts.load(Ordering::SeqCst)
        {
            return ScriptedResponse::status(403);
        }
        if request.path.contains("/interactions/") {
            return ScriptedResponse::status(204);
        }
        if request.method == "PATCH" && fail_edits.load(Ordering::SeqCst) {
            return ScriptedResponse::status(404);
        }
        if let Some(user) = request
            .path
            .strip_prefix("/api/v10/guilds/")
            .and_then(|rest| rest.strip_prefix(GUILD))
            .and_then(|rest| rest.strip_prefix("/members/"))
        {
            match members.get(user) {
                Some(MemberAnswer::Current(roles)) => {
                    return ScriptedResponse::json(
                        200,
                        json!({"user": {"id": user}, "roles": roles}),
                    );
                }
                Some(MemberAnswer::Outage) => return ScriptedResponse::status(500),
                None => {
                    return ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": []}));
                }
            }
        }
        ScriptedResponse::json(200, json!({"id": "99"}))
    })
    .await
}

struct Fixture {
    db: TestDatabase,
}

impl Fixture {
    fn pool(&self) -> &Pool<Postgres> {
        self.db.pool()
    }

    async fn close(self) {
        self.db.close().await.expect("drop test database");
    }
}

async fn pool() -> Option<Fixture> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("skipped giveaway recovery pins: TWO_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable test database; no credential fallback");
    Some(Fixture { db })
}

async fn handled(pool: &Pool<Postgres>, mock: &MockRest, interaction: &Interaction, what: &str) {
    let ok = runtime(pool, mock)
        .handle(interaction)
        .await
        .expect("handler executes");
    assert!(ok, "{what}: giveaway interaction must be handled");
}

async fn create_giveaway(
    pool: &Pool<Postgres>,
    mock: &MockRest,
    id: u64,
    prize: &str,
    duration_minutes: i64,
    winners: i64,
) -> String {
    let interaction = slash(
        id,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        create_options(prize, duration_minutes, winners),
    );
    handled(pool, mock, &interaction, "create").await;
    let found: String =
        sqlx::query_scalar("SELECT id FROM giveaways WHERE guild_id = $1 AND prize = $2")
            .bind(GUILD)
            .bind(prize)
            .fetch_one(pool)
            .await
            .expect("created giveaway row");
    found
}

async fn enter(pool: &Pool<Postgres>, mock: &MockRest, id: u64, user: &str, giveaway: &str) {
    let interaction = button(id, user, &format!("gw:{giveaway}"));
    handled(pool, mock, &interaction, "entry").await;
}

async fn force_due(pool: &Pool<Postgres>, giveaway: &str, minutes_ago: i64) {
    // Distinct bound offsets order rows for the due scan.
    sqlx::query(
        "UPDATE giveaways SET ends_at = now() - ($3::bigint * interval '1 minute') \
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(giveaway)
    .bind(minutes_ago)
    .execute(pool)
    .await
    .expect("force due");
}

struct GiveawayRow {
    message_id: String,
    claim_open: bool,
    announced: bool,
}

async fn giveaway_row(pool: &Pool<Postgres>, giveaway: &str) -> GiveawayRow {
    let row = sqlx::query(
        "SELECT message_id::text AS message_id, \
                claim_token IS NOT NULL AS claim_open, \
                announced_at IS NOT NULL AS announced \
         FROM giveaways WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(giveaway)
    .fetch_one(pool)
    .await
    .expect("read giveaway row");
    GiveawayRow {
        message_id: row.get("message_id"),
        claim_open: row.get("claim_open"),
        announced: row.get("announced"),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DrawRow {
    draw_no: i64,
    seed: String,
    winners: Vec<String>,
    reroll_kind: Option<String>,
}

async fn draw_rows(pool: &Pool<Postgres>, giveaway: &str) -> Vec<DrawRow> {
    let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT draw_no::bigint, seed::text, winner_ids::text, reroll_kind::text \
         FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2 ORDER BY draw_no",
    )
    .bind(GUILD)
    .bind(giveaway)
    .fetch_all(pool)
    .await
    .expect("read draw rows");
    rows.into_iter()
        .map(|(draw_no, seed, winners, reroll_kind)| DrawRow {
            draw_no,
            seed,
            winners: serde_json::from_str(&winners).expect("winner ids parse"),
            reroll_kind,
        })
        .collect()
}

fn channel_path() -> String {
    format!("/api/v10/channels/{CHANNEL}/messages")
}

fn message_path(message_id: &str) -> String {
    format!("/api/v10/channels/{CHANNEL}/messages/{message_id}")
}

fn live_edits() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[tokio::test]
async fn member_outage_aborts_the_tick_with_no_draw_row() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mut members = HashMap::new();
    members.insert(ALICE.to_owned(), MemberAnswer::Current(Vec::new()));
    members.insert(BOB.to_owned(), MemberAnswer::Outage);
    let mock = start_mock(members, live_edits()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 101, "Outage Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 102, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 103, BOB, &gid).await;
    force_due(fixture.pool(), &gid, 1).await;
    let outcome = runtime(fixture.pool(), &mock).tick_giveaways().await;
    assert!(
        matches!(outcome, Err(GiveawayJobError::Rest(_))),
        "a 500 member read must abort the draw, never shrink it: {outcome:?}"
    );
    assert!(
        draw_rows(fixture.pool(), &gid).await.is_empty(),
        "an aborted draw commits no row"
    );
    assert!(
        !giveaway_row(fixture.pool(), &gid).await.claim_open,
        "the aborted draw releases its claim for the next tick"
    );
    let member_gets = mock
        .requests()
        .iter()
        .filter(|request| {
            request.method == "GET" && request.path.ends_with(&format!("/members/{BOB}"))
        })
        .count();
    assert!(
        member_gets >= 2,
        "the outage is retried, not read once and excluded"
    );
    fixture.close().await;
}

#[tokio::test]
async fn single_reroll_on_two_winners_keeps_the_full_set() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock(HashMap::new(), live_edits()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 201, "Two Seat Prize", 60, 2).await;
    enter(fixture.pool(), &mock, 202, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 203, BOB, &gid).await;
    enter(fixture.pool(), &mock, 204, CAROL, &gid).await;
    let end = slash(
        205,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid),
    );
    handled(fixture.pool(), &mock, &end, "end").await;
    let first = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].winners.len(), 2);
    let target = first[0].winners[0].clone();
    let kept = first[0].winners[1].clone();
    let single = slash(
        206,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        single_reroll_options(&gid, &target),
    );
    handled(fixture.pool(), &mock, &single, "reroll single").await;
    let second = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(second.len(), 2);
    assert_eq!(second[0], first[0], "reroll leaves the old row intact");
    assert_eq!(second[1].draw_no, 1);
    assert_eq!(
        second[1].reroll_kind,
        Some(format!("single:{target}")),
        "the round records its kind"
    );
    // The replacement pool holds only the non-winner, so the outcome is
    // deterministic: the kept seat survives and the target seat turns over.
    assert_eq!(
        second[1].winners.len(),
        2,
        "a single reroll stores the full next set, not the replacement alone"
    );
    assert!(
        second[1].winners.contains(&kept),
        "the kept winner survives the reroll"
    );
    assert!(
        !second[1].winners.contains(&target),
        "the replaced winner leaves the set"
    );
    let again = slash(
        207,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        single_reroll_options(&gid, &kept),
    );
    handled(fixture.pool(), &mock, &again, "reroll kept").await;
    let third = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(
        third.len(),
        3,
        "the kept winner stays a valid reroll target"
    );
    assert_eq!(third[2].winners.len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn reroll_reopens_pending_announcement_and_recovers_the_stored_round() {
    let Some(fixture) = pool().await else {
        return;
    };
    let fail_posts = Arc::new(AtomicBool::new(false));
    let mock = start_mock_with_post_failure(HashMap::new(), live_edits(), fail_posts.clone()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 601, "Reroll Recovery Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 602, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 603, BOB, &gid).await;
    let end = slash(
        604,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid),
    );
    handled(fixture.pool(), &mock, &end, "end").await;
    let first = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(first.len(), 1);
    assert!(
        giveaway_row(fixture.pool(), &gid).await.announced,
        "round zero must be announced so its stamp cannot hide a lost reroll"
    );
    assert!(giveaway_store::list_unannounced(fixture.pool(), GUILD, 25)
        .await
        .expect("initial recovery sweep")
        .is_empty());

    let before_reroll = mock.requests().len();
    fail_posts.store(true, Ordering::SeqCst);
    let reroll = slash(
        605,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("reroll", &gid),
    );
    handled(fixture.pool(), &mock, &reroll, "reroll with refused post").await;
    let pending_draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(
        pending_draws.len(),
        2,
        "reroll commits despite the refused post"
    );
    assert_eq!(pending_draws[0], first[0], "round zero stays intact");
    assert_eq!(pending_draws[1].draw_no, 1);
    assert_eq!(pending_draws[1].reroll_kind.as_deref(), Some("full"));
    assert!(
        !giveaway_row(fixture.pool(), &gid).await.announced,
        "the command must reopen the marker, not retain round zero's stamp"
    );
    let pending = giveaway_store::list_unannounced(fixture.pool(), GUILD, 25)
        .await
        .expect("pending reroll recovery sweep");
    assert_eq!(pending.len(), 1, "the refused reroll remains recoverable");
    assert_eq!(pending[0].id, gid);
    let requests = mock.requests();
    let refused_post = requests[before_reroll..]
        .iter()
        .find(|request| request.method == "POST" && request.path == channel_path())
        .expect("reroll attempted the winners post");
    assert!(
        String::from_utf8_lossy(&refused_post.body).contains(&pending_draws[1].seed),
        "the refused post belongs to the newly stored round"
    );
    let refused_body = refused_post.body.clone();

    // A fresh runtime represents restart: only durable rows carry recovery.
    let before_recovery = requests.len();
    fail_posts.store(false, Ordering::SeqCst);
    let summary = runtime(fixture.pool(), &mock)
        .tick_giveaways()
        .await
        .expect("retry the stored reroll announcement");
    assert_eq!(summary.drawn, 0, "recovery must never draw another round");
    assert_eq!(summary.announced, 1);
    assert_eq!(
        draw_rows(fixture.pool(), &gid).await,
        pending_draws,
        "recovery preserves both rounds, their seeds and their winners"
    );
    assert!(giveaway_row(fixture.pool(), &gid).await.announced);
    assert!(giveaway_store::list_unannounced(fixture.pool(), GUILD, 25)
        .await
        .expect("settled recovery sweep")
        .is_empty());
    let recovered_requests = mock.requests();
    let posts: Vec<_> = recovered_requests[before_recovery..]
        .iter()
        .filter(|request| request.method == "POST" && request.path == channel_path())
        .collect();
    assert_eq!(posts.len(), 1, "recovery delivers one winners post");
    assert_eq!(
        posts[0].body, refused_body,
        "recovery replays the stored announcement"
    );
    fixture.close().await;
}

#[tokio::test]
async fn invalid_requirements_hold_the_claim_while_later_rows_draw() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock(HashMap::new(), live_edits()).await;
    let broken = create_giveaway(
        fixture.pool(),
        &mock,
        701,
        "Damaged Requirements Prize",
        60,
        1,
    )
    .await;
    enter(fixture.pool(), &mock, 702, ALICE, &broken).await;
    let healthy = create_giveaway(
        fixture.pool(),
        &mock,
        703,
        "Valid Requirements Prize",
        60,
        1,
    )
    .await;
    enter(fixture.pool(), &mock, 704, BOB, &healthy).await;
    force_due(fixture.pool(), &broken, 2).await;
    force_due(fixture.pool(), &healthy, 1).await;
    sqlx::query(
        "UPDATE giveaways SET requirements_json = '{\"roles\":\"damaged\"}'::jsonb \
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(&broken)
    .execute(fixture.pool())
    .await
    .expect("damage requirements without changing the valid nonce");
    let outcome = runtime(fixture.pool(), &mock).tick_giveaways().await;
    assert!(
        matches!(outcome, Err(GiveawayJobError::Announce)),
        "damaged requirements fail the tick loudly: {outcome:?}"
    );
    assert!(draw_rows(fixture.pool(), &broken).await.is_empty());
    assert!(
        giveaway_row(fixture.pool(), &broken).await.claim_open,
        "the invalid-requirements branch retains its claim until lease expiry"
    );
    let healthy_draws = draw_rows(fixture.pool(), &healthy).await;
    assert_eq!(healthy_draws.len(), 1, "later due rows are not starved");
    assert_eq!(healthy_draws[0].winners, vec![BOB.to_owned()]);
    assert!(
        giveaway_row(fixture.pool(), &healthy).await.announced,
        "the later-due giveaway still announces in the same tick"
    );
    fixture.close().await;
}

#[tokio::test]
async fn corrupt_row_holds_its_claim_while_later_rows_draw() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock(HashMap::new(), live_edits()).await;
    let broken = create_giveaway(fixture.pool(), &mock, 301, "Corrupt Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 302, ALICE, &broken).await;
    let healthy = create_giveaway(fixture.pool(), &mock, 303, "Healthy Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 304, BOB, &healthy).await;
    force_due(fixture.pool(), &broken, 2).await;
    force_due(fixture.pool(), &healthy, 1).await;
    sqlx::query("UPDATE giveaways SET draw_nonce = 'damaged' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&broken)
        .execute(fixture.pool())
        .await
        .expect("corrupt the nonce");
    let outcome = runtime(fixture.pool(), &mock).tick_giveaways().await;
    assert!(
        matches!(outcome, Err(GiveawayJobError::Announce)),
        "the corrupt row fails the tick loudly: {outcome:?}"
    );
    assert!(
        draw_rows(fixture.pool(), &broken).await.is_empty(),
        "the corrupt row draws nothing"
    );
    assert!(
        giveaway_row(fixture.pool(), &broken).await.claim_open,
        "the corrupt row holds its claim until lease expiry, \
         so the batch moves past it instead of reclaiming it"
    );
    assert_eq!(
        draw_rows(fixture.pool(), &healthy).await.len(),
        1,
        "the later-due giveaway still draws in the same tick"
    );
    assert!(
        giveaway_row(fixture.pool(), &healthy).await.announced,
        "the later-due giveaway still announces"
    );
    fixture.close().await;
}

#[tokio::test]
async fn announce_survives_a_definitively_refused_message_edit() {
    let Some(fixture) = pool().await else {
        return;
    };
    let fail_edits = live_edits();
    let mock = start_mock(HashMap::new(), fail_edits.clone()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 401, "Deleted Message Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 402, ALICE, &gid).await;
    force_due(fixture.pool(), &gid, 1).await;
    let before = mock.requests().len();
    fail_edits.store(true, Ordering::SeqCst);
    let outcome = runtime(fixture.pool(), &mock).tick_giveaways().await;
    assert!(
        outcome.is_ok(),
        "a refused edit must not wedge the announce: {outcome:?}"
    );
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert!(row.announced, "the draw still marks announced");
    let window = mock.requests();
    let window = &window[before..];
    assert!(
        window
            .iter()
            .any(|request| request.method == "PATCH"
                && request.path == message_path(&row.message_id)),
        "the edit was attempted and refused"
    );
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    let seed = draws[0].seed.clone();
    let winner = draws[0].winners[0].clone();
    assert!(
        window.iter().any(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(&seed)
        }),
        "the winners post is delivered despite the refused edit"
    );
    let post = window
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(&seed)
        })
        .expect("winners post present");
    assert!(
        String::from_utf8_lossy(&post.body).contains(&format!("<@{winner}>")),
        "the winners post mentions the winner"
    );
    fixture.close().await;
}

#[tokio::test]
async fn announce_skips_a_pending_entry_message() {
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock(HashMap::new(), live_edits()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 501, "Pending Message Prize", 60, 1).await;
    enter(fixture.pool(), &mock, 502, ALICE, &gid).await;
    force_due(fixture.pool(), &gid, 1).await;
    sqlx::query("UPDATE giveaways SET message_id = 'pending' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&gid)
        .execute(fixture.pool())
        .await
        .expect("leave the message unconfirmed");
    let before = mock.requests().len();
    let outcome = runtime(fixture.pool(), &mock).tick_giveaways().await;
    assert!(
        outcome.is_ok(),
        "a pending message must not wedge the announce: {outcome:?}"
    );
    assert!(
        giveaway_row(fixture.pool(), &gid).await.announced,
        "the draw still marks announced"
    );
    let window = mock.requests();
    let window = &window[before..];
    assert!(
        !window.iter().any(|request| request.method == "PATCH"),
        "pending rows skip the edit entirely"
    );
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    let seed = draws[0].seed.clone();
    assert!(
        window.iter().any(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(&seed)
        }),
        "the winners post is delivered for a pending row"
    );
    fixture.close().await;
}
