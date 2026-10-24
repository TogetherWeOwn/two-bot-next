//! Acceptance: giveaway timed draw, announce, reroll and recovery are safe.
//!
//! Protected test file: the paired build card implements against these tests
//! and must never edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! 1. Migration: the next free global number in `crates/cutover/migrations/`
//!    (`0452` at writing; the glob below tolerates a renumber, but the file
//!    name must contain `giveaway`). Additive and idempotent (`IF NOT
//!    EXISTS`): creates `giveaways`, `giveaway_entries` and `giveaway_draws`
//!    per the design spec. `giveaway_entries` carries
//!    `PRIMARY KEY (guild_id, giveaway_id, user_id)`; `giveaway_draws`
//!    carries `PRIMARY KEY (guild_id, giveaway_id, draw_no)`.
//! 2. Schema (superset-tolerant: extra columns are fine, missing pinned ones
//!    fail):
//!    - `giveaways`: `guild_id`, `id`, `prize`, `channel_id`, `message_id`,
//!      `created_by`, `winners_count`, `ends_at`, `status`, `requirements_json`,
//!      `max_entries`, `draw_nonce`, `claim_token`, `claimed_at`, `paused_at`,
//!      `announced_at`, `created_at`, `updated_at`. `paused_at` (nullable
//!      timestamptz, set by pause, cleared by resume) is this contract's
//!      addition so resume can extend `ends_at` by the paused duration.
//!      `claim_token`, `claimed_at`, `requirements_json`, `announced_at` and
//!      (on draws) `reroll_kind` are nullable.
//!    - `giveaway_entries`: `guild_id`, `giveaway_id`, `user_id`, `weight`
//!      (always 1 in MVP), `entered_at`.
//!    - `giveaway_draws`: `guild_id`, `giveaway_id`, `draw_no` (first draw
//!      0), `drawn_at`, `eligible_count`, `eligible_hash`, `seed`,
//!      `winner_ids`, `reroll_kind`.
//! 3. Canonical replay encodings (members recompute these, so the bytes are
//!    pinned): `eligible_hash` is lowercase hex sha256 over the comma-joined
//!    lexicographically-sorted decimal user-id list; `seed` is lowercase hex
//!    sha256 of `"{draw_nonce}:{draw_no}"`; `draw_nonce` is 32 lowercase hex
//!    chars; `winner_ids` is a JSON array of user-id strings in draw order;
//!    `reroll_kind` is NULL, `full`, or `single:<user_id>`.
//! 4. Slash surface: top-level command `giveaway` with type-1 subcommand
//!    options `create`, `list`, `cancel`, `end`, `pause`, `resume`,
//!    `extend`, `reroll`. Argument names: `prize` (string),
//!    `duration-minutes` (integer minutes, required on create, added on
//!    extend), `winners` (integer), `channel` (optional channel),
//!    `id` (giveaway id string), `user` (optional reroll target).
//! 5. Entry button `custom_id` is `gw:<giveaway-id>`, keyed by the custom id
//!    alone; any guild member may enter (no permission gate on the
//!    component). Status must be `open`, capacity and eligibility are
//!    re-checked server-side.
//! 6. `COMMAND_PERMISSIONS` row: `giveaway`, `BuiltinSlash`,
//!    `PERM_MANAGE_GUILD`, no policy hook. The registry publishes top-level
//!    `giveaway` with `default_member_permissions == "32"`.
//! 7. Gates: giveaway rides the existing `announcements` gate. No new
//!    `RouterGates`/`SurfaceFlags` field: this file constructs gates
//!    literally and cannot adapt to one.
//! 8. Tick seam: `pub async fn tick_giveaways(&self) -> Result<O, E>` as an
//!    inherent method on `InteractionRuntime`, with `O: Debug` and
//!    `E: Debug`. The exact zero-argument shape matters: these tests call it
//!    unqualified through the baseline adapter below, and the inherent
//!    method shadows the adapter only when the signature matches. The tick
//!    scans `status = open AND ends_at <= now` plus expired leases, draws
//!    under the claim (re-validate eligibility in-transaction, pure draw,
//!    insert the draw row, flip to `drawn`, release the claim by clearing
//!    the claim columns), then announces (edit the giveaway message, post
//!    the winners message, set `announced_at`). A `drawn` row with
//!    `announced_at IS NULL` re-announces from the stored row and never
//!    re-draws; a claimed row with an existing draw row skips to announce;
//!    an idle tick makes no Discord calls. Supervisor wiring (shared
//!    supervisor, 15-30 s cadence, Skip on missed ticks) is the builder's,
//!    proven by staging evidence.
//! 9. Hardening numbers: at most 10 open giveaways per guild (the 11th
//!    create is refused with no row); per-giveaway entry cap enforced from
//!    the `max_entries` column; per-user write budgets refuse spam
//!    pre-write (exact budget counts are builder-unit-tested, these tests
//!    only pin burst collapse to one row). Audit rows live in
//!    `automation_audit_log` with actions `giveaway.create`, `giveaway.draw`,
//!    `giveaway.reroll`, `giveaway.cancel`, `target_key` set to the giveaway
//!    id, ids and outcomes only (never prize text), and a unique id per
//!    write (a re-announce must not primary-key-conflict).
//! 10. Announce order and content: the message edit precedes the channel
//!     winners post; the winners post contains each winner as `<@id>` plus
//!     the draw `seed` and the giveaway `draw_nonce`. Create posts the entry
//!     message (prize text plus a `gw:<id>` button) to the invocation
//!     channel.
//!
//! Deliberately unpinned: reply copy and ack shapes (deferred or immediate);
//! default `winners_count`/`max_entries`; `drawing`-status internals and the
//! lease horizon (stale means `claimed_at` in 2020); winner DMs (deferred
//! unless a DM send path is proven during build); countdown and live-count
//! display; list output format; requirement option names on create
//! (requirements are set via SQL here); extend-while-paused; pause/cancel
//! audit actions beyond §9; the zero-entrant announcement body beyond the
//! stored draw row and `announced_at`.
//!
//! Test shapes: offline contract tests (migration glob, permission row,
//! registry) plus MockRest + disposable-database acceptance driven through
//! `InteractionRuntime::handle` (dispatch by command name, entries by
//! `custom_id`) and the tick seam above. Raw SQL strings only, so every
//! test compiles on the baseline and fails on assertions. DB tests skip
//! vacuously without `TWO_TEST_DATABASE_URL`; CI's integration step sets it.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Pool, Postgres, Row};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    command_permissions::{command_permission, CommandSurface},
    commands::PERM_MANAGE_GUILD,
    ClassifierConfig, InteractionRouter, RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060701";
