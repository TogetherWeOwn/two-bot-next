#![cfg(test)]
//! Protected acceptance: DISBOARD-style bump reminders at cooldown expiry.
//!
//! Failing-first pins for the bump-reminder build (roadmap item 3). The bot
//! watches for successful server bumps posted by allowlisted bumper-bot
//! accounts (DISBOARD-style listing bots), arms exactly one reminder for the
//! pinned cooldown, and pings the opt-in bump role exactly once at expiry.
//!
//! RED on baseline: no bump code exists (repo `bump` hits are counter/version
//! wording), so every `R` test below fails on missing effects while events are
//! still admitted (each test asserts admission first, proving the failure is
//! missing behavior, not a broken harness). The `C` controls already pass and
//! guard the builder against over-firing, spoofing and leaking internals.
//!
//! Expected results on the baseline (8 RED, 8 GREEN):
//! - RED (ignored DB): `success_embed_arms_exactly_one_pending_reminder`,
//!   `expiry_fires_exactly_one_role_ping`,
//!   `fresh_bump_replaces_pending_instead_of_stacking`,
//!   `pending_reminder_survives_runtime_restart_and_fires_once`,
//!   `opt_in_adds_member_to_bump_role_with_confirmation`,
//!   `opt_out_removes_member_from_bump_role_with_confirmation`,
//!   `help_lists_bump_commands_concisely`.
//! - RED (fast, offline): `default_cooldown_is_two_hours` (fallback returns 0).
//! - GREEN (fast, offline): `bump_fixtures_deserialize`,
//!   `fallback_reports_zero_cooldown_as_control`.
//! - GREEN (ignored DB controls): `non_success_embeds_never_arm`,
//!   `non_allowlisted_bots_never_arm`, `spoofed_user_embeds_never_arm`,
//!   `direct_messages_never_arm`,
//!   `opted_out_members_are_never_directly_mentioned`,
//!   `bump_replies_leak_no_bumper_internals`, `cooldown_does_not_fire_early`.
//!
//! Protected file: the builder must NEVER edit this file. Return necessary
//! test corrections to the test author. This file must compile both on the
//! baseline and after the builder lands; the trait fallback below provides the
//! one new-method surface on the baseline, and an inherent item shadows its
//! fallback once added. Everything else is driven through existing production
//! dispatch (`CommandRuntime::dispatch` / `dispatch_with_verdict`, the same
//! entry the gateway worker calls) with effects observed on the loopback REST
//! double, so no other new surface is referenced.
//!
//! Builder contract (all required for GREEN):
//! 1. Config is process env, read wherever the bump paths run (both
//!    production-constructed and test-constructed runtimes behave the same):
//!    `TWO_BUMP_REMINDERS=1` enables the feature;
//!    `TWO_BUMP_BOT_IDS` is the comma-separated allowlist of bumper-bot user
//!    IDs (tests always set exactly [`DISBOARD_ID`]); `TWO_BUMP_ROLE_ID` is
//!    the opt-in role to ping (tests use [`ROLE_ID`]);
//!    `TWO_BUMP_COOLDOWN_SECS` overrides the cooldown in seconds for tests and
//!    ops (tests use `2` and `3600`). Absent or unparseable override means the
//!    default from item 2. Any other unconfigured behavior is the builder's
//!    choice; document it.
//! 2. Add `impl CommandRuntime { pub(crate) fn bump_default_cooldown_secs(&self)
//!    -> u64 }` returning exactly `7200` (the DISBOARD 2-hour cooldown, the
//!    pinned default). The [`BumpDefaultCooldownFallback`] trait below returns
//!    `0` on the baseline so the pin REDs; the inherent item shadows it once
//!    added. Keep the signature identical.
//! 3. Success predicate (implement EXACTLY this; no extra conditions): a guild
//!    message arms if and only if its author ID is in `TWO_BUMP_BOT_IDS`, its
//!    author is a bot, and at least one embed description contains `Bump done`
//!    (ASCII case-insensitive). Embed color/title/footer/fields, message
//!    content, and attachments are IGNORED (the replace test re-arms with
//!    different cosmetics). Grounded in DISBOARD's "Bump done" success marker
//!    and 2-hour cooldown (roadmap research). The builder MUST verify the
//!    marker against a real DISBOARD success embed during the build and report
//!    the source; if reality differs, return a test correction to the test
//!    author instead of silently matching something else.
//! 4. Arm semantics: one durable pending reminder per guild (this bot is
//!    single-guild); a fresh success bump REPLACES the pending reminder
//!    instead of stacking. Arming completes within [`ARM_SETTLE`] of dispatch
//!    under test conditions (lanes empty; one store write). Messages outside
//!    the configured guild (including DMs) never arm and never error.
//! 5. Hook placement is free: anywhere in `CommandRuntime`'s dispatch path
//!    (`on_message` / `on_interaction` / admission / the custom-commands
//!    `handle_*` / `observe`) as long as it is reachable WITHOUT router
//!    registration (the acceptance router is baseline-built, so the builder
//!    cannot register `/bump` there) and WITHOUT custom-commands seeding.
//!    Tests initialize custom commands, set automations on, and dispatch
//!    creates with an automod `Accept` verdict. Bump observation must see
//!    bot-authored guild creates (do not hide behind a human-only gate).
//! 6. Fire semantics, mechanism-free: after the cooldown elapses, the ping
//!    lands within 30s of the next dispatched guild message, whether the
//!    implementation sweeps lazily on ingress/status or fires eagerly on a
//!    timer (tests always send a trigger message, so both pass; a pure
//!    wall-clock poller needs granularity under that deadline).
//!    The ping is one `POST` to the bumped channel whose content contains the
//!    role pill `<@&{role}>`, whose `allowed_mentions.roles` contains the role
//!    ID (otherwise Discord does not notify), with no `@everyone`/`@here`
//!    and no bumper internals. Each arming fires AT MOST once: re-sweeps,
//!    restarts and duplicate triggers never duplicate the ping (sent-marker,
//!    runtime-owned tasks, or equivalent; the tests cannot tell how).
//! 7. Opt-in/out are Discord role membership changes on the caller plus a
//!    confirmation reply: opt-in `PUT`s
//!    `guilds/{guild}/members/{member}/roles/{role}` exactly once; opt-out
//!    `DELETE`s it exactly once. Confirmations contain `opt`
//!    (case-insensitive) and none of [`ERROR_MARKERS`].
//! 8. `/bump status` reports the pending state: while armed the reply contains
//!    `armed` (case-insensitive) and NOT the idle phrase; while idle (none
//!    armed, or already fired) the reply contains the exact phrase
//!    `no bump reminder` (case-insensitive, e.g. "No bump reminder armed.").
//!    `/bump help` replies non-empty bump help of at most 1000 chars. Neither
//!    reply contains any of [`ERROR_MARKERS`]. Subcommands are
//!    `opt-in` / `opt-out` / `status` / `help` under one `/bump` root.
//! 9. No reply or ping (confirmations, status, help, fired ping) contains any
//!    of [`INTERNALS`]: the bumper bot ID, `TWO_BUMP` names, store/table
//!    words, or crash text. The role pill in the ping is the mechanism, not a
//!    leak. Error-marker words are likewise banned from success-path replies.
//! 10. Explicitly NOT pinned here (builder-owned, review-checked): slash
//!     registry publication (the registry derives from the test-built router,
//!     so acceptance cannot pin it; the builder must still make `/bump` exist
//!     in Discord), metrics/logs, multi-guild behavior, and the internal clock
//!     (wall vs tokio) behind the cooldown.
//!
//! Scope: the ignored tests need the explicit agent-testdb/CI URL via the
//! parent's guarded `TestDb` (full cutover migrations) plus loopback REST; the
//! two fast tests are offline (lazy pool, loopback REST, no dispatch). Live
//! time throughout: short-cooldown overrides replace 2-hour waits, so no
//! paused clock is used. The `TWO_BUMP_*` values are constant per test and
//! guarded by a process-wide async mutex, so the suite self-serializes even
//! under a parallel local runner; CI runs it serially via the existing
//! `gateway_tests` filter step with no workflow change.

