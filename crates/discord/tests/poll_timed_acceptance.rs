//! Acceptance: timed poll with auto-close and results.
//!
//! Protected test file: the paired build card implements against these tests
//! and must never edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! 1. Migration: the next free global number in `crates/cutover/migrations/`
//!    (the glob below tolerates any number, but the file name must contain
//!    `poll`). Additive and idempotent (`IF NOT EXISTS`): creates `polls`
//!    and `poll_votes` per this spec. `poll_votes` carries
//!    `PRIMARY KEY (guild_id, poll_id, user_id)` so one member holds exactly
//!    one vote per poll and a change is an upsert, never a second row.
//! 2. Schema (superset-tolerant: extra columns are fine, missing pinned ones
//!    fail):
//!    - `polls`: `guild_id`, `id`, `question`, `options_json` (JSON array of
//!      option strings in button order), `channel_id`, `message_id`,
//!      `created_by`, `ends_at`, `status` (`open` or `closed`), `max_voters`
//!      (nullable: explicit per-poll voter cap), `winner_index` (nullable:
//!      set at close, NULL when zero votes were cast), `closed_at`
//!      (nullable), `results_posted_at` (nullable: set once the results
//!      message is posted), `created_at`, `updated_at`.
//!    - `poll_votes`: `guild_id`, `poll_id`, `user_id`, `option_index`
//!      (0-based into `options_json`), `voted_at`.
//! 3. Bounds: question 1-256 chars after trimming; 2-10 options, each 1-80
//!    chars after trimming (80 is Discord's button-label cap); duration
//!    1-10080 minutes (1 minute to 7 days); at most 10 open polls per guild
//!    and 3 open polls per author (quotas count `status = 'open'` only);
//!    NULL `max_voters` means 1000 distinct voters, and an explicit cap
//!    overrides that default. The (max+1)-th distinct voter is refused;
//!    existing voters may change their vote at either cap. Vote
//!    button `custom_id` is `poll:<poll-id>:<option-index>` and is at most
//!    100 chars (Discord's custom-id cap). Out-of-bound creates and votes
//!    are refused with zero stored state.
//! 4. Slash surface: top-level command `poll` with type-1 subcommand options
//!    `create` and `close`. Argument names: `question` (string), `option1`
//!    .. `option10` (strings; `option1`/`option2` required), `duration-minutes`
//!    (integer minutes, required on create), `id` (poll id string, required
//!    on close).
//! 5. Vote buttons key on the `poll:` custom-id prefix alone; any guild
//!    member may vote (no permission gate on the component). Status must be
//!    `open`, `ends_at > now`, and the index in range (expiry refuses new
//!    votes and changes even before the next tick); unknown poll ids are
//!    refused inside the handler with no row (the interaction is still acknowledged);
//!    malformed custom ids never panic and never write. Interactions whose
//!    voting user carries `bot: true` are never counted (refused at write,
//!    so no bot row can exist for results to filter).
//! 6. `COMMAND_PERMISSIONS` row: `poll`, `BuiltinSlash`, `0` (anyone may run
//!    a poll), no policy hook. The registry publishes top-level `poll` with
//!    `default_member_permissions == None` and `create`/`close` subcommands.
//!    Gates: polls ride the existing `announcements` gate. No new
//!    `RouterGates`/`SurfaceFlags` field: this file constructs gates
//!    literally and cannot adapt to one.
//! 7. Tick seam: `pub async fn tick_polls(&self) -> Result<O, E>` as an
//!    inherent method on `InteractionRuntime`, with `O: Debug` and
//!    `E: Debug`. The exact zero-argument shape matters: these tests call it
//!    unqualified through the baseline adapter below, and the inherent
//!    method shadows the adapter only when the signature matches. The tick
//!    scans `status = open AND ends_at <= now`, flips each due poll to
//!    `closed` atomically (losers of a raced flip skip the post), sets
//!    `winner_index`/`closed_at`, edits the poll message (buttons disabled),
//!    posts the results embed, then sets `results_posted_at`. A `closed` row
//!    with `results_posted_at IS NULL` posts results from the stored votes
//!    and never re-flips; concurrent recoverers post once between them.
//!    An idle tick makes no Discord calls. Supervisor
//!    wiring (shared supervisor, sub-minute cadence, Skip on missed ticks)
//!    is the builder's, proven by staging evidence.
//! 8. Results content: one embed carrying every option's count; the winner
//!    is the most votes, ties broken to the lowest option index; zero votes
//!    still posts (zero counts, `winner_index` NULL, no winner mention).
//!    Mention-safe: poll and results messages carry
//!    `allowed_mentions: {"parse": []}` and their bodies contain no
//!    `@everyone`/`@here` substring. On every close the poll-message edit
//!    precedes the results post.
//! 9. Live counts: each accepted vote edits the poll message with updated
//!    counts before the vote ack settles.
//! 10. Author close: `/poll close` by `created_by` shares the results path
//!     (same observable shape as auto-close: edit, one results post,
//!     `winner_index` set). A non-author member's close is refused with no
//!     state change; closing twice still posts exactly one results message.
//! 11. Every refusal below still acknowledges the interaction: `handle()`
//!     returns `true` with zero stored state, except malformed custom ids
//!     whose ack is unpinned (no-write and no-panic only).
//!
//! Deliberately unpinned: reply copy and ack shapes (deferred or immediate);
//! exact embed layout beyond §8; the poll id format; a list/query surface; moderator
//! close-override (only member non-author refusal is pinned); the
//! post-then-flag crash window (at-least-once edge: tests pin the
//! crash-before-post recovery); countdown display; vote-change history;
//! vote retention after member departure.
//!
//! Test shapes: offline contract tests (migration glob, permission row,
//! registry) plus MockRest + disposable-database acceptance driven through
//! `InteractionRuntime::handle` (slash by command name, votes by `custom_id`)
//! and the tick seam above. Raw SQL strings only, so every test compiles on
//! the baseline and fails on assertions. DB tests skip vacuously without
//! `TWO_TEST_DATABASE_URL`; CI's integration step sets it. Expiry is
//! SQL-aged `ends_at` against the database clock, the deterministic
//! equivalent of an injected clock given the runtime has no clock seam.
//! Fresh `InteractionRuntime` instances stand in for process restarts:
//! durability comes from the rows, never from memory.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{Pool, Postgres, Row};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    command_permissions::{command_permission, CommandSurface},
    commands::PERM_MANAGE_GUILD,
    ClassifierConfig, InteractionRouter, RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1660102030405060701";