const CHANNEL: &str = "1550102030405060702";
const MOD: &str = "1550102030405060711";
const ALICE: &str = "1550102030405060721";
const BOB: &str = "1550102030405060722";
const CAROL: &str = "1550102030405060723";
const ROLE: &str = "1550102030405060731";

/// Pinned open-giveaway quota per guild (contract §9).
const MAX_OPEN_PER_GUILD: usize = 10;

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

/// Baseline tick seam (contract §8). At baseline there is no giveaway ticker,
/// so the adapter reports idle-ok and every tick test below fails on the
/// schema gate or the unchanged database. The builder's inherent
/// `InteractionRuntime::tick_giveaways` shadows these unqualified calls; the
/// tests only rely on `is_ok`/`Debug`, never on the outcome type's fields.
trait BaselineGiveawayTick {
    async fn tick_giveaways(&self) -> Result<(), String>;
}

impl BaselineGiveawayTick for InteractionRuntime {
    async fn tick_giveaways(&self) -> Result<(), String> {
        tokio::task::yield_now().await;
        Ok(())
    }
}

#[derive(Clone)]
enum MemberAnswer {
    Current(Vec<String>),
    Absent,
}

/// Path-routed mock: interaction callbacks 204, membership reads per the
/// table (unlisted members read back as current with no roles), every other
/// write an ID-bearing 200 receipt.
async fn start_mock(members: HashMap<String, MemberAnswer>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        if request.path.contains("/interactions/") {
            return ScriptedResponse::status(204);
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
                Some(MemberAnswer::Absent) => return ScriptedResponse::status(404),
                None => {
                    return ScriptedResponse::json(200, json!({"user": {"id": user}, "roles": []}));
                }
            }
        }
        ScriptedResponse::json(200, json!({"id": "99"}))
    })
    .await
}