mod review_regressions;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use super::*;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use twilight_gateway::Event;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    channel::Message,
    gateway::payload::incoming::{InteractionCreate, MessageCreate},
};
use two_bot_core::{automod_runtime::FunnelDisposition, RouterGates};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway_commands::GatewayCommandConfig;

/// Fake allowlisted bumper-bot user ID (a test double for DISBOARD's
/// application user; the only ID in `TWO_BUMP_BOT_IDS` in every test).
const DISBOARD_ID: &str = "765432109876543210";
/// Bot user ID that is NOT allowlisted (spoof/foreign control).
const FOREIGN_BOT_ID: &str = "999888777666555444";
const MEMBER_ID: &str = "3333";
const CHANNEL_ID: &str = "4444";
const ROLE_ID: &str = "8888";
const APP_ID: &str = "1111";

/// Success marker (ASCII case-insensitive substring of an embed description).
const SUCCESS_MARKER: &str = "bump done";
/// Armed status replies contain this word and NOT [`IDLE_PHRASE`].
const ARMED_WORD: &str = "armed";
/// Idle status replies contain this exact phrase (case-insensitive).
const IDLE_PHRASE: &str = "no bump reminder";

/// Failure language that must never appear in a success-path reply (checked
/// case-insensitively). Baseline unknown-command/silent paths trip the armed/
/// confirmation asserts through these or through missing markers.
const ERROR_MARKERS: [&str; 14] = [
    "unknown",
    "unavailable",
    "unrecognized",
    "not found",
    "no such",
    "failed",
    "failure",
    "went wrong",
    "try again",
    "error occurred",
    "couldn't",
    "could not",
    "unable to",
    "not implemented",
];

/// Bumper internals that must never appear in any reply or ping (checked
/// case-insensitively). Tight on purpose: no baseline error text contains
/// these, so the control stays GREEN until the builder writes replies.
const INTERNALS: [&str; 7] = [
    DISBOARD_ID,
    "two_bump",
    "fires_at",
    "bump_reminders",
    "sqlx",
    "postgres",
    "stack trace",
];