const CHANNEL: &str = "1660102030405060702";
const ALICE: &str = "1660102030405060721";
const BOB: &str = "1660102030405060722";
const CAROL: &str = "1660102030405060723";
const DAVE: &str = "1660102030405060724";
const BOT_VOTER: &str = "1660102030405060741";

/// Pinned quotas (contract §3).
const MAX_OPEN_PER_GUILD: usize = 10;
const MAX_OPEN_PER_AUTHOR: usize = 3;
const MAX_OPTIONS: usize = 10;
const MAX_DURATION_MINUTES: i64 = 10080;
const MAX_QUESTION_CHARS: usize = 256;
const MAX_OPTION_CHARS: usize = 80;
const DEFAULT_MAX_VOTERS: usize = 1000;

// Stored labels stay raw; rendered mass mentions split @ from the name.
const HOSTILE_WINNER_RENDERED: &str = "@\u{200b}everyone free nitro";

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
        "id": id.to_string(), "token": "mock-poll-token", "type": 2,
        "version": 1, "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": permissions.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": user, "username": "member", "discriminator": "0"}},
        "data": {"id": "4444", "name": name, "type": 1, "options": options,
            "resolved": {"users": {
                ALICE: {"id": ALICE, "username": "alice", "discriminator": "0"},
                BOB: {"id": BOB, "username": "bob", "discriminator": "0"},
                CAROL: {"id": CAROL, "username": "carol", "discriminator": "0"},
                DAVE: {"id": DAVE, "username": "dave", "discriminator": "0"}
            }}}
    }))
    .expect("wire interaction")
}

fn button(id: u64, user: &str, custom_id: &str) -> Interaction {
    component(id, user, custom_id, false)
}

fn bot_button(id: u64, user: &str, custom_id: &str) -> Interaction {
    component(id, user, custom_id, true)
}

fn component(id: u64, user: &str, custom_id: &str, bot: bool) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-poll-token", "type": 3,
        "version": 1, "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": "0", "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": user, "username": "member", "discriminator": "0", "bot": bot}},
        "data": {"custom_id": custom_id, "component_type": 2}
    }))
    .expect("wire component interaction")
}

fn subcommand(name: &str, options: Value) -> Value {
    json!([{"name": name, "type": 1, "options": options}])
}

fn create_options(question: &str, options: &[String], duration_minutes: i64) -> Value {
    let mut args = vec![json!({"name": "question", "type": 3, "value": question})];
    for (index, option) in options.iter().enumerate() {
        args.push(json!({
            "name": format!("option{}", index + 1),
            "type": 3,
            "value": option,
        }));
    }
    args.push(json!({"name": "duration-minutes", "type": 4, "value": duration_minutes}));
    subcommand("create", Value::Array(args))
}

fn close_options(poll: &str) -> Value {
    subcommand("close", json!([{"name": "id", "type": 3, "value": poll}]))
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

/// Baseline tick seam (contract §7). At baseline there is no poll ticker, so
/// the adapter reports idle-ok and every tick test below fails on the schema
/// gate or the unchanged database. The builder's inherent
/// `InteractionRuntime::tick_polls` shadows these unqualified calls; the
/// tests only rely on `is_ok`/`Debug`, never on the outcome type's fields.
trait BaselinePollTick {
    async fn tick_polls(&self) -> Result<(), String>;
}

impl BaselinePollTick for InteractionRuntime {
    async fn tick_polls(&self) -> Result<(), String> {
        tokio::task::yield_now().await;
        Ok(())
    }
}

/// Interaction callbacks 204; every other write an ID-bearing 200 receipt.
/// Poll votes key on the interaction payload alone, so no membership reads.
async fn start_mock() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        if request.path.contains("/interactions/") {
            return ScriptedResponse::status(204);
        }
        ScriptedResponse::json(200, json!({"id": "99"}))
    })
    .await
}

/// Guarded disposable fixture: the shared testsupport guard validates the
/// bootstrap URL before any connection is opened. Each test gets its own
/// migrated database; `close` drops it.
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
            eprintln!("skipped poll acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
        Err(_) => panic!("test URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create guarded disposable test database; no credential fallback");
    Some(Fixture { db })
}

async fn table_columns(pool: &Pool<Postgres>, table: &str) -> HashSet<String> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("read information_schema");
    rows.into_iter().collect()
}

/// Fail-fast schema gate: tables plus the pinned column superset. Every DB
/// test calls this first so the baseline RED names the missing migration.
async fn require_poll_schema(pool: &Pool<Postgres>) {
    for table in ["polls", "poll_votes"] {
        let reg: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(format!("public.{table}"))
            .fetch_one(pool)
            .await
            .expect("to_regclass probe");
        assert!(reg.is_some(), "poll migration must create table {table}");
    }
    for (table, pinned) in [
        (
            "polls",
            vec![
                "guild_id",
                "id",
                "question",
                "options_json",
                "channel_id",
                "message_id",
                "created_by",
                "ends_at",
                "status",
                "max_voters",
                "winner_index",
                "closed_at",
                "results_posted_at",
                "created_at",
                "updated_at",
            ],
        ),
        (
            "poll_votes",
            vec!["guild_id", "poll_id", "user_id", "option_index", "voted_at"],
        ),
    ] {
        let have = table_columns(pool, table).await;
        for column in pinned {
            assert!(
                have.contains(column),
                "table {table} must have column {column}"
            );
        }
    }
}

async fn handle_ok(
    pool: &Pool<Postgres>,
    mock: &MockRest,
    interaction: &Interaction,
) -> Result<bool, String> {
    runtime(pool, mock)
        .handle(interaction)
        .await
        .map_err(|error| format!("{error:?}"))
}

async fn handled(pool: &Pool<Postgres>, mock: &MockRest, interaction: &Interaction, what: &str) {
    let ok = handle_ok(pool, mock, interaction)
        .await
        .expect("handler executes");
    assert!(ok, "{what}: poll interaction must be handled");
}

async fn tick(pool: &Pool<Postgres>, mock: &MockRest, what: &str) {
    // Unqualified: the baseline adapter now, the builder's inherent seam
    // later. Behavior is asserted via the database and MockRest below.
    let outcome = runtime(pool, mock).tick_polls().await;
    assert!(outcome.is_ok(), "{what}: poll tick must succeed");
}

/// Create as a privilege-less member (perms 0): creation is open (§6), so
/// the happiest path already proves no gate. Questions must be unique per
/// test: the id is read back by question.
async fn create_poll(
    pool: &Pool<Postgres>,
    mock: &MockRest,
    id: u64,
    author: &str,
    question: &str,
    options: &[String],
    duration_minutes: i64,
) -> String {
    let interaction = slash(
        id,
        author,
        "poll",
        0,
        create_options(question, options, duration_minutes),
    );
    handled(pool, mock, &interaction, "create").await;
    sqlx::query_scalar("SELECT id FROM polls WHERE guild_id = $1 AND question = $2")
        .bind(GUILD)
        .bind(question)
        .fetch_one(pool)
        .await
        .expect("created poll row")
}