fn all_current() -> HashMap<String, MemberAnswer> {
    HashMap::new()
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
            eprintln!("skipped giveaway acceptance: TWO_TEST_DATABASE_URL not set");
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
async fn require_giveaway_schema(pool: &Pool<Postgres>) {
    for table in ["giveaways", "giveaway_entries", "giveaway_draws"] {
        let reg: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(format!("public.{table}"))
            .fetch_one(pool)
            .await
            .expect("to_regclass probe");
        assert!(
            reg.is_some(),
            "giveaway migration must create table {table}"
        );
    }
    for (table, pinned) in [
        (
            "giveaways",
            vec![
                "guild_id",
                "id",
                "prize",
                "channel_id",
                "message_id",
                "created_by",
                "winners_count",
                "ends_at",
                "status",
                "requirements_json",
                "max_entries",
                "draw_nonce",
                "claim_token",
                "claimed_at",
                "paused_at",
                "announced_at",
                "created_at",
                "updated_at",
            ],
        ),
        (
            "giveaway_entries",
            vec!["guild_id", "giveaway_id", "user_id", "weight", "entered_at"],
        ),
        (
            "giveaway_draws",
            vec![
                "guild_id",
                "giveaway_id",
                "draw_no",
                "drawn_at",
                "eligible_count",
                "eligible_hash",
                "seed",
                "winner_ids",
                "reroll_kind",
            ],
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

async fn handled(pool: &Pool<Postgres>, mock: &MockRest, interaction: &Interaction, what: &str) {
    let ok = runtime(pool, mock)
        .handle(interaction)
        .await
        .expect("handler executes");
    assert!(ok, "{what}: giveaway interaction must be handled");
}

async fn tick(pool: &Pool<Postgres>, mock: &MockRest, what: &str) {
    // Unqualified: the baseline adapter now, the builder's inherent seam
    // later. Behavior is asserted via the database and MockRest below.
    let outcome = runtime(pool, mock).tick_giveaways().await;
    assert!(outcome.is_ok(), "{what}: giveaway tick must succeed");
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

#[derive(Debug)]
struct GiveawayRow {
    status: String,
    prize: String,
    channel_id: String,
    message_id: String,
    created_by: String,
    winners_count: i64,
    ends_at_epoch: i64,
    requirements: Option<String>,
    max_entries: Option<i64>,
    draw_nonce: String,
    claim_open: bool,
    announced: bool,
}

async fn giveaway_row(pool: &Pool<Postgres>, giveaway: &str) -> GiveawayRow {
    let row = sqlx::query(
        "SELECT status::text AS status, prize::text AS prize, \
                channel_id::text AS channel_id, message_id::text AS message_id, \
                created_by::text AS created_by, winners_count::bigint AS winners_count, \
                EXTRACT(EPOCH FROM ends_at)::bigint AS ends_at_epoch, \
                requirements_json::text AS requirements_json, \
                max_entries::bigint AS max_entries, draw_nonce::text AS draw_nonce, \
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
        status: row.get("status"),
        prize: row.get("prize"),
        channel_id: row.get("channel_id"),
        message_id: row.get("message_id"),
        created_by: row.get("created_by"),
        winners_count: row.get("winners_count"),
        ends_at_epoch: row.get("ends_at_epoch"),
        requirements: row.get("requirements_json"),
        max_entries: row.get("max_entries"),
        draw_nonce: row.get("draw_nonce"),
        claim_open: row.get("claim_open"),
        announced: row.get("announced"),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DrawRow {
    draw_no: i64,
    eligible_count: i64,
    eligible_hash: String,
    seed: String,
    winners: Vec<String>,
    reroll_kind: Option<String>,
}

async fn draw_rows(pool: &Pool<Postgres>, giveaway: &str) -> Vec<DrawRow> {
    let rows: Vec<(i64, i64, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT draw_no::bigint, eligible_count::bigint, eligible_hash::text, \
                seed::text, winner_ids::text, reroll_kind::text \
         FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2 ORDER BY draw_no",
    )
    .bind(GUILD)
    .bind(giveaway)
    .fetch_all(pool)
    .await
    .expect("read draw rows");
    rows.into_iter()
        .map(
            |(draw_no, eligible_count, eligible_hash, seed, winners, reroll_kind)| DrawRow {
                draw_no,
                eligible_count,
                eligible_hash,
                seed,
                winners: serde_json::from_str(&winners).expect("winner ids parse"),
                reroll_kind,
            },
        )
        .collect()
}

async fn entry_count(pool: &Pool<Postgres>, giveaway: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_entries WHERE guild_id = $1 AND giveaway_id = $2",
    )
    .bind(GUILD)
    .bind(giveaway)
    .fetch_one(pool)
    .await
    .expect("count entries")
}

async fn giveaway_audits(pool: &Pool<Postgres>) -> Vec<(String, Option<String>, String)> {
    sqlx::query_as(
        "SELECT action::text, target_key::text, outcome::text \
         FROM automation_audit_log WHERE guild_id = $1 AND action LIKE 'giveaway.%' \
         ORDER BY created_at, id",
    )
    .bind(GUILD)
    .fetch_all(pool)
    .await
    .expect("read giveaway audits")
}

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
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

/// Winners post for `seed` inside a request window, with the edit-before-post
/// order pinned (contract §10).
fn assert_announced(
    window: &[RestRequest],
    message_id: &str,
    seed: &str,
    nonce: &str,
    winners: &[String],
    what: &str,
) {
    let summary = request_summary(window);
    let edit = window
        .iter()
        .position(|request| request.method == "PATCH" && request.path == message_path(message_id))
        .unwrap_or_else(|| panic!("{what}: tick must edit the giveaway message: {summary:?}"));
    let post = window
        .iter()
        .position(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(seed)
        })
        .unwrap_or_else(|| panic!("{what}: tick must post the winners message: {summary:?}"));
    assert!(
        edit < post,
        "{what}: announce edits the giveaway message before posting winners"
    );
    let body = String::from_utf8_lossy(&window[post].body).into_owned();
    for winner in winners {
        assert!(
            body.contains(&format!("<@{winner}>")),
            "{what}: winners post must mention {winner}: {body}"
        );
    }
    assert!(
        body.contains(nonce),
        "{what}: winners post must reveal the draw nonce for replay: {body}"
    );
}

#[test]
fn giveaway_migration_file_exists_and_names_tables() {
    let dir = format!("{}/../cutover/migrations", env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("migrations dir readable") {
        let name = entry
            .expect("dir entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.contains("giveaway") && name.ends_with(".sql") {
            hits.push(name);
        }
    }
    assert!(
        !hits.is_empty(),
        "cutover migrations must include a giveaway migration"
    );
    for name in &hits {
        let content = std::fs::read_to_string(format!("{dir}/{name}")).expect("migration readable");
        for pinned in [
            "giveaways",
            "giveaway_entries",
            "giveaway_draws",
            "draw_no",
            "announced_at",
            "IF NOT EXISTS",
        ] {
            assert!(
                content.contains(pinned),
                "migration {name} must mention {pinned}"
            );
        }
    }
}

#[test]
fn giveaway_permission_requires_manage_guild() {
    let perm = command_permission("giveaway").expect("giveaway permission row");
    assert_eq!(perm.required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(perm.surface, CommandSurface::BuiltinSlash);
    assert!(perm.allows(Some(PERM_MANAGE_GUILD)));
    assert!(!perm.allows(Some(0)));
    assert!(!perm.allows(None));
}

#[test]
fn giveaway_registry_publishes_command() {
    let defs = router().publish_set(&[]).expect("registry assembles");
    let def = defs
        .iter()
        .find(|def| def.name == "giveaway")
        .expect("giveaway published");
    assert_eq!(
        def.default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string())
    );
}

#[tokio::test]
async fn baseline_tick_adapter_stays_live() {
    // Explicit trait path: keeps BaselineGiveawayTick used after the
    // builder's inherent method shadows the unqualified tick calls, so
    // clippy `-D warnings` stays green in both states.
    let Some(fixture) = pool().await else {
        return;
    };
    let mock = start_mock(all_current()).await;
    let baseline = <InteractionRuntime as BaselineGiveawayTick>::tick_giveaways(&runtime(
        fixture.pool(),
        &mock,
    ))
    .await;
    assert!(baseline.is_ok(), "baseline adapter always reports idle-ok");
    assert!(mock.requests().is_empty());
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn create_posts_entry_message_and_audit_row() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let prize = "Golden Test Trophy QRX-9";
    let before = mock.requests().len();
    let gid = create_giveaway(fixture.pool(), &mock, 500, prize, 60, 1).await;
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(row.status, "open");
    assert_eq!(row.prize, prize);
    assert_eq!(row.channel_id, CHANNEL);
    assert!(!row.message_id.is_empty());
    assert_eq!(row.created_by, MOD);
    assert_eq!(row.winners_count, 1);
    assert!(
        (row.ends_at_epoch - (epoch_now() + 3600)).abs() <= 120,
        "60-minute create must land near now + 3600 s, got {exact}",
        exact = row.ends_at_epoch
    );
    assert_eq!(row.draw_nonce.len(), 32, "128-bit hex nonce");
    assert!(
        row.draw_nonce.chars().all(|c| c.is_ascii_hexdigit())
            && row.draw_nonce.to_lowercase() == row.draw_nonce,
        "nonce must be lowercase hex, got {nonce}",
        nonce = row.draw_nonce
    );
    assert!(!row.claim_open);
    assert!(!row.announced);
    // The entry message carries the prize and the entry button.
    let requests = mock.requests();
    let created = &requests[before..];
    let post = created
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(prize)
        })
        .unwrap_or_else(|| {
            panic!(
                "create must post the entry message: {:?}",
                request_summary(created)
            )
        });
    assert!(
        String::from_utf8_lossy(&post.body).contains(&format!("gw:{gid}")),
        "entry message must carry the gw:<id> button"
    );
    // Audit: one create row, ids only.
    let audits = giveaway_audits(fixture.pool()).await;
    let creates: Vec<_> = audits
        .iter()
        .filter(|(action, _, _)| action == "giveaway.create")
        .collect();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].1.as_deref(), Some(gid.as_str()));
    assert!(
        !format!("{audits:?}").contains("QRX-9"),
        "audit rows must not echo prize text"
    );
    // Without Manage Guild the create is refused with no row.
    let denied = slash(
        501,
        ALICE,
        "giveaway",
        0,
        create_options("Nope Prize", 60, 1),
    );
    handled(fixture.pool(), &mock, &denied, "unprivileged create").await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM giveaways WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(fixture.pool())
        .await
        .expect("count giveaways");
    assert_eq!(count, 1);
    // List answers.
    let list = slash(
        502,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        json!([{"name": "list", "type": 1}]),
    );
    handled(fixture.pool(), &mock, &list, "list").await;
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn entry_collapses_repeats_and_counts_entrants() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 510, "Entry Prize E-1", 60, 1).await;
    enter(fixture.pool(), &mock, 511, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 512, ALICE, &gid).await;
    for burst in 0..25 {
        let id = 520 + burst as u64;
        enter(fixture.pool(), &mock, id, ALICE, &gid).await;
    }
    enter(fixture.pool(), &mock, 550, BOB, &gid).await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 2);
    let weights: Vec<i64> =
        sqlx::query_scalar("SELECT weight::bigint FROM giveaway_entries WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_all(fixture.pool())
            .await
            .expect("read weights");
    assert_eq!(weights.len(), 2);
    assert!(weights.iter().all(|weight| *weight == 1));
    // A forged entry for an unknown giveaway is refused with no row.
    let forged = button(551, ALICE, "gw:nope-unknown-id");
    handled(fixture.pool(), &mock, &forged, "unknown entry").await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 2);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn entry_cap_refuses_over_max_entries() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 560, "Cap Prize C-1", 60, 1).await;
    sqlx::query("UPDATE giveaways SET max_entries = 1 WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&gid)
        .execute(fixture.pool())
        .await
        .expect("set entry cap");
    assert_eq!(
        giveaway_row(fixture.pool(), &gid).await.max_entries,
        Some(1)
    );
    enter(fixture.pool(), &mock, 561, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 562, BOB, &gid).await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 1);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn open_quota_refuses_eleventh_giveaway() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    for index in 0..MAX_OPEN_PER_GUILD {
        let id = 570 + index as u64;
        create_giveaway(
            fixture.pool(),
            &mock,
            id,
            &format!("Quota Prize Q-{index}"),
            60,
            1,
        )
        .await;
    }
    let refused = slash(
        590,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        create_options("Quota Prize Q-10", 60, 1),
    );
    handled(fixture.pool(), &mock, &refused, "over-quota create").await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaways WHERE guild_id = $1 AND status = 'open'",
    )
    .bind(GUILD)
    .fetch_one(fixture.pool())
    .await
    .expect("count open giveaways");
    assert_eq!(count, MAX_OPEN_PER_GUILD as i64);
    fixture.close().await;
    mock.shutdown().await;
}