/// Live-time budgets. Absence is proven only by exhausting the window, so
/// controls always pay the full budget; presence asserts return on sighting.
const ARM_SETTLE: Duration = Duration::from_secs(3);
const SHORT_COOLDOWN_SECS: &str = "2";
const LONG_COOLDOWN_SECS: &str = "3600";
const POST_EXPIRY_SLEEP: Duration = Duration::from_secs(2);
const FIRE_BUDGET: Duration = Duration::from_secs(45);
const ABSENCE_BUDGET: Duration = Duration::from_secs(40);
const REPLY_BUDGET: Duration = Duration::from_secs(8);
const ROLE_BUDGET: Duration = Duration::from_secs(15);
const POLL_TICK: Duration = Duration::from_millis(50);

/// Baseline fallback for the builder's
/// `CommandRuntime::bump_default_cooldown_secs` (contract item 2). Returns `0`
/// on the baseline so the default pin REDs. An inherent
/// `bump_default_cooldown_secs` shadows this fallback once the builder adds it.
trait BumpDefaultCooldownFallback {
    fn bump_default_cooldown_secs(&self) -> u64;
}

impl BumpDefaultCooldownFallback for CommandRuntime {
    fn bump_default_cooldown_secs(&self) -> u64 {
        0
    }
}

/// Process-wide serializer for the `TWO_BUMP_*` process env the contract reads.
/// Held for the whole of each test; values are constant per test.
static BUMP_ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Saved prior env, restored on drop for cross-test hygiene.
struct BumpEnv {
    saved: Vec<(&'static str, Option<String>)>,
}

impl BumpEnv {
    /// Caller must hold [`BUMP_ENV_LOCK`].
    fn set(cooldown_secs: &str) -> Self {
        let vars = [
            ("TWO_BUMP_REMINDERS", "1"),
            ("TWO_BUMP_BOT_IDS", DISBOARD_ID),
            ("TWO_BUMP_ROLE_ID", ROLE_ID),
            ("TWO_BUMP_COOLDOWN_SECS", cooldown_secs),
        ];
        let saved = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect();
        for (key, value) in &vars {
            std::env::set_var(key, value);
        }
        Self { saved }
    }
}

impl Drop for BumpEnv {
    fn drop(&mut self) {
        for (key, prior) in self.saved.drain(..) {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        announcements: true,
        scorecard: true,
        automations: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

/// Route-aware loopback Discord: plausible receipts per route (role edits 204,
/// message creates carry IDs, callbacks ack) so builder-side reads never fail
/// on harness shapes. Assertions filter by exact method+path, so background
/// traffic cannot mask a missing effect or fake a present one.
fn responder() -> impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static {
    let ids = Arc::new(AtomicU64::new(9100));
    move |request: &RestRequest| {
        let path = &request.path;
        if request.method == "GET" && path.ends_with("/applications/@me") {
            return ScriptedResponse::json(200, json!({"id": APP_ID}));
        }
        if request.method == "GET" && path.ends_with("/guilds/2222/members/3333") {
            return ScriptedResponse::json(200, json!({"user": {"id": MEMBER_ID}, "roles": []}));
        }
        if request.method == "GET" && path.ends_with("/guilds/2222/roles") {
            return ScriptedResponse::json(200, json!([{"id": ROLE_ID, "name": "Bump Squad"}]));
        }
        if request.method == "GET" && path.ends_with("/guilds/2222") {
            return ScriptedResponse::json(200, json!({"id": "2222", "name": "Bump guild"}));
        }
        if request.method == "GET" && path.contains("/users/") {
            return ScriptedResponse::json(200, json!({"id": MEMBER_ID, "username": "tester"}));
        }
        if request.method == "GET" && path.contains("/channels/") {
            return ScriptedResponse::json(
                200,
                json!({"id": CHANNEL_ID, "type": 0, "name": "bump"}),
            );
        }
        if (request.method == "PUT" || request.method == "DELETE") && path.contains("/roles/") {
            return ScriptedResponse::status(204);
        }
        if request.method == "POST" && path.ends_with("/callback") {
            return ScriptedResponse::status(204);
        }
        if request.method == "POST" && path.contains("/messages") {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            return ScriptedResponse::json(200, json!({"id": id.to_string()}));
        }
        if request.method == "PATCH" && path.replace("%40", "@").ends_with("/messages/@original") {
            // Editing a deferred original is a successful message operation,
            // not another callback. The executor needs only a success status.
            return ScriptedResponse::status(204);
        }
        ScriptedResponse::json(200, json!({"id": "9000"}))
    }
}

async fn test_runtime(pool: PgPool, mock: &MockRest) -> Arc<CommandRuntime> {
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).unwrap();
    let runtime = CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates()),
        GUILD.parse().unwrap(),
        true,
    );
    let vars = HashMap::from([
        ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
        ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
        ("TWO_AUTOMOD".to_owned(), "0".to_owned()),
    ]);
    let config =
        GatewayCommandConfig::from_map_for_activation(2222, &vars, true).expect("command config");
    runtime.initialize_custom_commands_for_test(config);
    runtime
}

fn bot_author(id: &str) -> Value {
    json!({"id": id, "username": "DISBOARD", "discriminator": "0000", "avatar": null, "bot": true})
}

fn human_author() -> Value {
    // No `bot` key: Discord omits it for humans, and twilight defaults it to
    // false. The spoof control pins that shape explicitly.
    json!({"id": MEMBER_ID, "username": "tester", "discriminator": "0000", "avatar": null})
}

fn success_embeds() -> Value {
    json!([{"type": "rich", "description": "Bump done! :thumbsup:\nServer bumped. Check it out on DISBOARD.", "color": 240116}])
}

/// Same success signal, different cosmetics: the predicate ignores everything
/// but the description marker (contract item 3).
fn success_embeds_variant() -> Value {
    json!([{
        "type": "rich",
        "title": "DISBOARD",
        "description": "BUMP DONE! Thanks for bumping this server.",
        "color": 112233,
        "footer": {"text": "disboard.org"},
    }])
}

fn failure_embeds() -> Value {
    json!([{"type": "rich", "description": "Please wait another 42 minutes until the server can be bumped.", "color": 15158332}])
}

fn message_event(
    id: u64,
    author: Value,
    embeds: Value,
    content: &str,
    guild: Option<&str>,
) -> Event {
    let mut message = json!({
        "id": id.to_string(), "channel_id": CHANNEL_ID, "type": 0,
        "author": author, "content": content,
        "timestamp": "2026-09-30T12:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": embeds, "pinned": false, "components": [],
    });
    if let Some(guild) = guild {
        message["guild_id"] = json!(guild);
    }
    let create: MessageCreate =
        serde_json::from_value(message).expect("bump message fixture deserializes");
    Event::MessageCreate(Box::new(create))
}

fn success_bump(id: u64) -> Event {
    message_event(
        id,
        bot_author(DISBOARD_ID),
        success_embeds(),
        "",
        Some(GUILD),
    )
}

fn bump_command(id: u64, token: &str, subcommand: &str) -> Event {
    let interaction: Interaction = serde_json::from_value(json!({
        "id": id.to_string(), "application_id": APP_ID, "type": 2,
        "token": token, "version": 1,
        "guild_id": GUILD, "channel": {"id": CHANNEL_ID, "type": 0, "name": "bump"},
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": [],
        "member": {
            "permissions": "0", "roles": [], "joined_at": null,
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": MEMBER_ID, "username": "tester", "discriminator": "0000", "avatar": null}
        },
        "data": {"id": "5555", "name": "bump", "type": 1,
            "options": [{"name": subcommand, "type": 1}]}
    }))
    .expect("bump interaction fixture deserializes");
    Event::InteractionCreate(Box::new(InteractionCreate(interaction)))
}

/// Creates the tests observe: role-pill pings posted to the bumped channel.
fn bump_pings(mock: &MockRest) -> Vec<RestRequest> {
    let pill = format!("<@&{ROLE_ID}>");
    let channel_suffix = format!("/channels/{CHANNEL_ID}/messages");
    mock.requests()
        .into_iter()
        .filter(|request| {
            request.method == "POST"
                && request.path.ends_with(channel_suffix.as_str())
                && serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body["content"].as_str().map(str::to_owned))
                    .is_some_and(|content| content.contains(pill.as_str()))
        })
        .collect()
}

