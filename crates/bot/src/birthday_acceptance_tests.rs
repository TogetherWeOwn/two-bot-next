#![cfg(test)]
//! Protected acceptance: birthdays (`/birthday set` + daily announcement).
//!
//! Failing-first contract for the birthday build (roadmap item 7). The Builder
//! builds on this branch and must never edit this file; change the product,
//! never the expectations. Failing tests fail because the feature is missing,
//! never because a fixture is broken: the controls at the bottom hold on the
//! baseline and distinguish fixture bugs from RED.
//!
//! Contract under test, one observable per bullet:
//!
//! * (a) `/birthday set <month> <day>` stores month/day; set/show/remove
//!   round-trip; a second set replaces; confirmations are immediate,
//!   ephemeral and concise.
//! * (b) Invalid dates (month 0/13, day 0/32, Feb 30, Apr 31, …) are refused
//!   with zero stored state; Feb 29 is accepted (leap birthdays exist).
//! * (c) Year input is refused and never stored: the published command has no
//!   year option, and no birthday table carries a year-like column.
//! * (d) The daily sweep posts exactly one cheer per celebrant in the
//!   configured channel and grants the birthday role for the day, removing it
//!   (and any stale grant) once the day passes.
//! * (e) Restarts never double-post nor strand the role: a same-day re-sweep
//!   over a fresh pool posts nothing new, and the next-day sweep still cycles
//!   the role.
//! * (f) Privacy: birthdays are readable only via the member's own query and
//!   the day's cheer; no birthday PII (month/day values) appears in logs;
//!   per-guild celebrants are capped.
//! * (g) Human UX: concise confirmations, cheer text configurable within
//!   mention-safe bounds; hostile templates are refused with zero stored.
//!
//! RED NOW (baseline): no birthday command, store, sweep or docs exist, so
//! every call below either resolves to the [`BaselineBirthdaySweep`] adapter
//! (which replays today's unwired behavior: do nothing, return `Ok`) or to
//! the router's unknown-command path. Registry, router, publish-set, docs,
//! dispatch and sweep tests fail on their assertions; only the controls pass.
//!
//! GREEN (builder): wire the feature so every test passes. The sweep entry is
//! specified exactly; everything behind it is the builder's design. Add ONE
//! inherent method with EXACTLY this signature:
//! ```ignore
//! impl MemberRuntime {
//!     pub async fn run_birthday_sweep(
//!         pool: &PgPool,
//!         rest: &ActionExecutor,
//!         guild: &str,
//!         now_ms: i64,
//!     ) -> Result<(), ErrorClass>;
//! }
//! ```
//! Inherent methods shadow trait methods, so these calls flip from the
//! adapter to the real sweep with no test edit. The body may delegate to any
//! builder-owned module. Then register one supervised daily tick that calls
//! it; that one-line registration is review-verified (it is observationally
//! quiet here for the same reason the job-gate scheduler swap was: these
//! tests pin the tick-level mechanism with an injected clock).
//!
//! Command surface (pinned): `/birthday` with subcommands `set` (required
//! integer `month` 1–12, required integer `day` 1–31), `remove` (no options),
//! `show` (no options, not even a member option: own query only) and `config`
//! (optional `channel`, `role`, `cheer` string ≤ 200 chars; `ManageGuild`
//! enforced by the router, not just the handler).
//!
//! Sweep semantics (pinned choices, all asserted below):
//!
//! * Day boundary is the UTC date of `now_ms` (server timezone v1).
//! * Cheer ⟺ mention: every cheer names its celebrant as `<@id>` with
//!   `allowed_mentions.parse == []` (no ping), ≤ 200 chars, no
//!   `@everyone`/`@here`, posted to the configured channel only.
//! * `{user}` in the cheer template renders the celebrant mention; a template
//!   without it still gains the mention (appended). An unconfigured template
//!   falls back to a default cheer.
//! * Role: granted for the day, removed once the day passes, including stale
//!   grants held by non-celebrants. Grants are idempotent (no redundant PUT).
//! * Cap: at most [`CELEBRANT_CHEER_CAP`] cheers per guild per sweep, lowest
//!   member id first; overflow celebrants are still granted the role.
//! * Unconfigured guilds sweep to a no-op; partial config degrades (channel
//!   without role still cheers, role without channel still grants).
//! * Feb 29 is cheered on Feb 28 in non-leap years (and Feb 29 when it exists).
//! * Observability: `birthday.set`, `birthday.remove` and `birthday.cheer`
//!   tracing events carry guild + member ids and never month/day values;
//!   UTC dates in logs use full ISO stamps, never bare day numbers.
//!
//! Shapes accepted: only the inherent sweep above (exact name and signature);
//! any internal module, store-function or table layout behind it, provided
//! birthday tables carry `birthday` in their name and no year-like column;
//! any reply wording that satisfies the content pins; any overflow UX beyond
//! the cap (silence is fine); additional subcommands or options (superset).
//!
//! Seams this file depends on (builder: preserve them, or return corrections
//! to the test author): `CommandRuntime::new`, `router_with_commands`,
//! `publication_definitions`, `InteractionRouter::route_slash`,
//! `builtin_command_names`, `reserved_command_names`, `MockRest::start` /
//! `with_responder` / `requests` / `origin` / `shutdown`,
//! `ScriptedResponse::{json, status}`, `ActionExecutor::with_proxy`,
//! `TestDatabase::{create, pool, independent_pool, close}`,
//! `sqlx::migrate!("../cutover/migrations")`, `parse_iso_millis`,
//! `gateway::ensure_crypto_provider`, and the `MemberRuntime` type itself
//! (no method: the adapter hangs the specified entry on it).
//!
//! `baseline_adapter_keeps_the_unwired_path_callable` is load-bearing for CI:
//! its explicit `<MemberRuntime as BaselineBirthdaySweep>::` call keeps the
//! adapter live after GREEN, else clippy `-D warnings` fails on a never-used
//! trait the builder is not allowed to fix here.
//!
//! Database tests skip vacuously without `TWO_TEST_DATABASE_URL` and run in
//! the nextest lane (which provides it) with no `#[ignore]` routing needed:
//! this is a bins unit module, covered by the workspace lib/bins run.
//! Focused: `python3 scripts/cargo_cache.py run -- test -p two-bot --bin
//! two-bot birthday_acceptance`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::guild::Permissions;
use twilight_model::id::{AnonymizableId, Id};
use twilight_model::user::User;
use two_bot_core::commands::{CommandOption, CommandOptionType};
use two_bot_core::{
    builtin_command_names, parse_iso_millis, reserved_command_names, RouterGates, SlashContext,
    SlashOutcome,
};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