async fn age_past(pool: &Pool<Postgres>, giveaway: &str) {
    sqlx::query("UPDATE giveaways SET ends_at = now() - interval '1 minute' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(giveaway)
        .execute(pool)
        .await
        .expect("age giveaway past its deadline");
}

#[tokio::test]
async fn deadline_tick_draws_and_announces() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 600, "Tick Prize TDA-1", 60, 1).await;
    enter(fixture.pool(), &mock, 601, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 602, BOB, &gid).await;
    age_past(fixture.pool(), &gid).await;
    let before = mock.requests().len();
    tick(fixture.pool(), &mock, "due tick").await;
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(row.status, "drawn");
    assert!(row.announced);
    assert!(!row.claim_open);
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    let draw = &draws[0];
    assert_eq!(draw.draw_no, 0);
    assert_eq!(draw.eligible_count, 2);
    assert_eq!(draw.winners.len(), 1);
    assert!(draw.winners[0] == ALICE || draw.winners[0] == BOB);
    assert!(draw.reroll_kind.is_none());
    let mut eligible = [ALICE.to_owned(), BOB.to_owned()];
    eligible.sort();
    assert_eq!(draw.eligible_hash, sha256_hex(&eligible.join(",")));
    assert_eq!(
        draw.seed,
        sha256_hex(&format!("{nonce}:0", nonce = row.draw_nonce))
    );
    let requests = mock.requests();
    assert_announced(
        &requests[before..],
        &row.message_id,
        &draw.seed,
        &row.draw_nonce,
        &draw.winners,
        "due tick",
    );
    let audits = giveaway_audits(fixture.pool()).await;
    let draws_audit: Vec<_> = audits
        .iter()
        .filter(|(action, _, _)| action == "giveaway.draw")
        .collect();
    assert_eq!(draws_audit.len(), 1);
    assert_eq!(draws_audit[0].1.as_deref(), Some(gid.as_str()));
    // A second giveaway proves multi-winner shape: distinct winners drawn
    // from the entrants.
    let gid2 = create_giveaway(fixture.pool(), &mock, 610, "Tick Prize TDA-2", 60, 2).await;
    enter(fixture.pool(), &mock, 611, ALICE, &gid2).await;
    enter(fixture.pool(), &mock, 612, BOB, &gid2).await;
    enter(fixture.pool(), &mock, 613, CAROL, &gid2).await;
    age_past(fixture.pool(), &gid2).await;
    tick(fixture.pool(), &mock, "multi-winner tick").await;
    let draws2 = draw_rows(fixture.pool(), &gid2).await;
    assert_eq!(draws2.len(), 1);
    assert_eq!(draws2[0].eligible_count, 3);
    assert_eq!(draws2[0].winners.len(), 2);
    assert_ne!(draws2[0].winners[0], draws2[0].winners[1]);
    for winner in &draws2[0].winners {
        assert!([ALICE, BOB, CAROL].contains(&winner.as_str()));
    }
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn end_draws_now_before_deadline() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 620, "End Prize END-1", 60, 1).await;
    enter(fixture.pool(), &mock, 621, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 622, BOB, &gid).await;
    let end = slash(
        623,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid),
    );
    let before = mock.requests().len();
    handled(fixture.pool(), &mock, &end, "end").await;
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(row.status, "drawn");
    assert!(row.announced);
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    assert_eq!(draws[0].draw_no, 0);
    let requests = mock.requests();
    assert_announced(
        &requests[before..],
        &row.message_id,
        &draws[0].seed,
        &row.draw_nonce,
        &draws[0].winners,
        "end",
    );
    // Ending an unknown giveaway, or ending twice, draws nothing new.
    let missing = slash(
        624,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", "gw-missing-id"),
    );
    handled(fixture.pool(), &mock, &missing, "end unknown").await;
    handled(fixture.pool(), &mock, &end, "end twice").await;
    assert_eq!(draw_rows(fixture.pool(), &gid).await.len(), 1);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn paused_giveaway_skips_due_scan_and_resume_extends_deadline() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 630, "Pause Prize P-1", 60, 1).await;
    enter(fixture.pool(), &mock, 631, ALICE, &gid).await;
    let pause = slash(
        632,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("pause", &gid),
    );
    handled(fixture.pool(), &mock, &pause, "pause").await;
    assert_eq!(giveaway_row(fixture.pool(), &gid).await.status, "paused");
    // Paused entries are refused and the due scan skips the row.
    enter(fixture.pool(), &mock, 633, BOB, &gid).await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 1);
    age_past(fixture.pool(), &gid).await;
    let quiet = mock.requests().len();
    tick(fixture.pool(), &mock, "paused tick").await;
    assert_eq!(mock.requests().len(), quiet);
    assert!(draw_rows(fixture.pool(), &gid).await.is_empty());
    assert_eq!(giveaway_row(fixture.pool(), &gid).await.status, "paused");
    // Resume extends the deadline by the paused duration: backdate the pause
    // start by one hour and expect ~3600 s added, never shortened.
    let paused_ends = giveaway_row(fixture.pool(), &gid).await.ends_at_epoch;
    sqlx::query("UPDATE giveaways SET paused_at = now() - interval '1 hour' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&gid)
        .execute(fixture.pool())
        .await
        .expect("backdate pause start");
    let resume = slash(
        634,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("resume", &gid),
    );
    handled(fixture.pool(), &mock, &resume, "resume").await;
    let resumed = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(resumed.status, "open");
    let gained = resumed.ends_at_epoch - paused_ends;
    assert!(
        (3570..=3630).contains(&gained),
        "resume must extend ends_at by ~3600 s, got {gained}"
    );
    // Extend adds its minutes on top.
    let extend = slash(
        635,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        subcommand(
            "extend",
            json!([
                {"name": "id", "type": 3, "value": &gid},
                {"name": "duration-minutes", "type": 4, "value": 30},
            ]),
        ),
    );
    handled(fixture.pool(), &mock, &extend, "extend").await;
    let extended = giveaway_row(fixture.pool(), &gid).await.ends_at_epoch;
    let added = extended - resumed.ends_at_epoch;
    assert!(
        (1680..=1920).contains(&added),
        "extend must add ~1800 s, got {added}"
    );
    // The resumed giveaway draws normally once due again.
    enter(fixture.pool(), &mock, 636, BOB, &gid).await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 2);
    age_past(fixture.pool(), &gid).await;
    tick(fixture.pool(), &mock, "resumed tick").await;
    assert_eq!(giveaway_row(fixture.pool(), &gid).await.status, "drawn");
    assert_eq!(draw_rows(fixture.pool(), &gid).await.len(), 1);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn cancelled_giveaway_never_draws() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 640, "Cancel Prize X-1", 60, 1).await;
    enter(fixture.pool(), &mock, 641, ALICE, &gid).await;
    let cancel = slash(
        642,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("cancel", &gid),
    );
    handled(fixture.pool(), &mock, &cancel, "cancel").await;
    assert_eq!(giveaway_row(fixture.pool(), &gid).await.status, "cancelled");
    age_past(fixture.pool(), &gid).await;
    let quiet = mock.requests().len();
    tick(fixture.pool(), &mock, "cancelled tick").await;
    assert_eq!(mock.requests().len(), quiet);
    assert!(draw_rows(fixture.pool(), &gid).await.is_empty());
    enter(fixture.pool(), &mock, 643, BOB, &gid).await;
    assert_eq!(entry_count(fixture.pool(), &gid).await, 1);
    let audits = giveaway_audits(fixture.pool()).await;
    let cancels: Vec<_> = audits
        .iter()
        .filter(|(action, _, _)| action == "giveaway.cancel")
        .collect();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].1.as_deref(), Some(gid.as_str()));
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn reroll_appends_new_draw_and_keeps_history() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 650, "Reroll Prize R-1", 60, 1).await;
    enter(fixture.pool(), &mock, 651, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 652, BOB, &gid).await;
    let end = slash(
        653,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid),
    );
    handled(fixture.pool(), &mock, &end, "end").await;
    let first = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(first.len(), 1);
    let before = mock.requests().len();
    let reroll = slash(
        654,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("reroll", &gid),
    );
    handled(fixture.pool(), &mock, &reroll, "reroll full").await;
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 2);
    assert_eq!(draws[0], first[0], "reroll must leave the old row intact");
    assert_eq!(draws[1].draw_no, 1);
    assert_eq!(draws[1].reroll_kind.as_deref(), Some("full"));
    assert_eq!(draws[1].eligible_count, 2);
    assert_eq!(draws[1].winners.len(), 1);
    assert!(draws[1].winners[0] == ALICE || draws[1].winners[0] == BOB);
    assert_eq!(draws[1].eligible_hash, draws[0].eligible_hash);
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(
        draws[1].seed,
        sha256_hex(&format!("{nonce}:1", nonce = row.draw_nonce))
    );
    assert_ne!(draws[1].seed, draws[0].seed);
    let requests = mock.requests();
    let reposted = requests[before..]
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == channel_path()
                && String::from_utf8_lossy(&request.body).contains(&draws[1].seed)
        })
        .unwrap_or_else(|| {
            panic!(
                "reroll must post the new winners: {:?}",
                request_summary(&requests[before..])
            )
        });
    let rechamp = &draws[1].winners[0];
    assert!(
        String::from_utf8_lossy(&reposted.body).contains(&format!("<@{rechamp}>")),
        "reroll post must mention the new winner"
    );
    let audits = giveaway_audits(fixture.pool()).await;
    let rerolls: Vec<_> = audits
        .iter()
        .filter(|(action, _, _)| action == "giveaway.reroll")
        .collect();
    assert_eq!(rerolls.len(), 1);
    assert_eq!(rerolls[0].1.as_deref(), Some(gid.as_str()));
    // Single-winner reroll is deterministic with two entrants: the other
    // member replaces the named winner.
    let gid2 = create_giveaway(fixture.pool(), &mock, 660, "Reroll Prize R-2", 60, 1).await;
    enter(fixture.pool(), &mock, 661, ALICE, &gid2).await;
    enter(fixture.pool(), &mock, 662, BOB, &gid2).await;
    let end2 = slash(
        663,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid2),
    );
    handled(fixture.pool(), &mock, &end2, "end").await;
    let winner = draw_rows(fixture.pool(), &gid2).await;
    assert_eq!(winner.len(), 1);
    let replaced = winner[0].winners[0].clone();
    let single = slash(
        664,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        subcommand(
            "reroll",
            json!([
                {"name": "id", "type": 3, "value": &gid2},
                {"name": "user", "type": 6, "value": &replaced},
            ]),
        ),
    );
    handled(fixture.pool(), &mock, &single, "reroll single").await;
    let redrawn = draw_rows(fixture.pool(), &gid2).await;
    assert_eq!(redrawn.len(), 2);
    assert_eq!(redrawn[1].draw_no, 1);
    assert_eq!(
        redrawn[1].reroll_kind.as_deref(),
        Some(format!("single:{replaced}").as_str())
    );
    assert_eq!(redrawn[1].winners.len(), 1);
    assert_ne!(redrawn[1].winners[0], replaced);
    assert!(redrawn[1].winners[0] == ALICE || redrawn[1].winners[0] == BOB);
    // Reroll before any draw, and rerolling a never-entered member, refuse
    // with no new row.
    let gid3 = create_giveaway(fixture.pool(), &mock, 670, "Reroll Prize R-3", 60, 1).await;
    enter(fixture.pool(), &mock, 671, ALICE, &gid3).await;
    let early = slash(
        672,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("reroll", &gid3),
    );
    handled(fixture.pool(), &mock, &early, "reroll early").await;
    assert!(draw_rows(fixture.pool(), &gid3).await.is_empty());
    let stranger = slash(
        673,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        subcommand(
            "reroll",
            json!([
                {"name": "id", "type": 3, "value": &gid},
                {"name": "user", "type": 6, "value": CAROL},
            ]),
        ),
    );
    handled(fixture.pool(), &mock, &stranger, "reroll stranger").await;
    assert_eq!(draw_rows(fixture.pool(), &gid).await.len(), 2);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn restart_recovery_never_double_draws() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    // Crash holding a claim with the draw already stored: the next tick
    // re-claims and skips to announce, drawing nothing new.
    let gid = create_giveaway(fixture.pool(), &mock, 680, "Crash Prize K-1", 60, 1).await;
    enter(fixture.pool(), &mock, 681, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 682, BOB, &gid).await;
    let end = slash(
        683,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid),
    );
    handled(fixture.pool(), &mock, &end, "end").await;
    let stored = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(stored.len(), 1);
    sqlx::query(
        "UPDATE giveaways SET status = 'open', announced_at = NULL, \
                claim_token = 'stale-claim-token', claimed_at = '2020-01-01T00:00:00+00:00', \
                ends_at = now() - interval '1 minute' \
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(&gid)
    .execute(fixture.pool())
    .await
    .expect("simulate crash holding a stale claim");
    let before = mock.requests().len();
    tick(fixture.pool(), &mock, "stale-claim tick").await;
    assert_eq!(
        draw_rows(fixture.pool(), &gid).await,
        stored,
        "re-claim with a stored draw must not re-draw"
    );
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(row.status, "drawn");
    assert!(row.announced);
    let requests = mock.requests();
    assert_announced(
        &requests[before..],
        &row.message_id,
        &stored[0].seed,
        &row.draw_nonce,
        &stored[0].winners,
        "stale-claim tick",
    );
    // Crash holding a claim with no draw stored: the next tick draws exactly
    // once and announces.
    let gid2 = create_giveaway(fixture.pool(), &mock, 690, "Crash Prize K-2", 60, 1).await;
    enter(fixture.pool(), &mock, 691, ALICE, &gid2).await;
    sqlx::query(
        "UPDATE giveaways SET claim_token = 'stale-claim-token', \
                claimed_at = '2020-01-01T00:00:00+00:00', \
                ends_at = now() - interval '1 minute' \
         WHERE guild_id = $1 AND id = $2",
    )
    .bind(GUILD)
    .bind(&gid2)
    .execute(fixture.pool())
    .await
    .expect("simulate crash before the draw insert");
    tick(fixture.pool(), &mock, "draw-after-crash tick").await;
    assert_eq!(draw_rows(fixture.pool(), &gid2).await.len(), 1);
    assert!(giveaway_row(fixture.pool(), &gid2).await.announced);
    // Crash mid-announce: drawn with announced_at NULL re-announces the same
    // stored winners, never re-draws.
    let gid3 = create_giveaway(fixture.pool(), &mock, 700, "Crash Prize K-3", 60, 1).await;
    enter(fixture.pool(), &mock, 701, ALICE, &gid3).await;
    let end3 = slash(
        702,
        MOD,
        "giveaway",
        PERM_MANAGE_GUILD,
        id_options("end", &gid3),
    );
    handled(fixture.pool(), &mock, &end3, "end").await;
    let stored3 = draw_rows(fixture.pool(), &gid3).await;
    assert_eq!(stored3.len(), 1);
    sqlx::query("UPDATE giveaways SET announced_at = NULL WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&gid3)
        .execute(fixture.pool())
        .await
        .expect("simulate crash mid-announce");
    let before = mock.requests().len();
    tick(fixture.pool(), &mock, "re-announce tick").await;
    assert_eq!(draw_rows(fixture.pool(), &gid3).await, stored3);
    let row3 = giveaway_row(fixture.pool(), &gid3).await;
    assert!(row3.announced);
    let requests = mock.requests();
    assert_announced(
        &requests[before..],
        &row3.message_id,
        &stored3[0].seed,
        &row3.draw_nonce,
        &stored3[0].winners,
        "re-announce tick",
    );
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn concurrent_ticks_yield_one_draw_row() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 710, "Race Prize RC-1", 60, 1).await;
    enter(fixture.pool(), &mock, 711, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 712, BOB, &gid).await;
    age_past(fixture.pool(), &gid).await;
    let first = runtime(fixture.pool(), &mock);
    let second = runtime(fixture.pool(), &mock);
    let third = runtime(fixture.pool(), &mock);
    let fourth = runtime(fixture.pool(), &mock);
    let (a, b, c, d) = tokio::join!(
        first.tick_giveaways(),
        second.tick_giveaways(),
        third.tick_giveaways(),
        fourth.tick_giveaways()
    );
    for outcome in [&a, &b, &c, &d] {
        assert!(outcome.is_ok(), "raced ticks must all succeed");
    }
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1, "raced ticks must yield one draw row");
    assert!(giveaway_row(fixture.pool(), &gid).await.announced);
    // Every winners post delivered under the race carries the same seed and
    // winners: at-most-once draw, at-least-once announce.
    let posts: Vec<String> = mock
        .requests()
        .iter()
        .filter(|request| request.method == "POST" && request.path == channel_path())
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .filter(|body| body.contains(&draws[0].seed))
        .collect();
    assert!(!posts.is_empty());
    let champ = &draws[0].winners[0];
    for body in &posts {
        assert!(body.contains(&format!("<@{champ}>")));
    }
    // The draw primary key holds under directly raced inserts too.
    async fn insert(
        pool: &Pool<Postgres>,
        giveaway: &str,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO giveaway_draws (guild_id, giveaway_id, draw_no, drawn_at, \
                    eligible_count, eligible_hash, seed, winner_ids, reroll_kind) \
             VALUES ($1, $2, 7, now(), 1, 'aa', 'bb', '[]', NULL)",
        )
        .bind(GUILD)
        .bind(giveaway)
        .execute(pool)
        .await
    }
    let (one, two) = tokio::join!(insert(fixture.pool(), &gid), insert(fixture.pool(), &gid));
    assert!(
        one.is_ok() ^ two.is_ok(),
        "exactly one raced draw insert must win"
    );
    let dupes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2 AND draw_no = 7",
    )
    .bind(GUILD)
    .bind(&gid)
    .fetch_one(fixture.pool())
    .await
    .expect("count raced rows");
    assert_eq!(dupes, 1);
    // A settled giveaway stays quiet: no new rows, no new Discord calls.
    sqlx::query(
        "DELETE FROM giveaway_draws WHERE guild_id = $1 AND giveaway_id = $2 AND draw_no = 7",
    )
    .bind(GUILD)
    .bind(&gid)
    .execute(fixture.pool())
    .await
    .expect("remove race probe row");
    let quiet = mock.requests().len();
    tick(fixture.pool(), &mock, "settled tick").await;
    assert_eq!(mock.requests().len(), quiet);
    assert_eq!(draw_rows(fixture.pool(), &gid).await.len(), 1);
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draw_excludes_departed_and_role_stripped_entrants() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mut members = HashMap::new();
    members.insert(
        ALICE.to_owned(),
        MemberAnswer::Current(vec![ROLE.to_owned()]),
    );
    members.insert(BOB.to_owned(), MemberAnswer::Absent);
    members.insert(CAROL.to_owned(), MemberAnswer::Current(vec![]));
    let mock = start_mock(members).await;
    let gid = create_giveaway(fixture.pool(), &mock, 720, "Eligible Prize EL-1", 60, 1).await;
    enter(fixture.pool(), &mock, 721, ALICE, &gid).await;
    enter(fixture.pool(), &mock, 722, BOB, &gid).await;
    enter(fixture.pool(), &mock, 723, CAROL, &gid).await;
    sqlx::query("UPDATE giveaways SET requirements_json = $1 WHERE guild_id = $2 AND id = $3")
        .bind(
            json!({"roles": [ROLE], "min_account_age_days": 0, "min_guild_age_days": 0})
                .to_string(),
        )
        .bind(GUILD)
        .bind(&gid)
        .execute(fixture.pool())
        .await
        .expect("set role requirement");
    assert!(giveaway_row(fixture.pool(), &gid)
        .await
        .requirements
        .expect("requirements stored")
        .contains(ROLE));
    age_past(fixture.pool(), &gid).await;
    tick(fixture.pool(), &mock, "eligibility tick").await;
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    assert_eq!(draws[0].eligible_count, 1);
    assert_eq!(draws[0].winners, vec![ALICE.to_owned()]);
    assert_eq!(draws[0].eligible_hash, sha256_hex(ALICE));
    // Without requirements the departed entrant is still excluded while both
    // present members stay eligible.
    let gid2 = create_giveaway(fixture.pool(), &mock, 730, "Eligible Prize EL-2", 60, 1).await;
    enter(fixture.pool(), &mock, 731, ALICE, &gid2).await;
    enter(fixture.pool(), &mock, 732, BOB, &gid2).await;
    enter(fixture.pool(), &mock, 733, CAROL, &gid2).await;
    sqlx::query("UPDATE giveaways SET requirements_json = '{}' WHERE guild_id = $1 AND id = $2")
        .bind(GUILD)
        .bind(&gid2)
        .execute(fixture.pool())
        .await
        .expect("clear requirements");
    age_past(fixture.pool(), &gid2).await;
    tick(fixture.pool(), &mock, "departure tick").await;
    let draws2 = draw_rows(fixture.pool(), &gid2).await;
    assert_eq!(draws2.len(), 1);
    assert_eq!(draws2[0].eligible_count, 2);
    assert_eq!(draws2[0].winners.len(), 1);
    assert!(draws2[0].winners[0] == ALICE || draws2[0].winners[0] == CAROL);
    let mut eligible = [ALICE.to_owned(), CAROL.to_owned()];
    eligible.sort();
    assert_eq!(draws2[0].eligible_hash, sha256_hex(&eligible.join(",")));
    fixture.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draw_handles_empty_and_short_fields() {
    let Some(fixture) = pool().await else {
        return;
    };
    require_giveaway_schema(fixture.pool()).await;
    let mock = start_mock(all_current()).await;
    let gid = create_giveaway(fixture.pool(), &mock, 740, "Empty Prize EM-1", 60, 2).await;
    age_past(fixture.pool(), &gid).await;
    tick(fixture.pool(), &mock, "empty tick").await;
    let row = giveaway_row(fixture.pool(), &gid).await;
    assert_eq!(row.status, "drawn");
    assert!(row.announced);
    let draws = draw_rows(fixture.pool(), &gid).await;
    assert_eq!(draws.len(), 1);
    assert_eq!(draws[0].eligible_count, 0);
    assert!(draws[0].winners.is_empty());
    // Fewer entrants than winners draws every entrant.
    let gid2 = create_giveaway(fixture.pool(), &mock, 750, "Short Prize SH-1", 60, 3).await;
    enter(fixture.pool(), &mock, 751, ALICE, &gid2).await;
    age_past(fixture.pool(), &gid2).await;
    let before = mock.requests().len();
    tick(fixture.pool(), &mock, "short tick").await;
    let draws2 = draw_rows(fixture.pool(), &gid2).await;
    assert_eq!(draws2.len(), 1);
    assert_eq!(draws2[0].eligible_count, 1);
    assert_eq!(draws2[0].winners, vec![ALICE.to_owned()]);
    let row2 = giveaway_row(fixture.pool(), &gid2).await;
    let requests = mock.requests();
    assert_announced(
        &requests[before..],
        &row2.message_id,
        &draws2[0].seed,
        &row2.draw_nonce,
        &draws2[0].winners,
        "short tick",
    );
    fixture.close().await;
    mock.shutdown().await;
}