/// Presence waits return on sighting; absence is proven by exhausting `budget`.
async fn await_pings(mock: &MockRest, want: usize, budget: Duration) -> Vec<RestRequest> {
    let start = tokio::time::Instant::now();
    loop {
        let pings = bump_pings(mock);
        if pings.len() >= want || start.elapsed() >= budget {
            return pings;
        }
        tokio::time::sleep(POLL_TICK).await;
    }
}

/// Concatenated reply text for one interaction token: immediate callbacks plus
/// any follow-up edit, so defer-then-edit and immediate replies are equivalent.
/// Settles briefly after the first sighting so an ack+edit pair reads whole.
async fn await_reply_text(mock: &MockRest, token: &str, budget: Duration) -> Option<String> {
    fn scan(mock: &MockRest, token: &str) -> Vec<String> {
        let mut parts = Vec::new();
        for request in mock.requests() {
            if !request.path.contains(token) {
                continue;
            }
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                continue;
            };
            if request.method == "POST" && request.path.ends_with("/callback") && body["type"] == 4
            {
                if let Some(content) = body["data"]["content"].as_str() {
                    parts.push(content.to_owned());
                }
            }
            if request.method == "PATCH"
                && request
                    .path
                    .replace("%40", "@")
                    .ends_with("/messages/@original")
            {
                if let Some(content) = body["content"].as_str() {
                    parts.push(content.to_owned());
                }
            }
        }
        parts
    }
    let start = tokio::time::Instant::now();
    loop {
        let parts = scan(mock, token);
        if !parts.is_empty() {
            tokio::time::sleep(Duration::from_millis(300)).await;
            return Some(scan(mock, token).join("\n"));
        }
        if start.elapsed() >= budget {
            return None;
        }
        tokio::time::sleep(POLL_TICK).await;
    }
}