use crate::command_runtime::{publication_definitions, router_with_commands, CommandRuntime};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::jobs::ErrorClass;
use crate::member_runtime::MemberRuntime;

const GUILD_ID: u64 = 8128;
const GUILD_S: &str = "8128";
const CHANNEL_ID: u64 = 8129;
const CHANNEL_S: &str = "8129";
const ROLE_ID: &str = "8130";
const BOT_ID_S: &str = "8120";
/// May 1 celebrant.
const ALICE: u64 = 8101;
/// Second May 1 celebrant.
const BRUNO: u64 = 8102;
/// May 2 (not a May 1 celebrant).
const CARA: u64 = 8103;
/// Feb 29 (leap birthday).
const LEAP: u64 = 8104;
/// Member with no birthday stored.
const NOBODY: u64 = 8105;
/// First id of the cap-test celebrant block.
const CAP_FIRST_ID: u64 = 8201;
/// Celebrants seeded for the cap test ([`CELEBRANT_CHEER_CAP`] + 2).
const CAP_CELEBRANTS: u64 = 27;
/// Max cheers per guild per sweep. Storm bound: one fake-birthday wave cannot
/// flood a channel. Overflow celebrants still get the day role; only the
/// cheer posts are capped, lowest member id first (deterministic).
const CELEBRANT_CHEER_CAP: usize = 25;
const MAX_CHEER_TEMPLATE_CHARS: usize = 200;
const MAX_CHEER_CHARS: usize = 200;
const MAX_REPLY_CHARS: usize = 500;

/// May-1 birthday renderings that must never appear in logs. Bare day numbers
/// are excluded only via full-date camouflage: UTC dates in logs use full ISO
/// stamps (`2026-05-01`), never bare month/day numbers or names.
const PII_MAY: &[&str] = &[
    "May 1",
    "may 1",
    "5/1",
    "05/01",
    "\"month\":5",
    "\"month\": 5",
    "month=5",
    "\"day\":1",
    "\"day\": 1",
    "day=1",
];
/// Feb-29 birthday renderings that must never appear in logs.
const PII_FEB29: &[&str] = &[
    "Feb 29",
    "feb 29",
    "2/29",
    "02/29",
    "\"month\":2",
    "\"month\": 2",
    "month=2",
    "\"day\":29",
    "\"day\": 29",
    "day=29",
];

/// Baseline adapter: the specified sweep entry point, replaying today's
/// unwired behavior (nothing exists, so nothing posts, grants or removes)
/// until the builder's inherent method shadows it.
trait BaselineBirthdaySweep {
    async fn run_birthday_sweep(
        pool: &PgPool,
        rest: &ActionExecutor,
        guild: &str,
        now_ms: i64,
    ) -> Result<(), ErrorClass>;
}

impl BaselineBirthdaySweep for MemberRuntime {
    async fn run_birthday_sweep(
        _pool: &PgPool,
        _rest: &ActionExecutor,
        _guild: &str,
        _now_ms: i64,
    ) -> Result<(), ErrorClass> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

static NEXT_IX: AtomicU64 = AtomicU64::new(90_000);

fn gates_all() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
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
    }
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn opt(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

fn opt_sub(name: &str, nested: Vec<CommandDataOption>) -> CommandDataOption {
    opt(name, CommandOptionValue::SubCommand(nested))
}

fn opt_int(name: &str, value: i64) -> CommandDataOption {
    opt(name, CommandOptionValue::Integer(value))
}

/// One `/birthday …` interaction from `member` with `permissions` and a fresh
/// id/token, so repeated dispatches never collide on nonce recovery. Built on
/// the shared `slash` helper (which owns the deprecated wire fields), then
/// re-homed to this suite's guild and actor.
fn birthday_ix(
    subcommand: CommandDataOption,
    member: u64,
    permissions: Permissions,
) -> Interaction {
    let n = NEXT_IX.fetch_add(1, Ordering::Relaxed);
    let mut ix =
        crate::command_runtime_tests::slash("birthday", Some(CHANNEL_ID), vec![subcommand]);
    ix.id = Id::new(n);
    ix.token = format!("birthday-test-{n}");
    ix.guild_id = Some(Id::new(GUILD_ID));
    ix.authorizing_integration_owners.guild = Some(AnonymizableId::Id(Id::new(GUILD_ID)));
    let resolved = ix.member.as_mut().expect("guild member");
    resolved.user = Some(user(member));
    resolved.permissions = Some(permissions);
    ix.user = Some(user(member));
    ix
}

fn set_ix(member: u64, month: i64, day: i64) -> Interaction {
    birthday_ix(
        opt_sub("set", vec![opt_int("month", month), opt_int("day", day)]),
        member,
        Permissions::empty(),
    )
}

fn remove_ix(member: u64) -> Interaction {
    birthday_ix(opt_sub("remove", Vec::new()), member, Permissions::empty())
}

fn show_ix(member: u64) -> Interaction {
    birthday_ix(opt_sub("show", Vec::new()), member, Permissions::empty())
}

fn config_ix(
    channel: Option<u64>,
    role: Option<u64>,
    cheer: Option<&str>,
    member: u64,
    permissions: Permissions,
) -> Interaction {
    let mut nested = Vec::new();
    if let Some(channel) = channel {
        nested.push(opt(
            "channel",
            CommandOptionValue::Channel(Id::new(channel)),
        ));
    }
    if let Some(role) = role {
        nested.push(opt("role", CommandOptionValue::Role(Id::new(role))));
    }
    if let Some(cheer) = cheer {
        nested.push(opt("cheer", CommandOptionValue::String(cheer.to_owned())));
    }
    birthday_ix(opt_sub("config", nested), member, permissions)
}

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool")
}