async fn vote(
    pool: &Pool<Postgres>,
    mock: &MockRest,
    id: u64,
    user: &str,
    poll_id: &str,
    index: u32,
) {
    let interaction = button(id, user, &format!("poll:{poll_id}:{index}"));
    handled(pool, mock, &interaction, "vote").await;
}

async fn age_past(pool: &Pool<Postgres>, poll: &str) {
    sqlx::query(
        "UPDATE polls SET ends_at = now() - interval '1 minute' WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(poll)
    .execute(pool)
    .await
    .expect("age poll past its deadline");
}

#[derive(Debug)]
struct PollRow {
    status: String,
    question: String,
    options: Vec<String>,
    channel_id: String,
    message_id: String,
    created_by: String,
    ends_at_epoch: i64,
    max_voters: Option<i64>,
    winner: Option<i64>,
    closed: bool,
    results_posted: bool,
}

async fn poll_row(pool: &Pool<Postgres>, poll: &str) -> PollRow {
    let row = sqlx::query(
        "SELECT status::text AS status, question::text AS question, \
                options_json::text AS options_json, channel_id::text AS channel_id, \
                message_id::text AS message_id, created_by::text AS created_by, \
                EXTRACT(EPOCH FROM ends_at)::bigint AS ends_at_epoch, \
                max_voters::bigint AS max_voters, winner_index::bigint AS winner_index, \
                closed_at IS NOT NULL AS closed, \
                results_posted_at IS NOT NULL AS results_posted \
         FROM polls WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(poll)
    .fetch_one(pool)
    .await
    .expect("read poll row");
    let options_json: String = row.get("options_json");
    PollRow {
        status: row.get("status"),
        question: row.get("question"),
        options: serde_json::from_str(&options_json).expect("options_json parses"),
        channel_id: row.get("channel_id"),
        message_id: row.get("message_id"),
        created_by: row.get("created_by"),
        ends_at_epoch: row.get("ends_at_epoch"),
        max_voters: row.get("max_voters"),
        winner: row.get("winner_index"),
        closed: row.get("closed"),
        results_posted: row.get("results_posted"),
    }
}

async fn vote_rows(pool: &Pool<Postgres>, poll: &str) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT user_id::text, option_index::bigint FROM poll_votes \
         WHERE guild_id = $1 AND poll_id = $2 ORDER BY user_id",
    )
    .bind(GUILD)
    .bind(poll)
    .fetch_all(pool)
    .await
    .expect("read vote rows")
}

async fn poll_count(pool: &Pool<Postgres>) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM polls WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(pool)
        .await
        .expect("count polls")
}

async fn open_count(pool: &Pool<Postgres>) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM polls WHERE guild_id = $1 AND status = 'open'")
        .bind(GUILD)
        .fetch_one(pool)
        .await
        .expect("count open polls")
}

async fn author_open_count(pool: &Pool<Postgres>, author: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM polls WHERE guild_id = $1 AND created_by = $2 AND status = 'open'",
    )
    .bind(GUILD)
    .bind(author)
    .fetch_one(pool)
    .await
    .expect("count author open polls")
}

fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64
}

fn request_summary(requests: &[RestRequest]) -> Vec<(String, String)> {
    requests
        .iter()
        .map(|request| (request.method.clone(), request.path.clone()))
        .collect()
}

fn channel_path() -> String {
    format!("/api/v10/channels/{CHANNEL}/messages")
}

fn message_path(message_id: &str) -> String {
    format!("/api/v10/channels/{CHANNEL}/messages/{message_id}")
}