async fn await_role_call(
    mock: &MockRest,
    method: &str,
    member: &str,
    role: &str,
    budget: Duration,
) -> Vec<RestRequest> {
    let suffix = format!("/guilds/{GUILD}/members/{member}/roles/{role}");
    let start = tokio::time::Instant::now();
    loop {
        let calls: Vec<RestRequest> = mock
            .requests()
            .into_iter()
            .filter(|request| request.method == method && request.path.ends_with(suffix.as_str()))
            .collect();
        if !calls.is_empty() || start.elapsed() >= budget {
            return calls;
        }
        tokio::time::sleep(POLL_TICK).await;
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    haystack.contains(needle.as_str())
}

fn is_armed_text(reply: &str) -> bool {
    contains_ci(reply, ARMED_WORD) && !contains_ci(reply, IDLE_PHRASE)
}

fn assert_no_error_markers(reply: &str, context: &str) {
    for marker in ERROR_MARKERS {
        assert!(
            !contains_ci(reply, marker),
            "{context} reply must not carry failure language {marker:?}: {reply:?}"
        );
    }
}

fn assert_no_internals(text: &str, context: &str) {
    for marker in INTERNALS {
        assert!(
            !contains_ci(text, marker),
            "{context} must not leak bumper internals {marker:?}: {text:?}"
        );
    }
}

fn assert_armed_reply(reply: Option<String>, context: &str) -> String {
    let reply =
        reply.unwrap_or_else(|| panic!("{context}: expected an armed status reply, got silence"));
    assert!(
        is_armed_text(&reply),
        "{context}: expected an armed status reply, got: {reply:?}"
    );
    assert_no_error_markers(&reply, context);
    assert_no_internals(&reply, context);
    reply
}

fn assert_idle_reply(reply: Option<String>, context: &str) -> String {
    let reply =
        reply.unwrap_or_else(|| panic!("{context}: expected an idle status reply, got silence"));
    assert!(
        contains_ci(&reply, IDLE_PHRASE),
        "{context}: expected the idle phrase {IDLE_PHRASE:?}, got: {reply:?}"
    );
    assert_no_error_markers(&reply, context);
    assert_no_internals(&reply, context);
    reply
}

fn assert_ping_shape(ping: &RestRequest) {
    let body: Value = serde_json::from_slice(&ping.body).expect("ping body is JSON");
    let content = body["content"].as_str().unwrap_or("");
    let pill = format!("<@&{ROLE_ID}>");
    assert!(
        content.contains(pill.as_str()),
        "ping must carry the role pill: {content:?}"
    );
    let role_allowed = body["allowed_mentions"]["roles"]
        .as_array()
        .is_some_and(|roles| roles.iter().any(|role| role.as_str() == Some(ROLE_ID)));
    assert!(
        role_allowed,
        "ping must allowlist the role mention or Discord stays silent: {body:?}"
    );
    assert!(
        !content.contains("@everyone") && !content.contains("@here"),
        "mention-safe ping must not blast the server: {content:?}"
    );
    assert_no_internals(content, "fired ping");
    // The role pill is the only mention pill: no direct user pings ever.
    let role_suffix = format!("&{ROLE_ID}>");
    let mut rest = content;
    while let Some(at) = rest.find("<@") {
        rest = &rest[at + 2..];
        assert!(
            rest.starts_with(role_suffix.as_str()),
            "ping must mention only the bump role: {content:?}"
        );
    }
}

#[test]
fn bump_fixtures_deserialize() {
    let Event::MessageCreate(success) = success_bump(7001) else {
        panic!("success fixture must be a message create");
    };
    assert_eq!(success.0.author.id.get().to_string(), DISBOARD_ID);
    assert!(success.0.author.bot);
    assert_eq!(
        success.0.guild_id.map(|id| id.get().to_string()).as_deref(),
        Some(GUILD)
    );
    let description = success.0.embeds[0].description.as_deref().unwrap_or("");
    assert!(
        contains_ci(description, SUCCESS_MARKER),
        "success fixture must carry the marker: {description:?}"
    );

    let Event::MessageCreate(variant) = message_event(
        7002,
        bot_author(DISBOARD_ID),
        success_embeds_variant(),
        "",
        Some(GUILD),
    ) else {
        panic!("variant fixture must be a message create");
    };
    let description = variant.0.embeds[0].description.as_deref().unwrap_or("");
    assert!(
        contains_ci(description, SUCCESS_MARKER),
        "variant fixture must carry the marker through cosmetics: {description:?}"
    );

    let Event::MessageCreate(failure) = message_event(
        7003,
        bot_author(DISBOARD_ID),
        failure_embeds(),
        "",
        Some(GUILD),
    ) else {
        panic!("failure fixture must be a message create");
    };
    assert!(failure.0.author.bot);
    for embed in &failure.0.embeds {
        assert!(
            !contains_ci(embed.description.as_deref().unwrap_or(""), SUCCESS_MARKER),
            "failure fixture must not carry the marker"
        );
    }

    let Event::MessageCreate(spoof) = message_event(
        7004,
        human_author(),
        success_embeds(),
        "Bump done! :thumbsup: everyone bump now!",
        Some(GUILD),
    ) else {
        panic!("spoof fixture must be a message create");
    };
    assert!(
        !spoof.0.author.bot,
        "absent bot flag deserializes to human (Discord omits it)"
    );

    let Event::MessageCreate(dm) =
        message_event(7005, bot_author(DISBOARD_ID), success_embeds(), "", None)
    else {
        panic!("DM fixture must be a message create");
    };
    assert!(dm.0.guild_id.is_none(), "DM fixture has no guild");

    for (subcommand, id) in [
        ("opt-in", 7101u64),
        ("opt-out", 7102u64),
        ("status", 7103u64),
        ("help", 7104u64),
    ] {
        let Event::InteractionCreate(interaction) =
            bump_command(id, &format!("bump-fixture-{id}"), subcommand)
        else {
            panic!("{subcommand} fixture must be an interaction create");
        };
        let Some(InteractionData::ApplicationCommand(data)) = interaction.0.data.as_ref() else {
            panic!("{subcommand} fixture must carry command data");
        };
        assert_eq!(data.name, "bump");
        assert_eq!(data.options.len(), 1, "one subcommand option");
        assert_eq!(data.options[0].name, subcommand);
        assert!(
            matches!(data.options[0].value, CommandOptionValue::SubCommand(_)),
            "{subcommand} fixture must parse as a subcommand"
        );
    }

    let surrogate: Message = serde_json::from_value(json!({
        "id": "7006", "channel_id": CHANNEL_ID, "type": 0,
        "author": human_author(), "content": "hello bump channel",
        "timestamp": "2026-09-30T12:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "components": [],
        "guild_id": GUILD,
    }))
    .expect("trigger message deserializes");
    assert_eq!(surrogate.channel_id.get().to_string(), CHANNEL_ID);
}

#[tokio::test]
async fn fallback_reports_zero_cooldown_as_control() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let mock = MockRest::with_responder(responder()).await;
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).unwrap();
    let runtime = CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates()),
        GUILD.parse().unwrap(),
        true,
    );
    assert_eq!(
        <CommandRuntime as BumpDefaultCooldownFallback>::bump_default_cooldown_secs(&runtime),
        0,
        "explicit trait call keeps the baseline adapter exercised after shadowing"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn default_cooldown_is_two_hours() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let mock = MockRest::with_responder(responder()).await;
    let executor = ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).unwrap();
    let runtime = CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates()),
        GUILD.parse().unwrap(),
        true,
    );
    assert_eq!(
        runtime.bump_default_cooldown_secs(),
        7200,
        "the pinned default is DISBOARD's 2-hour cooldown"
    );
    mock.shutdown().await;
}