fn executor_for(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-birthday-test-token".to_owned(),
        Some(mock.origin()),
    )
    .expect("mock executor")
}

fn runtime_for(pool: PgPool, executor: ActionExecutor) -> Arc<CommandRuntime> {
    let gates = gates_all();
    CommandRuntime::new(pool, executor, router_with_commands(gates), GUILD_ID, true)
}

/// Live role state behind the loopback Discord double: member GETs read it,
/// role PUTs/DELETEs mutate it, so assertions are state-based and a fix may
/// change read counts without breaking the contract.
#[derive(Default)]
struct DiscoState {
    roles: Mutex<HashMap<String, HashSet<String>>>,
}

async fn start_stateful(state: Arc<DiscoState>) -> MockRest {
    MockRest::with_responder(move |req: &RestRequest| -> ScriptedResponse {
        let path = req.path.split('?').next().unwrap_or("").to_owned();
        let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if req.method == "POST" && seg.last() == Some(&"callback") {
            return ScriptedResponse::json(200, json!({"id": "990000000000000001"}));
        }
        if req.method == "POST"
            && seg.len() == 5
            && seg.first() == Some(&"api")
            && seg.get(1) == Some(&"v10")
            && seg.get(2) == Some(&"channels")
            && seg.get(4) == Some(&"messages")
        {
            return ScriptedResponse::json(200, json!({"id": "990000000000000002"}));
        }
        if seg.len() >= 6
            && seg.first() == Some(&"api")
            && seg.get(1) == Some(&"v10")
            && seg.get(2) == Some(&"guilds")
            && seg.get(4) == Some(&"members")
        {
            let member = seg[5].to_owned();
            if seg.len() == 8 && seg.get(6) == Some(&"roles") {
                let role = seg[7].to_owned();
                let mut map = state.roles.lock().expect("roles lock");
                if req.method == "PUT" {
                    map.entry(member).or_default().insert(role);
                } else if req.method == "DELETE" {
                    if let Some(set) = map.get_mut(&member) {
                        set.remove(&role);
                    }
                }
                return ScriptedResponse::json(200, json!({"id": "990000000000000003"}));
            }
            if req.method == "GET" && seg.len() == 6 {
                let map = state.roles.lock().expect("roles lock");
                let list: Vec<&str> = map
                    .get(&member)
                    .map(|set| set.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                return ScriptedResponse::json(
                    200,
                    json!({"user": {"id": member}, "roles": list, "deaf": false, "mute": false}),
                );
            }
        }
        if path == "/api/v10/users/@me" {
            return ScriptedResponse::json(200, json!({"id": BOT_ID_S, "bot": true}));
        }
        ScriptedResponse::json(200, json!({}))
    })
    .await
}

fn db_url() -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("SKIP birthday acceptance: TWO_TEST_DATABASE_URL is not set");
            None
        }
    }
}

async fn test_db(url: &str) -> TestDatabase {
    TestDatabase::create(url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated test database")
}

fn utc_ms(stamp: &str) -> i64 {
    parse_iso_millis(stamp).expect("fixture stamp")
}

fn callbacks(reqs: &[RestRequest]) -> Vec<&RestRequest> {
    reqs.iter()
        .filter(|r| r.method == "POST" && r.path.contains("/callback"))
        .collect()
}

/// Exactly one interaction callback per dispatch: the runtime answers every
/// routed interaction exactly once.
fn only_callback(reqs: &[RestRequest]) -> Value {
    let found = callbacks(reqs);
    assert_eq!(found.len(), 1, "one interaction callback");
    serde_json::from_slice(&found[0].body).expect("callback json")
}

fn content_of(reply: &Value) -> &str {
    reply["data"]["content"].as_str().expect("text content")
}

fn assert_ephemeral(reply: &Value, what: &str) {
    assert_eq!(reply["type"], 4, "{what}: immediate reply");
    assert_eq!(reply["data"]["flags"], 64, "{what}: ephemeral");
}

fn assert_concise(content: &str, what: &str) {
    assert!(
        content.chars().count() <= MAX_REPLY_CHARS,
        "{what}: concise ({} chars): {content:?}",
        content.chars().count()
    );
}

/// Birthday callbacks never carry mention syntax: hostile input must not echo
/// into a ping, whatever plumbing answers.
fn assert_callback_mention_safe(content: &str, what: &str) {
    for banned in ["@everyone", "@here", "<@"] {
        assert!(
            !content.contains(banned),
            "{what}: no {banned:?} in callback: {content:?}"
        );
    }
}

fn channel_posts<'a>(reqs: &'a [RestRequest], channel: &str) -> Vec<&'a RestRequest> {
    let want = format!("/api/v10/channels/{channel}/messages");
    reqs.iter()
        .filter(|r| r.method == "POST" && r.path.split('?').next().unwrap_or("") == want)
        .collect()
}