/// Channel-message POST bodies in a window. Inside close windows the only
/// channel posts are results (poll messages post at create, outside the
/// window; vote acks ride the interaction callback).
fn channel_posts(window: &[RestRequest]) -> Vec<String> {
    window
        .iter()
        .filter(|request| request.method == "POST" && request.path == channel_path())
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

fn assert_mention_safe(body: &str, what: &str) {
    assert!(
        body.contains("\"parse\":[]"),
        "{what}: poll messages must pin allowed_mentions parse:[]: {body}"
    );
    assert!(
        !body.contains("@everyone"),
        "{what}: poll messages must never carry @everyone: {body}"
    );
    assert!(
        !body.contains("@here"),
        "{what}: poll messages must never carry @here: {body}"
    );
}

/// Shared results-path assertion (contract §10): auto-close and author-close
/// prove the same observable shape through this one helper.
fn assert_results_post(body: &str, winner_text: &str, counts: &[i64], what: &str) {
    assert!(
        body.contains("embeds"),
        "{what}: results must post an embed: {body}"
    );
    assert!(
        body.contains(winner_text),
        "{what}: results must name the winning option: {body}"
    );
    for count in counts {
        assert!(
            body.contains(&count.to_string()),
            "{what}: results must carry count {count}: {body}"
        );
    }
    assert_mention_safe(body, what);
}

/// Close shape: the poll-message edit precedes the single results post.
fn assert_closed_once(
    window: &[RestRequest],
    message_id: &str,
    winner_text: &str,
    counts: &[i64],
    what: &str,
) -> String {
    let summary = request_summary(window);
    let edit = window
        .iter()
        .position(|request| request.method == "PATCH" && request.path == message_path(message_id))
        .unwrap_or_else(|| panic!("{what}: close must edit the poll message: {summary:?}"));
    let posts = channel_posts(window);
    assert_eq!(
        posts.len(),
        1,
        "{what}: close must post exactly one results message: {summary:?}"
    );
    // The window holds exactly one channel post (asserted above), so the
    // first channel POST is the results post.
    let post = window
        .iter()
        .position(|request| request.method == "POST" && request.path == channel_path())
        .expect("results post position");
    assert!(
        edit < post,
        "{what}: close edits the poll message before posting results"
    );
    assert_results_post(&posts[0], winner_text, counts, what);
    posts[0].clone()
}

fn options(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

#[test]
fn poll_migration_file_exists_and_names_tables() {
    let dir = format!("{}/../cutover/migrations", env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("migrations dir readable") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.contains("poll") && name.ends_with(".sql") {
            hits.push(name);
        }
    }
    assert!(
        !hits.is_empty(),
        "cutover migrations must include a poll migration"
    );
    let mut union = String::new();
    for name in &hits {
        union.push_str(
            &std::fs::read_to_string(format!("{dir}/{name}")).expect("migration readable"),
        );
    }
    for pinned in [
        "polls",
        "poll_votes",
        "options_json",
        "option_index",
        "winner_index",
        "results_posted_at",
        "IF NOT EXISTS",
    ] {
        assert!(
            union.contains(pinned),
            "poll migration(s) {hits:?} must mention {pinned}"
        );
    }
}

#[test]
fn poll_permission_open_to_members() {
    let perm = command_permission("poll").expect("poll permission row");
    assert_eq!(perm.required_permissions, 0);
    assert_eq!(perm.surface, CommandSurface::BuiltinSlash);
    assert!(perm.policy_hook.is_none());
    assert!(perm.allows(Some(0)));
    assert!(perm.allows(Some(PERM_MANAGE_GUILD)));
    assert!(perm.allows(None));
}

#[test]
fn poll_registry_publishes_command() {
    let defs = router().publish_set(&[]).expect("registry assembles");
    let def = defs
        .iter()
        .find(|def| def.name == "poll")
        .expect("poll published");
    assert_eq!(def.default_member_permissions, None);
    let subs: Vec<&str> = def
        .options
        .iter()
        .filter(|option| option.kind == 1)
        .map(|option| option.name.as_str())
        .collect();
    assert!(
        subs.contains(&"create"),
        "poll must publish a create subcommand"
    );
    assert!(
        subs.contains(&"close"),
        "poll must publish a close subcommand"
    );
}

#[tokio::test]
async fn baseline_tick_adapter_stays_live() {
    // Explicit trait path: keeps BaselinePollTick used after the builder's
    // inherent method shadows the unqualified tick calls, so clippy
    // `-D warnings` stays green in both states.
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock().await;
    let baseline =
        <InteractionRuntime as BaselinePollTick>::tick_polls(&runtime(fixture.pool(), &mock)).await;
    assert!(baseline.is_ok(), "baseline adapter always reports idle-ok");
    assert!(mock.requests().is_empty());
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn create_posts_poll_message_and_row() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let question = "Best sprint demo snack? P-1";
    let opts = options(&["Blue Widget W-1", "Red Gadget G-1", "Green Gizmo Z-1"]);
    let before = mock.requests().len();
    let pid = create_poll(fixture.pool(), &mock, 500, ALICE, question, &opts, 60).await;
    let row = poll_row(fixture.pool(), &pid).await;
    assert_eq!(row.status, "open");
    assert_eq!(row.question, question);
    assert_eq!(row.options, opts);
    assert_eq!(row.channel_id, CHANNEL);
    assert!(!row.message_id.is_empty());
    assert_eq!(row.created_by, ALICE);
    assert!(
        (row.ends_at_epoch - (epoch_now() + 3600)).abs() <= 120,
        "60-minute create must land near now + 3600 s, got {exact}",
        exact = row.ends_at_epoch
    );
    assert!(row.max_voters.is_none());
    assert!(row.winner.is_none());
    assert!(!row.closed);
    assert!(!row.results_posted);
    // The poll message carries the question, every option and every vote
    // button, and is mention-safe.
    let requests = mock.requests();
    let created = &requests[before..];
    let post = created
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(question)
        })
        .unwrap_or_else(|| {
            panic!(
                "create must post the poll message: {:?}",
                request_summary(created)
            )
        });
    let body = String::from_utf8_lossy(&post.body).into_owned();
    assert!(
        body.contains("components"),
        "poll message must carry buttons"
    );
    for (index, option) in opts.iter().enumerate() {
        assert!(
            body.contains(option),
            "poll message must carry option {option}"
        );
        let custom_id = format!("poll:{pid}:{index}");
        assert!(
            custom_id.len() <= 100,
            "vote custom_id must fit Discord's 100-char cap"
        );
        assert!(
            body.contains(&custom_id),
            "poll message must carry button {custom_id}"
        );
    }
    assert_mention_safe(&body, "create");
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_create_refused_with_zero_rows() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    // Each invalid shape is acknowledged (handle() true) and stores nothing.
    let cases: Vec<(&str, Value)> = vec![
        (
            "one option",
            create_options("Lonely? I-1", &options(&["Solo"]), 60),
        ),
        (
            "zero duration",
            create_options("Zero? I-2", &options(&["A", "B"]), 0),
        ),
        (
            "negative duration",
            create_options("Negative? I-3", &options(&["A", "B"]), -5),
        ),
        (
            "overlong duration",
            create_options(
                "Forever? I-4",
                &options(&["A", "B"]),
                MAX_DURATION_MINUTES + 1,
            ),
        ),
        (
            "empty question",
            create_options("", &options(&["A", "B"]), 60),
        ),
        (
            "blank question",
            create_options("   ", &options(&["A", "B"]), 60),
        ),
        (
            "overlong question",
            create_options(
                &"q".repeat(MAX_QUESTION_CHARS + 1),
                &options(&["A", "B"]),
                60,
            ),
        ),
        (
            "empty option",
            create_options("Empty? I-8", &options(&["A", ""]), 60),
        ),
        (
            "overlong option",
            create_options(
                "Long? I-9",
                &options(&["A", &"o".repeat(MAX_OPTION_CHARS + 1)]),
                60,
            ),
        ),
    ];
    for (index, (what, opts)) in cases.into_iter().enumerate() {
        let interaction = slash(510 + index as u64, ALICE, "poll", 0, opts);
        handled(pool_ref, &mock, &interaction, what).await;
    }
    assert_eq!(poll_count(pool_ref).await, 0);
    // Boundary durations are inclusive: 1 minute and 7 days both create.
    create_poll(
        pool_ref,
        &mock,
        530,
        ALICE,
        "One minute? I-10",
        &options(&["A", "B"]),
        1,
    )
    .await;
    create_poll(
        pool_ref,
        &mock,
        531,
        ALICE,
        "Seven days? I-11",
        &options(&["A", "B"]),
        MAX_DURATION_MINUTES,
    )
    .await;
    assert_eq!(poll_count(pool_ref).await, 2);
    // Boundary text lengths are inclusive: 256-char question, 80-char option.
    create_poll(
        pool_ref,
        &mock,
        532,
        ALICE,
        &"q".repeat(MAX_QUESTION_CHARS),
        &options(&["A", &"o".repeat(MAX_OPTION_CHARS)]),
        60,
    )
    .await;
    assert_eq!(poll_count(pool_ref).await, 3);
    // Ten options create; the surface defines option1..option10 (contract §4).
    let ten: Vec<String> = (1..=MAX_OPTIONS).map(|n| format!("Opt-{n}")).collect();
    assert_eq!(ten.len(), MAX_OPTIONS);
    let ten_id = create_poll(pool_ref, &mock, 533, BOB, "Ten? I-12", &ten, 60).await;
    assert_eq!(poll_row(pool_ref, &ten_id).await.options.len(), MAX_OPTIONS);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn vote_change_vote_single_row_per_member() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let opts = options(&["Alpha V-1", "Beta V-1", "Gamma V-1"]);
    let pid = create_poll(pool_ref, &mock, 540, ALICE, "Change? V-1", &opts, 60).await;
    let row = poll_row(pool_ref, &pid).await;
    // First vote: one row, and the poll message is live-edited with counts.
    let before = mock.requests().len();
    vote(pool_ref, &mock, 541, ALICE, &pid, 0).await;
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 0)],
        "first vote stores one row"
    );
    let requests = mock.requests();
    let edits: Vec<&RestRequest> = requests[before..]
        .iter()
        .filter(|request| {
            request.method == "PATCH" && request.path == message_path(&row.message_id)
        })
        .collect();
    assert_eq!(
        edits.len(),
        1,
        "each accepted vote live-edits the poll message"
    );
    let edit_body = String::from_utf8_lossy(&edits[0].body).into_owned();
    assert!(
        edit_body.contains("Alpha V-1") && edit_body.contains('1'),
        "live edit must carry the voted option and its count: {edit_body}"
    );
    // Re-voting the same option is idempotent: still one row.
    vote(pool_ref, &mock, 542, ALICE, &pid, 0).await;
    assert_eq!(vote_rows(pool_ref, &pid).await.len(), 1);
    // Changing the vote moves the single row, never adds one.
    vote(pool_ref, &mock, 543, ALICE, &pid, 1).await;
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 1)],
        "change-vote moves the member's single row"
    );
    vote(pool_ref, &mock, 544, BOB, &pid, 0).await;
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 1), (BOB.to_owned(), 0)],
        "two members hold one row each"
    );
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_votes_refused() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        550,
        ALICE,
        "Refuse? R-1",
        &options(&["Yes R-1", "No R-1"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 551, ALICE, &pid, 0).await;
    let baseline = vote_rows(pool_ref, &pid).await;
    // Well-formed but unknown poll id: acknowledged, no row.
    let forged = button(552, BOB, "poll:nope-unknown-id-000:0");
    handled(pool_ref, &mock, &forged, "unknown poll vote").await;
    // Out-of-range and non-numeric indexes: acknowledged, no row.
    for (id, custom_id, what) in [
        (553, format!("poll:{pid}:7"), "over-range index"),
        (554, format!("poll:{pid}:abc"), "non-numeric index"),
    ] {
        let bad = button(id, BOB, &custom_id);
        handled(pool_ref, &mock, &bad, what).await;
    }
    assert_eq!(vote_rows(pool_ref, &pid).await, baseline);
    // Malformed custom ids: never panic, never write (ack unpinned).
    for (id, custom_id) in [(555, format!("poll:{pid}")), (556, "poll:".to_owned())] {
        let malformed = button(id, BOB, &custom_id);
        handle_ok(pool_ref, &mock, &malformed)
            .await
            .expect("malformed vote must not error");
    }
    assert_eq!(vote_rows(pool_ref, &pid).await, baseline);
    // Votes after close are refused with no change.
    let close = slash(557, ALICE, "poll", 0, close_options(&pid));
    handled(pool_ref, &mock, &close, "close").await;
    assert_eq!(poll_row(pool_ref, &pid).await.status, "closed");
    let late = button(558, BOB, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &late, "vote after close").await;
    assert_eq!(vote_rows(pool_ref, &pid).await, baseline);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn bot_votes_never_counted() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        560,
        ALICE,
        "Humans? B-1",
        &options(&["Human B-1", "Also human B-1"]),
        60,
    )
    .await;
    // Absent bot flag means human: the plain vote counts.
    vote(pool_ref, &mock, 561, ALICE, &pid, 0).await;
    // bot:true voters are refused at write: acknowledged, zero rows.
    let bot_vote = bot_button(562, BOT_VOTER, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &bot_vote, "bot vote").await;
    let bot_change = bot_button(563, BOT_VOTER, &format!("poll:{pid}:0"));
    handled(pool_ref, &mock, &bot_change, "bot change-vote").await;
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 0)],
        "bot votes must never be counted"
    );
    // Results computed from the rows exclude bots by construction.
    age_past(pool_ref, &pid).await;
    tick(pool_ref, &mock, "bot-exclusion tick").await;
    assert_eq!(poll_row(pool_ref, &pid).await.winner, Some(0));
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn hostile_option_text_inert() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let question = "Pick one, @everyone? H-1";
    let opts = options(&[
        "@everyone free nitro",
        "<@1660102030405060721> you won",
        "<@&1660102030405060731> role ping",
        "@here newline\n`code` **bold** ||spoiler||",
        "\"quoted\" \\ backslash H-1",
        "poll:fake-id:0 injection",
    ]);
    let before = mock.requests().len();
    let pid = create_poll(pool_ref, &mock, 570, ALICE, question, &opts, 60).await;
    assert_eq!(poll_row(pool_ref, &pid).await.options, opts);
    // Hostile text is accepted but inert: the poll message stays mention-safe.
    let requests = mock.requests();
    let post = requests[before..]
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains("H-1")
        })
        .expect("hostile create posts the poll message");
    assert_mention_safe(&String::from_utf8_lossy(&post.body), "hostile create");
    // Voting still keys on custom ids, not labels: the injection label does
    // not cross-talk, and counts stay exact.
    vote(pool_ref, &mock, 571, ALICE, &pid, 5).await;
    vote(pool_ref, &mock, 572, BOB, &pid, 0).await;
    vote(pool_ref, &mock, 573, CAROL, &pid, 0).await;
    assert_eq!(vote_rows(pool_ref, &pid).await.len(), 3);
    age_past(pool_ref, &pid).await;
    let before = mock.requests().len();
    tick(pool_ref, &mock, "hostile tick").await;
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.winner, Some(0));
    let requests = mock.requests();
    let body = assert_closed_once(
        &requests[before..],
        &row.message_id,
        HOSTILE_WINNER_RENDERED,
        &[2, 0, 0, 0, 0, 1],
        "hostile tick",
    );
    serde_json::from_slice::<Value>(body.as_bytes()).expect("results stay valid JSON");
    assert!(
        !body.contains("<@1660102030405060721>"),
        "hostile mention must not survive into results: {body}"
    );
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn auto_close_posts_results_once() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let opts = options(&["Sunrise A-1", "Sunset A-1", "Midnight A-1"]);
    let pid = create_poll(pool_ref, &mock, 580, ALICE, "Best hour? A-1", &opts, 60).await;
    vote(pool_ref, &mock, 581, ALICE, &pid, 0).await;
    vote(pool_ref, &mock, 582, BOB, &pid, 0).await;
    vote(pool_ref, &mock, 583, CAROL, &pid, 2).await;
    // A tick before expiry changes nothing and posts nothing.
    let quiet = mock.requests().len();
    tick(pool_ref, &mock, "early tick").await;
    assert_eq!(poll_row(pool_ref, &pid).await.status, "open");
    assert!(channel_posts(&mock.requests()[quiet..]).is_empty());
    // At duration the tick closes once: one edit, one results post.
    age_past(pool_ref, &pid).await;
    let before = mock.requests().len();
    tick(pool_ref, &mock, "due tick").await;
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.status, "closed");
    assert_eq!(row.winner, Some(0));
    assert!(row.closed);
    assert!(row.results_posted);
    let requests = mock.requests();
    assert_closed_once(
        &requests[before..],
        &row.message_id,
        "Sunrise A-1",
        &[2, 0, 1],
        "due tick",
    );
    // A settled poll stays quiet: no new rows, no new Discord calls.
    let quiet = mock.requests().len();
    tick(pool_ref, &mock, "settled tick").await;
    assert_eq!(mock.requests().len(), quiet);
    assert_eq!(poll_row(pool_ref, &pid).await.winner, Some(0));
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn restart_recovery_posts_results_once() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        590,
        ALICE,
        "Crash? K-1",
        &options(&["Keep K-1", "Drop K-1"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 591, ALICE, &pid, 1).await;
    vote(pool_ref, &mock, 592, BOB, &pid, 1).await;
    age_past(pool_ref, &pid).await;
    tick(pool_ref, &mock, "close tick").await;
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.winner, Some(1));
    assert!(row.results_posted);
    // Crash after the flip, before the results flag: closed with
    // results_posted_at NULL. A fresh runtime (the restart) posts exactly
    // one more results message with identical content, then goes quiet.
    sqlx::query("UPDATE polls SET results_posted_at = NULL WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&pid)
        .execute(pool_ref)
        .await
        .expect("simulate crash mid-announce");
    let before = mock.requests().len();
    tick(pool_ref, &mock, "recovery tick").await;
    let recovered = poll_row(pool_ref, &pid).await;
    assert_eq!(recovered.status, "closed");
    assert_eq!(recovered.winner, Some(1));
    assert!(recovered.results_posted);
    let requests = mock.requests();
    // Recovery re-posts results; the poll-message edit is idempotent and may
    // repeat, so only the single results post is pinned here.
    let posts = channel_posts(&requests[before..]);
    assert_eq!(
        posts.len(),
        1,
        "recovery must post exactly one results message"
    );
    assert_results_post(&posts[0], "Drop K-1", &[0, 2], "recovery tick");
    let quiet = mock.requests().len();
    tick(pool_ref, &mock, "post-recovery tick").await;
    assert_eq!(mock.requests().len(), quiet);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn concurrent_ticks_yield_one_results_post() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        600,
        ALICE,
        "Race? C-1",
        &options(&["Left C-1", "Right C-1"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 601, ALICE, &pid, 0).await;
    vote(pool_ref, &mock, 602, BOB, &pid, 1).await;
    age_past(pool_ref, &pid).await;
    let runtimes = [
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
    ];
    let before = mock.requests().len();
    let (a, b, c, d) = tokio::join!(
        runtimes[0].tick_polls(),
        runtimes[1].tick_polls(),
        runtimes[2].tick_polls(),
        runtimes[3].tick_polls()
    );
    for outcome in [&a, &b, &c, &d] {
        assert!(outcome.is_ok(), "raced ticks must all succeed");
    }
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.status, "closed");
    assert_eq!(row.winner, Some(0), "1-1 tie breaks to the lowest index");
    assert!(row.results_posted);
    // The flip losers skip the post: exactly one results message under race.
    // Windowed: the create message (which also names the options) sits
    // outside the race window.
    let requests = mock.requests();
    let posts = channel_posts(&requests[before..]);
    assert_eq!(posts.len(), 1, "raced ticks must yield one results post");
    assert_results_post(&posts[0], "Left C-1", &[1, 1], "raced ticks");
    // Raced change-votes collapse to one row: last writer wins, never two.
    let pid2 = create_poll(
        pool_ref,
        &mock,
        610,
        ALICE,
        "Race? C-2",
        &options(&["Up C-2", "Down C-2"]),
        60,
    )
    .await;
    let first = button(611, CAROL, &format!("poll:{pid2}:0"));
    let second = button(612, CAROL, &format!("poll:{pid2}:1"));
    let (one, two) = tokio::join!(
        handle_ok(pool_ref, &mock, &first),
        handle_ok(pool_ref, &mock, &second)
    );
    assert!(one.expect("first raced vote executes"));
    assert!(two.expect("second raced vote executes"));
    assert_eq!(
        vote_rows(pool_ref, &pid2).await.len(),
        1,
        "raced votes from one member must collapse to one row"
    );
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn author_close_shares_results_path() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let opts = options(&["North E-1", "South E-1"]);
    let pid = create_poll(pool_ref, &mock, 620, ALICE, "Early? E-1", &opts, 60).await;
    vote(pool_ref, &mock, 621, ALICE, &pid, 1).await;
    vote(pool_ref, &mock, 622, BOB, &pid, 1).await;
    vote(pool_ref, &mock, 623, CAROL, &pid, 0).await;
    // A non-author member's close is refused with no state change.
    let refused = slash(624, BOB, "poll", 0, close_options(&pid));
    let before = mock.requests().len();
    handled(pool_ref, &mock, &refused, "non-author close").await;
    assert_eq!(poll_row(pool_ref, &pid).await.status, "open");
    assert!(channel_posts(&mock.requests()[before..]).is_empty());
    // The author's early close shares the auto-close results path: same edit,
    // same single results shape, same winner computation.
    let close = slash(625, ALICE, "poll", 0, close_options(&pid));
    let before = mock.requests().len();
    handled(pool_ref, &mock, &close, "author close").await;
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.status, "closed");
    assert_eq!(row.winner, Some(1));
    assert!(row.results_posted);
    let requests = mock.requests();
    assert_closed_once(
        &requests[before..],
        &row.message_id,
        "South E-1",
        &[1, 2],
        "author close",
    );
    // Closing twice still posts exactly one results message.
    let again = slash(626, ALICE, "poll", 0, close_options(&pid));
    handled(pool_ref, &mock, &again, "double close").await;
    assert_eq!(channel_posts(&mock.requests()[before..]).len(), 1);
    // Closing an unknown poll is refused with no state change.
    let missing = slash(627, ALICE, "poll", 0, close_options("poll-missing-id"));
    handled(pool_ref, &mock, &missing, "unknown close").await;
    assert_eq!(channel_posts(&mock.requests()[before..]).len(), 1);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn open_quotas_refuse_over_cap() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    // Per-author quota: 3 open, the 4th refused with no row.
    for index in 0..MAX_OPEN_PER_AUTHOR {
        let id = 630 + index as u64;
        create_poll(
            pool_ref,
            &mock,
            id,
            ALICE,
            &format!("Author quota Q-{index}"),
            &options(&["A", "B"]),
            60,
        )
        .await;
    }
    let refused = slash(
        633,
        ALICE,
        "poll",
        0,
        create_options("Author quota Q-3", &options(&["A", "B"]), 60),
    );
    handled(pool_ref, &mock, &refused, "over-author-quota create").await;
    assert_eq!(
        author_open_count(pool_ref, ALICE).await,
        MAX_OPEN_PER_AUTHOR as i64
    );
    // Quotas count open polls only: closing one frees the author's slot.
    let theirs: String =
        sqlx::query_scalar("SELECT id FROM polls WHERE guild_id = $1 AND created_by = $2 AND status = 'open' ORDER BY created_at LIMIT 1")
            .bind(GUILD)
            .bind(ALICE)
            .fetch_one(pool_ref)
            .await
            .expect("author open poll");
    let close = slash(634, ALICE, "poll", 0, close_options(&theirs));
    handled(pool_ref, &mock, &close, "quota close").await;
    create_poll(
        pool_ref,
        &mock,
        635,
        ALICE,
        "Author quota Q-4",
        &options(&["A", "B"]),
        60,
    )
    .await;
    assert_eq!(
        author_open_count(pool_ref, ALICE).await,
        MAX_OPEN_PER_AUTHOR as i64
    );
    // Per-guild quota: 10 open across authors, the 11th refused.
    // Alice already holds 3 open; Bob/Carol take 3 each, Dave 1.
    let mut id = 640;
    for (author, count) in [(BOB, 3), (CAROL, 3), (DAVE, 1)] {
        for n in 0..count {
            create_poll(
                pool_ref,
                &mock,
                id,
                author,
                &format!("Guild quota {author}-{n}"),
                &options(&["A", "B"]),
                60,
            )
            .await;
            id += 1;
        }
    }
    assert_eq!(open_count(pool_ref).await, MAX_OPEN_PER_GUILD as i64);
    let over = slash(
        650,
        DAVE,
        "poll",
        0,
        create_options("Guild quota over", &options(&["A", "B"]), 60),
    );
    handled(pool_ref, &mock, &over, "over-guild-quota create").await;
    assert_eq!(open_count(pool_ref).await, MAX_OPEN_PER_GUILD as i64);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn voter_cap_refuses_over_max_voters() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        660,
        ALICE,
        "Cap? M-1",
        &options(&["In M-1", "Out M-1"]),
        60,
    )
    .await;
    sqlx::query("UPDATE polls SET max_voters = 1 WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&pid)
        .execute(pool_ref)
        .await
        .expect("set voter cap");
    assert_eq!(poll_row(pool_ref, &pid).await.max_voters, Some(1));
    vote(pool_ref, &mock, 661, ALICE, &pid, 0).await;
    let capped = button(662, BOB, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &capped, "over-cap vote").await;
    assert_eq!(vote_rows(pool_ref, &pid).await.len(), 1);
    // A change-vote by the seated voter is not a new voter: still accepted.
    vote(pool_ref, &mock, 663, ALICE, &pid, 1).await;
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 1)],
        "capped change-vote moves the seated row"
    );
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn tie_and_empty_results() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    // Ties break to the lowest option index.
    let tied = create_poll(
        pool_ref,
        &mock,
        670,
        ALICE,
        "Tie? T-1",
        &options(&["First T-1", "Second T-1"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 671, ALICE, &tied, 1).await;
    vote(pool_ref, &mock, 672, BOB, &tied, 0).await;
    age_past(pool_ref, &tied).await;
    let before = mock.requests().len();
    tick(pool_ref, &mock, "tie tick").await;
    let row = poll_row(pool_ref, &tied).await;
    assert_eq!(row.winner, Some(0), "ties break to the lowest index");
    let requests = mock.requests();
    assert_closed_once(
        &requests[before..],
        &row.message_id,
        "First T-1",
        &[1, 1],
        "tie tick",
    );
    // Zero votes still post: zero counts, NULL winner, no winner mention.
    let empty = create_poll(
        pool_ref,
        &mock,
        673,
        ALICE,
        "Empty? T-2",
        &options(&["Lonely T-2", "Lonesome T-2"]),
        60,
    )
    .await;
    age_past(pool_ref, &empty).await;
    let before = mock.requests().len();
    tick(pool_ref, &mock, "empty tick").await;
    let row = poll_row(pool_ref, &empty).await;
    assert_eq!(row.status, "closed");
    assert_eq!(row.winner, None, "zero votes set no winner");
    assert!(row.results_posted);
    let requests = mock.requests();
    let posts = channel_posts(&requests[before..]);
    assert_eq!(posts.len(), 1, "empty close must still post results");
    assert!(
        posts[0].contains("embeds"),
        "empty results must post an embed: {}",
        posts[0]
    );
    assert!(
        !posts[0].contains("<@"),
        "empty results must mention no winner: {}",
        posts[0]
    );
    assert_mention_safe(&posts[0], "empty tick");
    fixture.close().await;
    mock.shutdown().await;
}

#[test]
fn hostile_results_oracle_accepts_safe_text_and_rejects_raw_mentions() {
    // Oracle control, not implementation evidence: the corrected winner and
    // every existing results assertion must be simultaneously satisfiable.
    let safe = json!({
        "allowed_mentions": {"parse": []},
        "embeds": [{"description": "@\u{200b}everyone free nitro: 2; others: 0, 0, 0, 0, 1"}]
    })
    .to_string();
    assert_results_post(
        &safe,
        HOSTILE_WINNER_RENDERED,
        &[2, 0, 0, 0, 0, 1],
        "safe oracle control",
    );
    let raw = json!({
        "allowed_mentions": {"parse": []},
        "embeds": [{"description": "@everyone free nitro: 2; others: 0, 0, 0, 0, 1"}]
    })
    .to_string();
    assert!(
        std::panic::catch_unwind(|| assert_results_post(
            &raw,
            "@everyone free nitro",
            &[2, 0, 0, 0, 0, 1],
            "raw oracle control",
        ))
        .is_err(),
        "allowed_mentions alone must not permit raw mass mentions"
    );
}

#[tokio::test]
async fn null_voter_cap_defaults_to_1000_and_allows_changes_at_cap() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        680,
        ALICE,
        "Default cap? M-2",
        &options(&["In M-2", "Changed M-2"]),
        60,
    )
    .await;
    assert_eq!(poll_row(pool_ref, &pid).await.max_voters, None);
    // Seed only the precondition, avoiding 999 irrelevant REST edits. The
    // inclusive boundary, refusal and change all go through the real handler.
    let inserted = sqlx::query(
        "INSERT INTO poll_votes (guild_id, poll_id, user_id, option_index, voted_at) \
         SELECT $1, $2, (1660102030405070000::bigint + n)::text, 0, now() \
         FROM generate_series(1, $3::bigint) AS voter(n)",
    )
    .bind(GUILD)
    .bind(&pid)
    .bind((DEFAULT_MAX_VOTERS - 1) as i64)
    .execute(pool_ref)
    .await
    .expect("seed 999 distinct voters in the disposable fixture");
    assert_eq!(inserted.rows_affected(), (DEFAULT_MAX_VOTERS - 1) as u64);
    assert_eq!(
        vote_rows(pool_ref, &pid).await.len(),
        DEFAULT_MAX_VOTERS - 1
    );
    vote(pool_ref, &mock, 681, ALICE, &pid, 0).await;
    let at_cap = vote_rows(pool_ref, &pid).await;
    assert_eq!(at_cap.len(), DEFAULT_MAX_VOTERS);
    assert!(at_cap.contains(&(ALICE.to_owned(), 0)));
    let over = button(682, BOB, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &over, "1001st distinct voter").await;
    assert_eq!(vote_rows(pool_ref, &pid).await, at_cap);
    // The seated voter may change without increasing the distinct count.
    vote(pool_ref, &mock, 683, ALICE, &pid, 1).await;
    let mut changed = at_cap;
    let alice = changed
        .iter_mut()
        .find(|(user, _)| user == ALICE)
        .expect("Alice occupies the thousandth seat");
    alice.1 = 1;
    assert_eq!(vote_rows(pool_ref, &pid).await, changed);
    assert_eq!(poll_row(pool_ref, &pid).await.max_voters, None);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn expired_open_poll_refuses_new_votes_and_changes_before_tick() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    let mock = start_mock().await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        690,
        ALICE,
        "Expired before tick? R-2",
        &options(&["Before R-2", "Too late R-2"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 691, ALICE, &pid, 0).await;
    let before = vote_rows(pool_ref, &pid).await;
    assert_eq!(before, vec![(ALICE.to_owned(), 0)]);
    age_past(pool_ref, &pid).await;
    assert_eq!(poll_row(pool_ref, &pid).await.status, "open");
    let late_new = button(692, BOB, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &late_new, "expired new voter before tick").await;
    assert_eq!(vote_rows(pool_ref, &pid).await, before);
    let late_change = button(693, ALICE, &format!("poll:{pid}:1"));
    handled(pool_ref, &mock, &late_change, "expired change before tick").await;
    assert_eq!(vote_rows(pool_ref, &pid).await, before);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn concurrent_closed_unposted_recovery_posts_once_without_reflip() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_poll_schema(fixture.pool()).await;
    // Keep results delivery in flight briefly so independent runtimes can
    // contend for the same unposted row before the first sender settles it.
    let mock = MockRest::with_responder(|request: &RestRequest| {
        if request.path.contains("/interactions/") {
            return ScriptedResponse::status(204);
        }
        let receipt = ScriptedResponse::json(200, json!({"id": "99"}));
        if request.method == "POST" && request.path == channel_path() {
            receipt.delayed(std::time::Duration::from_millis(100))
        } else {
            receipt
        }
    })
    .await;
    let pool_ref = fixture.pool();
    let pid = create_poll(
        pool_ref,
        &mock,
        700,
        ALICE,
        "Recover together? K-2",
        &options(&["Leave K-2", "Recover K-2"]),
        60,
    )
    .await;
    vote(pool_ref, &mock, 701, ALICE, &pid, 1).await;
    vote(pool_ref, &mock, 702, BOB, &pid, 1).await;
    // Crash-before-post precondition: closed state is already durable, but
    // no result was delivered. This is not the post-then-flag crash window.
    sqlx::query(
        "UPDATE polls SET status = 'closed', winner_index = 1, \
         ends_at = now() - interval '1 minute', \
         closed_at = now() - interval '30 seconds', results_posted_at = NULL \
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(&pid)
    .execute(pool_ref)
    .await
    .expect("seed closed/unposted recovery in the disposable fixture");
    let closed_at: String =
        sqlx::query_scalar("SELECT closed_at::text FROM polls WHERE guild_id = $1 AND id = $2")
            .bind(GUILD)
            .bind(&pid)
            .fetch_one(pool_ref)
            .await
            .expect("read durable close time");
    let runtimes = [
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
        runtime(pool_ref, &mock),
    ];
    let before = mock.requests().len();
    let (a, b, c, d) = tokio::join!(
        runtimes[0].tick_polls(),
        runtimes[1].tick_polls(),
        runtimes[2].tick_polls(),
        runtimes[3].tick_polls()
    );
    for outcome in [&a, &b, &c, &d] {
        assert!(outcome.is_ok(), "concurrent recovery ticks must succeed");
    }
    let row = poll_row(pool_ref, &pid).await;
    assert_eq!(row.status, "closed");
    assert_eq!(row.winner, Some(1));
    assert!(row.closed);
    assert!(row.results_posted);
    let after_closed_at: String =
        sqlx::query_scalar("SELECT closed_at::text FROM polls WHERE guild_id = $1 AND id = $2")
            .bind(GUILD)
            .bind(&pid)
            .fetch_one(pool_ref)
            .await
            .expect("read recovered close time");
    assert_eq!(
        after_closed_at, closed_at,
        "recovery must not re-flip the poll"
    );
    let requests = mock.requests();
    let posts = channel_posts(&requests[before..]);
    assert_eq!(
        posts.len(),
        1,
        "concurrent recoverers must post results once"
    );
    assert_results_post(&posts[0], "Recover K-2", &[0, 2], "concurrent recovery");
    assert_eq!(
        vote_rows(pool_ref, &pid).await,
        vec![(ALICE.to_owned(), 1), (BOB.to_owned(), 1)]
    );
    let quiet = mock.requests().len();
    tick(pool_ref, &mock, "settled concurrent recovery").await;
    assert_eq!(mock.requests().len(), quiet);
    fixture.close().await;
    mock.shutdown().await;
}