async fn dispatch_message(runtime: &Arc<CommandRuntime>, event: &Event, context: &str) {
    assert!(
        runtime.dispatch_with_verdict(event, Some(FunnelDisposition::Accept)),
        "{context}: message dispatch must be admitted"
    );
}

async fn dispatch_command(runtime: &Arc<CommandRuntime>, event: &Event, context: &str) {
    assert!(
        runtime.dispatch(event),
        "{context}: interaction dispatch must be admitted"
    );
}

fn trigger_message(id: u64) -> Event {
    message_event(
        id,
        human_author(),
        json!([]),
        "hello bump channel, anyone here?",
        Some(GUILD),
    )
}

async fn armed_status(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    id: u64,
    token: &str,
    context: &str,
) -> String {
    dispatch_command(runtime, &bump_command(id, token, "status"), context).await;
    let reply = await_reply_text(mock, token, REPLY_BUDGET).await;
    assert_armed_reply(reply, context)
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn success_embed_arms_exactly_one_pending_reminder() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_message(&runtime, &success_bump(8001), "success bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    armed_status(
        &runtime,
        &mock,
        8101,
        "bump-status-arm-1",
        "armed after success",
    )
    .await;

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn expiry_fires_exactly_one_role_ping() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_message(&runtime, &success_bump(8002), "success bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    armed_status(
        &runtime,
        &mock,
        8102,
        "bump-status-arm-2",
        "armed before expiry",
    )
    .await;
    tokio::time::sleep(POST_EXPIRY_SLEEP).await;

    dispatch_message(&runtime, &trigger_message(8202), "expiry trigger").await;
    let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
    assert_eq!(
        pings.len(),
        1,
        "expiry fires exactly one role ping, saw {}",
        pings.len()
    );
    assert_ping_shape(&pings[0]);

    // A second sweep never re-fires the consumed reminder.
    dispatch_message(&runtime, &trigger_message(8203), "second trigger").await;
    tokio::time::sleep(ARM_SETTLE).await;
    assert_eq!(
        bump_pings(&mock).len(),
        1,
        "the fired reminder stays consumed"
    );

    // Fired state reads idle again.
    dispatch_command(
        &runtime,
        &bump_command(8103, "bump-status-fired-2", "status"),
        "fired status",
    )
    .await;
    let reply = await_reply_text(&mock, "bump-status-fired-2", REPLY_BUDGET).await;
    assert_idle_reply(reply, "fired status");

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn fresh_bump_replaces_pending_instead_of_stacking() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_message(&runtime, &success_bump(8003), "first bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    armed_status(
        &runtime,
        &mock,
        8104,
        "bump-status-arm-3a",
        "armed after first",
    )
    .await;

    // Cosmetically different success still recognizes and replaces.
    let second = message_event(
        8004,
        bot_author(DISBOARD_ID),
        success_embeds_variant(),
        "",
        Some(GUILD),
    );
    dispatch_message(&runtime, &second, "second bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    armed_status(
        &runtime,
        &mock,
        8105,
        "bump-status-arm-3b",
        "armed after second",
    )
    .await;
    tokio::time::sleep(POST_EXPIRY_SLEEP).await;

    dispatch_message(&runtime, &trigger_message(8204), "expiry trigger").await;
    let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
    assert_eq!(
        pings.len(),
        1,
        "a fresh bump replaces the pending reminder instead of stacking, saw {}",
        pings.len()
    );
    assert_ping_shape(&pings[0]);

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn pending_reminder_survives_runtime_restart_and_fires_once() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;

    let runtime = test_runtime(db.pool.clone(), &mock).await;
    dispatch_message(&runtime, &success_bump(8005), "success bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    armed_status(
        &runtime,
        &mock,
        8106,
        "bump-status-arm-4a",
        "armed before restart",
    )
    .await;
    drop(runtime);

    // A fresh runtime over the same database reloads the pending reminder.
    let runtime = test_runtime(db.pool.clone(), &mock).await;
    armed_status(
        &runtime,
        &mock,
        8107,
        "bump-status-arm-4b",
        "armed after restart",
    )
    .await;
    tokio::time::sleep(POST_EXPIRY_SLEEP).await;

    dispatch_message(&runtime, &trigger_message(8205), "expiry trigger").await;
    let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
    assert_eq!(
        pings.len(),
        1,
        "restarted runtime fires the reloaded reminder exactly once, saw {}",
        pings.len()
    );
    assert_ping_shape(&pings[0]);

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn opt_in_adds_member_to_bump_role_with_confirmation() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_command(
        &runtime,
        &bump_command(8108, "bump-opt-in-5", "opt-in"),
        "opt-in",
    )
    .await;
    let calls = await_role_call(&mock, "PUT", MEMBER_ID, ROLE_ID, ROLE_BUDGET).await;
    assert_eq!(
        calls.len(),
        1,
        "opt-in adds the caller to the bump role exactly once, saw {}",
        calls.len()
    );
    let reply = await_reply_text(&mock, "bump-opt-in-5", REPLY_BUDGET).await;
    let reply =
        reply.unwrap_or_else(|| panic!("opt-in: expected a confirmation reply, got silence"));
    assert!(
        contains_ci(&reply, "opt"),
        "opt-in confirmation must name the opt-in: {reply:?}"
    );
    assert_no_error_markers(&reply, "opt-in");
    assert_no_internals(&reply, "opt-in");

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn opt_out_removes_member_from_bump_role_with_confirmation() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_command(
        &runtime,
        &bump_command(8109, "bump-opt-out-6", "opt-out"),
        "opt-out",
    )
    .await;
    let calls = await_role_call(&mock, "DELETE", MEMBER_ID, ROLE_ID, ROLE_BUDGET).await;
    assert_eq!(
        calls.len(),
        1,
        "opt-out removes the caller from the bump role exactly once, saw {}",
        calls.len()
    );
    let reply = await_reply_text(&mock, "bump-opt-out-6", REPLY_BUDGET).await;
    let reply =
        reply.unwrap_or_else(|| panic!("opt-out: expected a confirmation reply, got silence"));
    assert!(
        contains_ci(&reply, "opt"),
        "opt-out confirmation must name the opt-out: {reply:?}"
    );
    assert_no_error_markers(&reply, "opt-out");
    assert_no_internals(&reply, "opt-out");

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn help_lists_bump_commands_concisely() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_command(&runtime, &bump_command(8110, "bump-help-7", "help"), "help").await;
    let reply = await_reply_text(&mock, "bump-help-7", REPLY_BUDGET).await;
    let reply = reply.unwrap_or_else(|| panic!("help: expected a help reply, got silence"));
    assert!(
        contains_ci(&reply, "bump"),
        "help must describe the bump commands: {reply:?}"
    );
    assert!(
        reply.len() <= 1000,
        "concise help fits one message comfortably, got {} chars",
        reply.len()
    );
    assert_no_error_markers(&reply, "help");
    assert_no_internals(&reply, "help");

    mock.shutdown().await;
    db.close().await;
}