/// (member id, role id) for role PUTs/DELETEs in arrival order.
fn role_ops(reqs: &[RestRequest], method: &str) -> Vec<(String, String)> {
    reqs.iter()
        .filter(|r| r.method == method)
        .filter_map(|r| {
            let path = r.path.split('?').next()?;
            let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if seg.len() == 8
                && seg.first() == Some(&"api")
                && seg.get(1) == Some(&"v10")
                && seg.get(2) == Some(&"guilds")
                && seg.get(4) == Some(&"members")
                && seg.get(6) == Some(&"roles")
            {
                Some((seg[5].to_owned(), seg[7].to_owned()))
            } else {
                None
            }
        })
        .collect()
}

fn cheer_body(post: &RestRequest) -> Value {
    serde_json::from_slice(&post.body).expect("cheer json")
}

/// Pin the cheer copy contract: names the celebrant, concise, silent.
fn assert_cheer(post: &RestRequest, member: u64, what: &str) {
    let body = cheer_body(post);
    let content = body["content"].as_str().expect("text cheer");
    assert!(
        content.contains(&format!("<@{member}>")),
        "{what}: cheer names the celebrant: {content:?}"
    );
    assert!(
        content.chars().count() <= MAX_CHEER_CHARS,
        "{what}: cheer stays concise: {content:?}"
    );
    for banned in ["@everyone", "@here"] {
        assert!(
            !content.contains(banned),
            "{what}: cheer never mass-pings: {content:?}"
        );
    }
    assert_eq!(
        body["allowed_mentions"]["parse"],
        json!([]),
        "{what}: the post is mention-safe"
    );
}

fn member_roles(state: &DiscoState, member: &str) -> HashSet<String> {
    state
        .roles
        .lock()
        .expect("roles lock")
        .get(member)
        .cloned()
        .unwrap_or_default()
}

fn lines_with<'a>(logs: &'a str, marker: &str) -> Vec<&'a str> {
    logs.lines().filter(|l| l.contains(marker)).collect()
}

fn assert_no_pii(logs: &str, forbidden: &[&str], what: &str) {
    for shape in forbidden {
        assert!(
            !logs.contains(shape),
            "{what}: no birthday PII ({shape:?}) in logs"
        );
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn captured_logs(cap: &Capture) -> String {
    String::from_utf8(cap.0.lock().expect("capture lock").clone()).expect("utf8 logs")
}

async fn dispatch_set(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    member: u64,
    month: i64,
    day: i64,
) -> Value {
    let before = callbacks(&mock.requests()).len();
    runtime.on_interaction(&set_ix(member, month, day)).await;
    let reqs = mock.requests();
    let found = callbacks(&reqs);
    assert_eq!(found.len(), before + 1, "set answers exactly one callback");
    serde_json::from_slice(&found[before].body).expect("callback json")
}

async fn sweep(pool: &PgPool, executor: &ActionExecutor, now_ms: i64) {
    MemberRuntime::run_birthday_sweep(pool, executor, GUILD_S, now_ms)
        .await
        .expect("sweep succeeds");
}

// ---------------------------------------------------------------------------
// Always-run pins: registry, router, publish set, docs, adapter liveness
// ---------------------------------------------------------------------------

#[test]
fn birthday_is_reserved_builtin() {
    assert!(
        builtin_command_names().contains("birthday"),
        "birthday is a builtin (custom rows cannot squat it)"
    );
    assert!(
        reserved_command_names().contains("birthday"),
        "birthday is reserved at publish"
    );
}

#[test]
fn birthday_routes_in_guild_and_fences_outside() {
    let router = router_with_commands(gates_all());
    let in_guild = router.route_slash(&SlashContext {
        name: "birthday",
        guild_id: Some(GUILD_ID),
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    });
    assert!(
        matches!(in_guild, SlashOutcome::Handled { .. }),
        "birthday routes in-guild, got {in_guild:?}"
    );
    let no_guild = router.route_slash(&SlashContext {
        name: "birthday",
        guild_id: None,
        actor_permissions: Some(u64::MAX),
        custom_row: None,
    });
    assert!(
        matches!(no_guild, SlashOutcome::Ignore),
        "birthday stays fenced outside the guild, got {no_guild:?}"
    );
}

fn find_sub<'a>(options: &'a [CommandOption], name: &str) -> &'a CommandOption {
    options
        .iter()
        .find(|o| o.name == name && o.kind == CommandOptionType::SubCommand.as_u8())
        .unwrap_or_else(|| panic!("subcommand {name}"))
}

fn find_option<'a>(options: &'a [CommandOption], name: &str) -> &'a CommandOption {
    options
        .iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| panic!("option {name}"))
}

fn all_option_names(options: &[CommandOption], out: &mut Vec<String>) {
    for option in options {
        out.push(option.name.clone());
        all_option_names(&option.options, out);
    }
}

#[test]
fn birthday_publish_set_has_bounded_options_and_no_year() {
    let router = router_with_commands(gates_all());
    let defs = publication_definitions(&router, gates_all(), Some(&[]))
        .expect("publish set builds")
        .expect("publication is not deferred");
    let birthday = defs
        .iter()
        .find(|d| d.name == "birthday")
        .expect("birthday is published");
    assert!(!birthday.dm_permission, "birthdays are guild-only");

    let set = find_sub(&birthday.options, "set");
    let month = find_option(&set.options, "month");
    assert_eq!(month.kind, CommandOptionType::Integer.as_u8());
    assert_eq!(month.required, Some(true));
    assert_eq!(month.min_value, Some(1));
    assert_eq!(month.max_value, Some(12));
    let day = find_option(&set.options, "day");
    assert_eq!(day.kind, CommandOptionType::Integer.as_u8());
    assert_eq!(day.required, Some(true));
    assert_eq!(day.min_value, Some(1));
    assert_eq!(day.max_value, Some(31));

    let remove = find_sub(&birthday.options, "remove");
    assert!(remove.options.is_empty(), "remove takes no options");
    let show = find_sub(&birthday.options, "show");
    assert!(
        show.options.is_empty(),
        "show takes no options: own query only, never another member"
    );

    let config = find_sub(&birthday.options, "config");
    let channel = find_option(&config.options, "channel");
    assert_eq!(channel.kind, CommandOptionType::Channel.as_u8());
    assert_ne!(channel.required, Some(true), "channel is optional");
    let role = find_option(&config.options, "role");
    assert_eq!(role.kind, CommandOptionType::Role.as_u8());
    assert_ne!(role.required, Some(true), "role is optional");
    let cheer = find_option(&config.options, "cheer");
    assert_eq!(cheer.kind, CommandOptionType::String.as_u8());
    assert_ne!(cheer.required, Some(true), "cheer is optional");
    assert_eq!(
        cheer.max_length,
        Some(MAX_CHEER_TEMPLATE_CHARS as u32),
        "cheer template is bounded"
    );

    let mut names = Vec::new();
    all_option_names(&birthday.options, &mut names);
    for name in &names {
        assert!(
            !name.to_lowercase().contains("year"),
            "no year option anywhere: {names:?}"
        );
    }
}

#[test]
fn birthday_documented_in_commands_reference() {
    let docs = include_str!("../../../docs/commands.md");
    assert!(
        docs.contains("## `/birthday`"),
        "the commands reference documents /birthday"
    );
}

#[tokio::test]
async fn baseline_adapter_keeps_the_unwired_path_callable() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({}))).await;
    let executor = executor_for(&mock);
    let pool = lazy_pool();
    // Explicitly qualified: this is the call shape that keeps the adapter
    // trait live after GREEN (clippy `-D warnings` would otherwise fail on a
    // never-used trait the builder cannot fix here).
    let outcome =
        <MemberRuntime as BaselineBirthdaySweep>::run_birthday_sweep(&pool, &executor, GUILD_S, 0)
            .await;
    assert!(outcome.is_ok(), "the unwired stub succeeds");
    assert!(
        mock.requests().is_empty(),
        "the unwired stub writes nothing"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_command_still_refused_for_gibberish() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({}))).await;
    let runtime = runtime_for(lazy_pool(), executor_for(&mock));
    let mut ix = birthday_ix(opt_sub("set", Vec::new()), ALICE, Permissions::empty());
    if let Some(InteractionData::ApplicationCommand(data)) = ix.data.as_mut() {
        data.name = "zzz-no-such-command".to_owned();
    }
    runtime.on_interaction(&ix).await;
    let reply = only_callback(&mock.requests());
    assert_ephemeral(&reply, "unknown command");
    assert!(
        !content_of(&reply).is_empty(),
        "unknown commands get a refusal"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn help_still_answers_through_dispatch_fixture() {
    let mock = MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({}))).await;
    let runtime = runtime_for(lazy_pool(), executor_for(&mock));
    let mut ix = birthday_ix(opt_sub("set", Vec::new()), ALICE, Permissions::empty());
    if let Some(InteractionData::ApplicationCommand(data)) = ix.data.as_mut() {
        data.name = "help".to_owned();
        data.options.clear();
    }
    runtime.on_interaction(&ix).await;
    let reply = only_callback(&mock.requests());
    assert_ephemeral(&reply, "help");
    assert!(!content_of(&reply).is_empty(), "help answers");
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Database acceptance (vacuous skip without TWO_TEST_DATABASE_URL)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_show_remove_round_trip() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let runtime = runtime_for(pool, executor_for(&mock));

    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert_ephemeral(&reply, "set");
    let content = content_of(&reply).to_owned();
    assert_concise(&content, "set");
    assert_callback_mention_safe(&content, "set");
    assert!(content.contains("May"), "set names the month: {content:?}");

    runtime.on_interaction(&show_ix(ALICE)).await;
    let reply = only_callback(&mock.requests());
    assert_ephemeral(&reply, "show");
    let content = content_of(&reply).to_owned();
    assert_concise(&content, "show");
    assert_callback_mention_safe(&content, "show");
    assert!(
        content.contains("May"),
        "show renders the month: {content:?}"
    );
    assert!(
        !content.contains("1990") && !content.contains("2000"),
        "show renders no year: {content:?}"
    );

    let reply = dispatch_set(&runtime, &mock, ALICE, 6, 15).await;
    assert_ephemeral(&reply, "update");
    assert!(
        content_of(&reply).contains("June"),
        "update names the new month"
    );
    runtime.on_interaction(&show_ix(ALICE)).await;
    let shown = content_of(&only_callback(&mock.requests())).to_owned();
    assert!(shown.contains("June"), "show follows the update: {shown:?}");
    assert!(!shown.contains("May"), "update replaces: {shown:?}");

    runtime.on_interaction(&remove_ix(ALICE)).await;
    let reply = only_callback(&mock.requests());
    assert_ephemeral(&reply, "remove");
    let lowered = content_of(&reply).to_lowercase();
    assert!(
        lowered.contains("remov") || lowered.contains("delet"),
        "remove confirms: {lowered:?}"
    );

    runtime.on_interaction(&show_ix(ALICE)).await;
    let lowered = content_of(&only_callback(&mock.requests())).to_lowercase();
    assert!(
        lowered.contains("not set") || lowered.contains("no birthday"),
        "show after remove reports unset: {lowered:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn invalid_dates_refused_with_zero_state() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    for (month, day) in [
        (0, 1),
        (13, 1),
        (5, 0),
        (5, 32),
        (2, 30),
        (4, 31),
        (6, 31),
        (9, 31),
        (11, 31),
    ] {
        let reply = dispatch_set(&runtime, &mock, ALICE, month, day).await;
        assert_ephemeral(&reply, "invalid");
        let lowered = content_of(&reply).to_lowercase();
        assert!(
            lowered.contains("invalid"),
            "month {month} day {day} is refused: {lowered:?}"
        );
    }

    runtime.on_interaction(&show_ix(ALICE)).await;
    let lowered = content_of(&only_callback(&mock.requests())).to_lowercase();
    assert!(
        lowered.contains("not set") || lowered.contains("no birthday"),
        "refusals store nothing: {lowered:?}"
    );

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    sweep(&pool, &executor, utc_ms("2026-02-28T12:00:00.000Z")).await;
    assert!(
        channel_posts(&mock.requests(), CHANNEL_S).is_empty(),
        "refusals cheer nothing"
    );

    // Feb 29 is a real (leap) birthday: accepted, then removed.
    let reply = dispatch_set(&runtime, &mock, ALICE, 2, 29).await;
    assert_ephemeral(&reply, "leap set");
    assert!(
        content_of(&reply).contains("February") || content_of(&reply).contains("Feb"),
        "leap set confirms: {:?}",
        content_of(&reply)
    );
    runtime.on_interaction(&remove_ix(ALICE)).await;

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn birthday_tables_store_no_year() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();

    let columns: Vec<(String, String)> =
        sqlx::query_as("SELECT table_name, column_name FROM information_schema.columns WHERE table_schema = 'public' AND table_name LIKE '%birthday%'")
            .fetch_all(&pool)
            .await
            .expect("inspect birthday tables");
    let tables: HashSet<&str> = columns.iter().map(|(t, _)| t.as_str()).collect();
    assert!(!tables.is_empty(), "the migration creates birthday tables");
    for (table, column) in &columns {
        let lowered = column.to_lowercase();
        assert!(
            !lowered.contains("year") && lowered != "yob",
            "no year storage in {table}.{column}"
        );
    }

    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn config_requires_manage_guild_and_bounds_cheer() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    // Router-enforced permission: the same refusal as any other ManageGuild
    // command, not a handler-side text.
    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            None,
            None,
            ALICE,
            Permissions::empty(),
        ))
        .await;
    let denied = content_of(&only_callback(&mock.requests())).to_owned();
    let mut command_ix = birthday_ix(opt_sub("set", Vec::new()), ALICE, Permissions::empty());
    if let Some(InteractionData::ApplicationCommand(data)) = command_ix.data.as_mut() {
        data.name = "command".to_owned();
        data.options.clear();
    }
    runtime.on_interaction(&command_ix).await;
    let reference = content_of(&only_callback(&mock.requests())).to_owned();
    assert_eq!(
        denied, reference,
        "config refusal matches the router permission refusal"
    );

    // Hostile templates are refused and store nothing.
    for hostile in [
        "@everyone party time",
        "hi @here",
        "<@&8130> yay",
        "<@8101> yo",
        "2000 miles of @everyone",
    ] {
        runtime
            .on_interaction(&config_ix(
                None,
                None,
                Some(hostile),
                ALICE,
                Permissions::MANAGE_GUILD,
            ))
            .await;
        let lowered = content_of(&only_callback(&mock.requests())).to_lowercase();
        assert!(
            lowered.contains("invalid"),
            "hostile template refused: {hostile:?} -> {lowered:?}"
        );
    }
    let long = "x".repeat(MAX_CHEER_TEMPLATE_CHARS + 1);
    runtime
        .on_interaction(&config_ix(
            None,
            None,
            Some(&long),
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let lowered = content_of(&only_callback(&mock.requests())).to_lowercase();
    assert!(
        lowered.contains("invalid"),
        "overlong template refused: {lowered:?}"
    );

    // Valid config confirms; the refused templates left zero stored state,
    // so the sweep below uses the default cheer, not hostile text.
    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = only_callback(&mock.requests());
    assert_ephemeral(&reply, "config");
    let lowered = content_of(&reply).to_lowercase();
    assert!(
        lowered.contains("configur") || lowered.contains("saved") || lowered.contains("updated"),
        "config confirms: {lowered:?}"
    );

    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 1, "one cheer with default template");
    let body = cheer_body(posts[0]);
    let cheer = body["content"].as_str().expect("text cheer");
    assert!(
        !cheer.contains("@everyone") && !cheer.contains("@here") && !cheer.contains("<@&"),
        "refused templates never render: {cheer:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn own_query_only_never_leaks_other_member() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let runtime = runtime_for(pool, executor_for(&mock));

    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    let reply = dispatch_set(&runtime, &mock, BRUNO, 7, 4).await;
    assert!(content_of(&reply).contains("July"));

    runtime.on_interaction(&show_ix(BRUNO)).await;
    let shown = content_of(&only_callback(&mock.requests())).to_owned();
    assert!(shown.contains("July"), "Bruno sees his own: {shown:?}");
    assert!(
        !shown.contains("May"),
        "Bruno never sees Alice's: {shown:?}"
    );

    runtime.on_interaction(&show_ix(ALICE)).await;
    let shown = content_of(&only_callback(&mock.requests())).to_owned();
    assert!(shown.contains("May"), "Alice sees her own: {shown:?}");
    assert!(
        !shown.contains("July"),
        "Alice never sees Bruno's: {shown:?}"
    );

    runtime.on_interaction(&show_ix(NOBODY)).await;
    let lowered = content_of(&only_callback(&mock.requests())).to_lowercase();
    assert!(
        lowered.contains("not set") || lowered.contains("no birthday"),
        "unset member reports unset: {lowered:?}"
    );
    assert!(
        !lowered.contains("may") && !lowered.contains("july"),
        "unset member learns nothing: {lowered:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test(flavor = "current_thread")]
async fn set_and_remove_emit_redacted_events_without_pii() {
    let Some(url) = db_url() else { return };
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let runtime = runtime_for(pool, executor_for(&mock));

    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    runtime.on_interaction(&remove_ix(ALICE)).await;

    let logs = captured_logs(&capture);
    let sets = lines_with(&logs, "birthday.set");
    assert!(!sets.is_empty(), "set emits a redacted event");
    assert!(
        sets.iter()
            .any(|l| l.contains(&ALICE.to_string()) && l.contains(GUILD_S)),
        "set event carries ids: {sets:?}"
    );
    let removes = lines_with(&logs, "birthday.remove");
    assert!(!removes.is_empty(), "remove emits a redacted event");
    assert_no_pii(&logs, PII_MAY, "set/remove");

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn sweep_posts_exactly_one_cheer_per_celebrant() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    for (member, month, day) in [(ALICE, 5, 1), (BRUNO, 5, 1), (CARA, 5, 2)] {
        let reply = dispatch_set(&runtime, &mock, member, month, day).await;
        assert_ephemeral(&reply, "seed set");
    }

    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 2, "exactly one cheer per celebrant");
    let mut mentioned: Vec<u64> = Vec::new();
    for post in &posts {
        let body = cheer_body(post);
        let content = body["content"].as_str().expect("text cheer");
        for member in [ALICE, BRUNO] {
            if content.contains(&format!("<@{member}>")) {
                mentioned.push(member);
            }
        }
        assert!(
            !content.contains(&format!("<@{CARA}>")),
            "tomorrow's celebrant is not cheered today: {content:?}"
        );
    }
    mentioned.sort_unstable();
    assert_eq!(mentioned, vec![ALICE, BRUNO], "both celebrants cheered");
    for post in &posts {
        let body = cheer_body(post);
        let content = body["content"].as_str().expect("text cheer");
        let member = if content.contains(&format!("<@{ALICE}>")) {
            ALICE
        } else {
            BRUNO
        };
        assert_cheer(post, member, "celebrant cheer");
    }

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn sweep_cycles_birthday_role_for_the_day() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    // Cara holds a stale birthday role though her birthday is May 2.
    state
        .roles
        .lock()
        .expect("roles lock")
        .insert(CARA.to_string(), HashSet::from([ROLE_ID.to_owned()]));
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));

    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    let puts = role_ops(&mock.requests(), "PUT");
    assert!(
        puts.contains(&(ALICE.to_string(), ROLE_ID.to_owned())),
        "the day's celebrant is granted the role: {puts:?}"
    );
    assert!(
        member_roles(&state, &ALICE.to_string()).contains(ROLE_ID),
        "grant lands in member state"
    );
    let deletes = role_ops(&mock.requests(), "DELETE");
    assert!(
        deletes.contains(&(CARA.to_string(), ROLE_ID.to_owned())),
        "stale grants are cleaned on sight: {deletes:?}"
    );
    assert!(
        !member_roles(&state, &CARA.to_string()).contains(ROLE_ID),
        "stale cleanup lands in member state"
    );

    sweep(&pool, &executor, utc_ms("2026-05-02T12:00:00.000Z")).await;

    let deletes = role_ops(&mock.requests(), "DELETE");
    assert!(
        deletes.contains(&(ALICE.to_string(), ROLE_ID.to_owned())),
        "yesterday's celebrant loses the role: {deletes:?}"
    );
    assert!(
        !member_roles(&state, &ALICE.to_string()).contains(ROLE_ID),
        "removal lands in member state"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn restart_never_double_posts_nor_strands_role() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));

    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    assert_eq!(
        channel_posts(&mock.requests(), CHANNEL_S).len(),
        1,
        "first sweep cheers once"
    );
    assert_eq!(
        role_ops(&mock.requests(), "PUT").len(),
        1,
        "first sweep grants once"
    );

    // Restart: a fresh pool over the same database (no shared pool state),
    // then the same-day sweep again.
    let pool2 = db.independent_pool().await.expect("second pool");
    sweep(&pool2, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    assert_eq!(
        channel_posts(&mock.requests(), CHANNEL_S).len(),
        1,
        "restart never double-posts"
    );
    assert_eq!(
        role_ops(&mock.requests(), "PUT").len(),
        1,
        "restart never double-grants"
    );

    // The next day still cycles the role: nothing stranded by the restart.
    sweep(&pool2, &executor, utc_ms("2026-05-02T12:00:00.000Z")).await;
    let deletes = role_ops(&mock.requests(), "DELETE");
    assert!(
        deletes.contains(&(ALICE.to_string(), ROLE_ID.to_owned())),
        "restart never strands the role: {deletes:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn sweep_caps_daily_cheers_but_roles_every_celebrant() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    for id in CAP_FIRST_ID..CAP_FIRST_ID + CAP_CELEBRANTS {
        let reply = dispatch_set(&runtime, &mock, id, 5, 1).await;
        assert!(content_of(&reply).contains("May"), "cap seed {id} stores");
    }

    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(
        posts.len(),
        CELEBRANT_CHEER_CAP,
        "cheers are capped per sweep"
    );
    let mut cheered: Vec<u64> = Vec::new();
    for post in &posts {
        let content = cheer_body(post)["content"]
            .as_str()
            .expect("text cheer")
            .to_owned();
        for id in CAP_FIRST_ID..CAP_FIRST_ID + CAP_CELEBRANTS {
            if content.contains(&format!("<@{id}>")) {
                cheered.push(id);
            }
        }
    }
    cheered.sort_unstable();
    let expected: Vec<u64> = (CAP_FIRST_ID..CAP_FIRST_ID + CELEBRANT_CHEER_CAP as u64).collect();
    assert_eq!(cheered, expected, "lowest ids cheer first");
    let puts = role_ops(&mock.requests(), "PUT");
    assert_eq!(
        puts.len() as u64,
        CAP_CELEBRANTS,
        "every celebrant is still granted the role: {puts:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn sweep_uses_configured_template_and_channel() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            Some("HBD {user}!"),
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    let reply = dispatch_set(&runtime, &mock, CARA, 5, 2).await;
    assert!(content_of(&reply).contains("May"));

    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 1, "one cheer in the configured channel");
    let body = cheer_body(posts[0]);
    assert_eq!(
        body["content"].as_str(),
        Some("HBD <@8101>!"),
        "{{user}} renders the celebrant mention"
    );
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));

    // A template without the placeholder still names the celebrant.
    runtime
        .on_interaction(&config_ix(
            None,
            None,
            Some("No placeholder here"),
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    sweep(&pool, &executor, utc_ms("2026-05-02T12:00:00.000Z")).await;
    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 2, "the next day cheers its celebrant");
    let content = cheer_body(posts[1])["content"]
        .as_str()
        .expect("text cheer")
        .to_owned();
    assert!(
        content.contains("No placeholder here"),
        "template text renders: {content:?}"
    );
    assert!(
        content.contains(&format!("<@{CARA}>")),
        "the mention is appended: {content:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn feb29_celebrated_on_feb28_in_nonleap_years() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, LEAP, 2, 29).await;
    assert_ephemeral(&reply, "leap set");

    sweep(&pool, &executor, utc_ms("2026-02-27T12:00:00.000Z")).await;
    assert!(
        channel_posts(&mock.requests(), CHANNEL_S).is_empty(),
        "no early cheer on Feb 27"
    );
    sweep(&pool, &executor, utc_ms("2026-02-28T12:00:00.000Z")).await;
    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 1, "Feb 29 is cheered on Feb 28");
    assert_cheer(posts[0], LEAP, "leap cheer");
    sweep(&pool, &executor, utc_ms("2026-03-01T12:00:00.000Z")).await;
    assert_eq!(
        channel_posts(&mock.requests(), CHANNEL_S).len(),
        1,
        "no second cheer on Mar 1"
    );
    sweep(&pool, &executor, utc_ms("2028-02-29T12:00:00.000Z")).await;
    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 2, "the true leap day cheers too");
    assert_cheer(posts[1], LEAP, "true leap-day cheer");

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn unconfigured_guild_sweep_writes_nothing() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    // No config at all: stored birthdays sweep to silence.
    dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    assert!(
        channel_posts(&mock.requests(), CHANNEL_S).is_empty(),
        "no channel, no cheers"
    );
    assert!(
        role_ops(&mock.requests(), "PUT").is_empty()
            && role_ops(&mock.requests(), "DELETE").is_empty(),
        "no role, no grants"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test(flavor = "current_thread")]