async fn assert_never_arms(
    runtime: &Arc<CommandRuntime>,
    mock: &MockRest,
    event: Event,
    status_id: u64,
    token: &str,
    context: &str,
) {
    dispatch_message(runtime, &event, context).await;
    tokio::time::sleep(ARM_SETTLE).await;
    dispatch_command(runtime, &bump_command(status_id, token, "status"), context).await;
    let reply = await_reply_text(mock, token, REPLY_BUDGET).await;
    if let Some(reply) = reply {
        assert!(
            !is_armed_text(&reply),
            "{context}: must never read armed, got: {reply:?}"
        );
    }
    tokio::time::sleep(POST_EXPIRY_SLEEP).await;
    dispatch_message(runtime, &trigger_message(status_id + 1000), context).await;
    let pings = await_pings(mock, 1, ABSENCE_BUDGET).await;
    assert!(
        pings.is_empty(),
        "{context}: must never fire a ping, saw {}",
        pings.len()
    );
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn non_success_embeds_never_arm() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    // A bumper-bot failure embed carries no success marker.
    let failure = message_event(
        8006,
        bot_author(DISBOARD_ID),
        failure_embeds(),
        "",
        Some(GUILD),
    );
    assert_never_arms(
        &runtime,
        &mock,
        failure,
        8111,
        "bump-status-non-success-9",
        "failure embed",
    )
    .await;

    // Content-only success text without the embed signal never arms either.
    let content_only = message_event(
        8007,
        bot_author(DISBOARD_ID),
        json!([]),
        "Bump done! :thumbsup:",
        Some(GUILD),
    );
    assert_never_arms(
        &runtime,
        &mock,
        content_only,
        8112,
        "bump-status-content-only-9",
        "content-only success text",
    )
    .await;

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn non_allowlisted_bots_never_arm() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    let foreign = message_event(
        8008,
        bot_author(FOREIGN_BOT_ID),
        success_embeds(),
        "",
        Some(GUILD),
    );
    assert_never_arms(
        &runtime,
        &mock,
        foreign,
        8113,
        "bump-status-foreign-10",
        "non-allowlisted bot",
    )
    .await;

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn spoofed_user_embeds_never_arm() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    // A human reposting the success marker (content and embed): the author
    // allowlist refuses it, not the content match.
    let spoof = message_event(
        8009,
        human_author(),
        success_embeds(),
        "Bump done! :thumbsup: everyone bump now!",
        Some(GUILD),
    );
    assert_never_arms(
        &runtime,
        &mock,
        spoof,
        8114,
        "bump-status-spoof-11",
        "spoofed user embed",
    )
    .await;

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn direct_messages_never_arm() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    let dm = message_event(8010, bot_author(DISBOARD_ID), success_embeds(), "", None);
    dispatch_message(&runtime, &dm, "DM success embed").await;
    tokio::time::sleep(ARM_SETTLE + POST_EXPIRY_SLEEP).await;
    dispatch_message(&runtime, &trigger_message(8210), "DM trigger").await;
    let pings = await_pings(&mock, 1, ABSENCE_BUDGET).await;
    assert!(
        pings.is_empty(),
        "guildless success embeds must never arm, saw {}",
        pings.len()
    );

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn opted_out_members_are_never_directly_mentioned() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_command(
        &runtime,
        &bump_command(8115, "bump-opt-in-13", "opt-in"),
        "opt-in",
    )
    .await;
    await_role_call(&mock, "PUT", MEMBER_ID, ROLE_ID, ROLE_BUDGET).await;
    dispatch_command(
        &runtime,
        &bump_command(8116, "bump-opt-out-13", "opt-out"),
        "opt-out",
    )
    .await;
    let removals = await_role_call(&mock, "DELETE", MEMBER_ID, ROLE_ID, ROLE_BUDGET).await;
    if removals.is_empty() {
        // Baseline: no bump code, so no ping can exist either; the per-ping
        // asserts below hold vacuously.
        mock.shutdown().await;
        db.close().await;
        return;
    }

    dispatch_message(&runtime, &success_bump(8011), "success bump").await;
    tokio::time::sleep(ARM_SETTLE + POST_EXPIRY_SLEEP).await;
    dispatch_message(&runtime, &trigger_message(8211), "expiry trigger").await;
    let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
    for ping in &pings {
        assert_ping_shape(ping);
    }

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn bump_replies_leak_no_bumper_internals() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(SHORT_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    // Every user-visible surface in one round trip; each text must be clean.
    for (id, token, subcommand) in [
        (8117u64, "bump-opt-in-14", "opt-in"),
        (8118u64, "bump-status-14", "status"),
        (8119u64, "bump-help-14", "help"),
    ] {
        dispatch_command(&runtime, &bump_command(id, token, subcommand), subcommand).await;
        if let Some(reply) = await_reply_text(&mock, token, REPLY_BUDGET).await {
            assert_no_internals(&reply, subcommand);
        }
    }
    dispatch_message(&runtime, &success_bump(8012), "success bump").await;
    tokio::time::sleep(ARM_SETTLE + POST_EXPIRY_SLEEP).await;
    dispatch_message(&runtime, &trigger_message(8212), "expiry trigger").await;
    let pings = await_pings(&mock, 1, FIRE_BUDGET).await;
    for ping in &pings {
        let body: Value = serde_json::from_slice(&ping.body).expect("ping body is JSON");
        assert_no_internals(body["content"].as_str().unwrap_or(""), "fired ping");
    }

    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn cooldown_does_not_fire_early() {
    let _serial = BUMP_ENV_LOCK.lock().await;
    let _env = BumpEnv::set(LONG_COOLDOWN_SECS);
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(responder()).await;
    let runtime = test_runtime(db.pool.clone(), &mock).await;

    dispatch_message(&runtime, &success_bump(8013), "success bump").await;
    tokio::time::sleep(ARM_SETTLE).await;
    dispatch_message(&runtime, &trigger_message(8213), "early trigger").await;
    let pings = await_pings(&mock, 1, ABSENCE_BUDGET).await;
    assert!(
        pings.is_empty(),
        "a one-hour cooldown must not fire within seconds, saw {}",
        pings.len()
    );

    mock.shutdown().await;
    db.close().await;
}