async fn sweep_cheers_emit_redacted_events_without_pii() {
    let Some(url) = db_url() else { return };
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    for (member, month, day) in [(ALICE, 5, 1), (BRUNO, 5, 1)] {
        let reply = dispatch_set(&runtime, &mock, member, month, day).await;
        assert!(content_of(&reply).contains("May"));
    }
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    let logs = captured_logs(&capture);
    let cheers = lines_with(&logs, "birthday.cheer");
    assert!(cheers.len() >= 2, "each cheer emits a redacted event");
    for member in [ALICE, BRUNO] {
        assert!(
            cheers
                .iter()
                .any(|l| l.contains(&member.to_string()) && l.contains(GUILD_S)),
            "cheer event carries ids for {member}: {cheers:?}"
        );
    }
    assert_no_pii(&logs, PII_MAY, "sweep");
    assert_no_pii(&logs, PII_FEB29, "sweep");

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn role_only_config_still_grants_without_cheers() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    // Role without a channel: grants flow, cheers wait.
    runtime
        .on_interaction(&config_ix(
            None,
            Some(8130),
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    assert!(
        channel_posts(&mock.requests(), CHANNEL_S).is_empty(),
        "no channel, no cheers"
    );
    assert_eq!(
        role_ops(&mock.requests(), "PUT").len(),
        1,
        "the role still grants"
    );

    // Adding the channel merges (role preserved): the uncheered celebrant is
    // cheered on re-sweep, with no redundant grant.
    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            None,
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;
    assert_eq!(
        channel_posts(&mock.requests(), CHANNEL_S).len(),
        1,
        "the late channel cheers the uncheered"
    );
    assert_eq!(
        role_ops(&mock.requests(), "PUT").len(),
        1,
        "grants are idempotent"
    );

    // And the preserved role still cycles the next day.
    sweep(&pool, &executor, utc_ms("2026-05-02T12:00:00.000Z")).await;
    let deletes = role_ops(&mock.requests(), "DELETE");
    assert!(
        deletes.contains(&(ALICE.to_string(), ROLE_ID.to_owned())),
        "merged config cycles the role: {deletes:?}"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}

#[tokio::test]
async fn channel_only_config_still_cheers_without_grants() {
    let Some(url) = db_url() else { return };
    let db = test_db(&url).await;
    let pool = db.pool().clone();
    let state = Arc::new(DiscoState::default());
    let mock = start_stateful(Arc::clone(&state)).await;
    let executor = executor_for(&mock);
    let runtime = runtime_for(pool.clone(), executor.clone());

    runtime
        .on_interaction(&config_ix(
            Some(CHANNEL_ID),
            None,
            None,
            ALICE,
            Permissions::MANAGE_GUILD,
        ))
        .await;
    let reply = dispatch_set(&runtime, &mock, ALICE, 5, 1).await;
    assert!(content_of(&reply).contains("May"));
    sweep(&pool, &executor, utc_ms("2026-05-01T12:00:00.000Z")).await;

    let reqs = mock.requests();
    let posts = channel_posts(&reqs, CHANNEL_S);
    assert_eq!(posts.len(), 1, "channel without role still cheers");
    assert_cheer(posts[0], ALICE, "partial cheer");
    assert!(
        role_ops(&mock.requests(), "PUT").is_empty(),
        "no role, no grants"
    );

    mock.shutdown().await;
    db.close().await.expect("drop test database");
}
